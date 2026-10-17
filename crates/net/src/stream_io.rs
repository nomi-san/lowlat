//! Optional TCP/TLS relay IO. One poller and one session thread; no runtime.
//! Only the client-to-TURN leg is a stream. Peer allocations remain UDP.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::turn_stream::{Output, Progress};
use lowlat_core::turn::stream::Decoder;
use mio::{Events, Interest, Poll, Token, Waker, net::TcpStream};
use rustls::{ClientConfig, ClientConnection, RootCertStore, pki_types::ServerName};

const SOCKET: Token = Token(0);
const WAKE: Token = Token(1);
const SETUP: Duration = Duration::from_secs(10);
const PASSES: usize = 64;
const TLS_BUFFER: usize = 32 * 1024;
const WRITE_STALL: Duration = Duration::from_secs(5);

/// Explicit trust and server identity. No permissive verifier is exposed.
#[derive(Debug, Clone)]
pub struct Tls {
    name: ServerName<'static>,
    config: Arc<ClientConfig>,
}

impl Tls {
    /// Public roots plus explicitly installed OS roots, with hostname checking.
    pub fn system(name: &str) -> io::Result<Self> {
        let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        Self::with_roots(name, roots)
    }

    /// Explicit private CA trust, also useful for hermetic TLS fixtures.
    pub fn with_roots(name: &str, roots: RootCertStore) -> io::Result<Self> {
        let name = ServerName::try_from(name.to_owned()).map_err(io::Error::other)?;
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(io::Error::other)?
                .with_root_certificates(roots)
                .with_no_client_auth();
        Ok(Self {
            name,
            config: Arc::new(config),
        })
    }
}

#[derive(Debug, Clone)]
pub enum Security {
    Tcp,
    Tls(Tls),
}

#[derive(Debug)]
pub(crate) struct Notify {
    armed: AtomicBool,
    waker: Waker,
}

impl Notify {
    pub(crate) fn notify(&self) -> io::Result<()> {
        // Do not skip wake() on an armed flag: the poller may have consumed its
        // edge during setup. Taking the flag still precedes application work.
        self.armed.store(true, Ordering::Release);
        self.waker.wake()
    }
}

#[derive(Debug)]
pub(crate) struct StreamIo {
    socket: TcpStream,
    server: SocketAddr,
    local: Option<std::net::IpAddr>,
    poll: Poll,
    events: Events,
    notify: Arc<Notify>,
    tls: Option<ClientConnection>,
    connecting: bool,
    readable: bool,
    writable: bool,
    wants_write: bool,
    continuation: bool,
    plaintext: usize,
    decoder: Decoder<Box<[u8]>>,
    input: Box<[u8]>,
    start: usize,
    end: usize,
    received: usize,
    output: Output,
    pending_at: Option<Instant>,
    write_progress: Instant,
    terminal: Option<io::Error>,
}

impl StreamIo {
    pub(crate) fn start(server: SocketAddr, security: Security) -> io::Result<Self> {
        let poll = Poll::new()?;
        let notify = Arc::new(Notify {
            armed: AtomicBool::new(false),
            waker: Waker::new(poll.registry(), WAKE)?,
        });
        let mut socket = TcpStream::connect(server)?;
        poll.registry()
            .register(&mut socket, SOCKET, Interest::READABLE | Interest::WRITABLE)?;
        let tls = match security {
            Security::Tcp => None,
            Security::Tls(settings) => {
                let mut connection = ClientConnection::new(settings.config, settings.name)
                    .map_err(io::Error::other)?;
                connection.set_buffer_limit(Some(TLS_BUFFER));
                Some(connection)
            }
        };
        Ok(Self {
            socket,
            server,
            local: None,
            poll,
            notify,
            tls,
            events: Events::with_capacity(8),
            connecting: true,
            readable: false,
            writable: false,
            wants_write: true,
            continuation: false,
            plaintext: 0,
            decoder: Decoder::new(vec![0; crate::RECV_SLOT + 3].into_boxed_slice())
                .map_err(core_error)?,
            input: vec![0; 8192].into_boxed_slice(),
            start: 0,
            end: 0,
            received: 0,
            output: Output::new(crate::RECV_SLOT + 3)?,
            pending_at: None,
            write_progress: Instant::now(),
            terminal: None,
        })
    }

    pub(crate) fn notify(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }
    pub(crate) fn take_wake(&self) -> bool {
        self.notify.armed.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn prepare(&mut self, stopping: impl Fn() -> bool) -> io::Result<()> {
        let deadline = Instant::now() + SETUP;
        loop {
            if stopping() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if !self.connecting {
                self.flush()?;
                if self.readable && self.tls.as_ref().is_some_and(|tls| tls.is_handshaking()) {
                    self.read_tls()?;
                    self.flush()?;
                }
                if self
                    .tls
                    .as_ref()
                    .is_none_or(|tls| !tls.is_handshaking() && !tls.wants_write())
                {
                    self.continuation = true;
                    return Ok(());
                }
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(io::ErrorKind::TimedOut)?;
            self.poll_once(if self.readable || self.continuation {
                Duration::ZERO
            } else {
                remaining
            })?;
            self.continuation = false;
        }
    }

    fn poll_once(&mut self, timeout: Duration) -> io::Result<()> {
        self.interest()?;
        match self.poll.poll(&mut self.events, Some(timeout)) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
            result => result?,
        }
        for event in &self.events {
            if event.token() == SOCKET {
                if let Some(error) = self.socket.take_error()? {
                    return Err(error);
                }
                if self.connecting && (event.is_writable() || event.is_readable()) {
                    // Readiness may be spurious while a nonblocking connect
                    // is pending. Do not start TLS or treat it as a failure.
                    match self.socket.peer_addr() {
                        Ok(_) => {
                            self.socket.set_nodelay(true)?;
                            self.local = Some(self.socket.local_addr()?.ip());
                            self.connecting = false;
                        }
                        Err(error) if connect_pending(&error) => continue,
                        Err(error) => return Err(error),
                    }
                }
                self.readable |= event.is_readable() || event.is_read_closed();
                self.writable |= event.is_writable() || event.is_write_closed();
                if event.is_error() {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
            }
        }
        Ok(())
    }

    fn interest(&mut self) -> io::Result<()> {
        let write = self.connecting || self.pending();
        if write != self.wants_write {
            let interest = if write {
                Interest::READABLE | Interest::WRITABLE
            } else {
                Interest::READABLE
            };
            self.poll
                .registry()
                .reregister(&mut self.socket, SOCKET, interest)?;
            self.wants_write = write;
        }
        Ok(())
    }

    pub(crate) fn wait(&mut self, timeout_ms: f64) -> io::Result<crate::shell::Ready> {
        if let Some(error) = self.terminal.take() {
            return Err(error);
        }
        let now = Instant::now();
        self.track_output(now);
        let timeout = self.output_timeout(Duration::from_secs_f64(timeout_ms / 1000.0), now)?;
        self.received = 0;
        let immediate = self.continuation
            || self.start < self.end
            || self.plaintext > 0
            || self.readable
            || (self.writable && self.pending());
        self.continuation = false;
        let timeout = if immediate { Duration::ZERO } else { timeout };
        self.poll_once(timeout)?;
        self.output_timeout(Duration::ZERO, Instant::now())?;
        self.flush()?;
        Ok(crate::shell::Ready {
            socket: self.readable || self.plaintext > 0 || self.start < self.end || immediate,
            wake: self.notify.armed.load(Ordering::Acquire),
        })
    }

    // Track one continuous unit of carrier work, including TLS ciphertext.
    // TLS accepting plaintext is not socket progress and must not reset age.
    fn track_output(&mut self, now: Instant) {
        if !self.pending() {
            self.pending_at = None;
        } else if self.pending_at.is_none() {
            self.pending_at = Some(now);
            self.write_progress = now;
        }
    }

    fn output_timeout(&self, requested: Duration, now: Instant) -> io::Result<Duration> {
        if !self.pending() {
            return Ok(requested);
        }
        let mut timeout = requested;
        for since in [self.pending_at, Some(self.write_progress)]
            .into_iter()
            .flatten()
        {
            let remaining = (since + WRITE_STALL)
                .checked_duration_since(now)
                .filter(|remaining| !remaining.is_zero())
                .ok_or(io::ErrorKind::TimedOut)?;
            timeout = timeout.min(remaining);
        }
        Ok(timeout)
    }

    fn read_tls(&mut self) -> io::Result<bool> {
        let Some(tls) = self.tls.as_mut() else {
            return Ok(false);
        };
        if !self.readable {
            return Ok(false);
        }
        match tls.read_tls(&mut self.socket) {
            Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) => {
                let state = tls
                    .process_new_packets()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                self.plaintext = state.plaintext_bytes_to_read();
                self.track_output(Instant::now());
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                self.readable = false;
                Ok(false)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                self.continuation = true;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn drain(&mut self) -> io::Result<usize> {
        if self.terminal.is_some() {
            return Ok(0);
        }
        match self.drain_inner() {
            // Let the driver see authenticated messages already received in
            // this pass (including a peer departure) before reporting EOF.
            Err(error) if self.received != 0 => {
                self.terminal = Some(error);
                self.continuation = true;
                Ok(0)
            }
            result => result,
        }
    }

    fn drain_inner(&mut self) -> io::Result<usize> {
        if self.received >= PASSES {
            self.continuation = true;
            return Ok(0);
        }
        for _ in 0..PASSES {
            if self.start < self.end {
                let input = self
                    .input
                    .get(self.start..self.end)
                    .ok_or(io::ErrorKind::InvalidData)?;
                let (used, frame) = self.decoder.push(input).map_err(core_error)?;
                self.start += used;
                if frame.is_some() {
                    self.received += 1;
                    return Ok(1);
                }
                if self.start < self.end {
                    continue;
                }
            }
            self.start = 0;
            self.end = 0;
            let result = match &mut self.tls {
                Some(tls) => tls.reader().read(&mut self.input),
                None if self.readable => self.socket.read(&mut self.input),
                None => return Ok(0),
            };
            match result {
                Ok(0) => {
                    self.decoder.finish().map_err(core_error)?;
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                Ok(count) => {
                    self.end = count;
                    self.plaintext = self.plaintext.saturating_sub(count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if self.tls.is_none() {
                        self.readable = false;
                        return Ok(0);
                    }
                    if !self.read_tls()? {
                        return Ok(0);
                    }
                    self.flush()?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.continuation = true;
        Ok(0)
    }

    pub(crate) fn frame(&self) -> Option<(SocketAddr, Option<std::net::IpAddr>, &[u8])> {
        self.decoder
            .last_frame()
            .map(|message| (self.server, self.local, message))
    }

    pub(crate) fn pending(&self) -> bool {
        self.output.pending() != 0 || self.tls.as_ref().is_some_and(|tls| tls.wants_write())
    }
    pub(crate) fn queue(&mut self, message: &[u8], to: SocketAddr) -> io::Result<()> {
        if to != self.server {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        // The shell must not consume another core datagram while an earlier
        // frame is retained by either the framer or the TLS record writer.
        if self.pending() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.output.queue(message)?;
        let now = Instant::now();
        self.pending_at = Some(now);
        self.write_progress = now;
        self.flush()
    }
    pub(crate) fn continue_soon(&mut self) {
        self.continuation = true;
    }

    pub(crate) fn finish_output(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        while self.pending() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(io::ErrorKind::TimedOut)?;
            self.wait(remaining.min(Duration::from_millis(50)).as_secs_f64() * 1000.0)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.track_output(Instant::now());
        if !self.writable || self.connecting {
            return Ok(());
        }
        if let Some(tls) = &mut self.tls {
            let progress = self.output.write_ready(&mut PlainWriter(tls))?;
            if progress == Progress::Yielded {
                self.continuation = true;
            }
            for _ in 0..PASSES {
                if !tls.wants_write() {
                    if self.output.pending() == 0 {
                        self.pending_at = None;
                    }
                    self.continuation |= self.output.pending() != 0;
                    return Ok(());
                }
                match tls.write_tls(&mut self.socket) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(_) => {
                        self.write_progress = Instant::now();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.writable = false;
                        return Ok(());
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            self.continuation = true;
        } else {
            let before = self.output.pending();
            match self.output.write_ready(&mut self.socket)? {
                Progress::Blocked => self.writable = false,
                Progress::Yielded => self.continuation = true,
                Progress::Complete => {}
            }
            if self.output.pending() < before {
                self.write_progress = Instant::now();
            }
            if self.output.pending() == 0 {
                self.pending_at = None;
            }
        }
        Ok(())
    }
}

fn connect_pending(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::NotConnected | io::ErrorKind::WouldBlock
    ) {
        return true;
    }
    #[cfg(target_os = "linux")]
    if error.raw_os_error() == Some(libc::EINPROGRESS) {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(windows_sys::Win32::Networking::WinSock::WSAEINPROGRESS) {
        return true;
    }
    false
}

struct PlainWriter<'a>(&'a mut ClientConnection);
fn core_error(error: lowlat_core::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.as_str())
}
impl Write for PlainWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.0.writer().write(bytes)?;
        if count == 0 && !bytes.is_empty() {
            Err(io::ErrorKind::WouldBlock.into())
        } else {
            Ok(count)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stream_pending_nonblocking_connect_is_not_a_transport_failure() {
        use super::connect_pending;
        use std::io::{Error, ErrorKind};
        assert!(connect_pending(&Error::from(ErrorKind::NotConnected)));
        assert!(connect_pending(&Error::from(ErrorKind::WouldBlock)));
        assert!(!connect_pending(&Error::from(ErrorKind::ConnectionRefused)));
        assert!(!connect_pending(&Error::from(ErrorKind::PermissionDenied)));
        #[cfg(target_os = "linux")]
        assert!(connect_pending(&Error::from_raw_os_error(
            libc::EINPROGRESS
        )));
        #[cfg(windows)]
        assert!(connect_pending(&Error::from_raw_os_error(
            windows_sys::Win32::Networking::WinSock::WSAEINPROGRESS
        )));
    }
    use super::*;
    use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
    use std::net::{TcpListener, TcpStream as Blocking};

    const WIRE: &[u8] = &[0x40, 0, 0, 3, 1, 2, 3, 0];
    const MESSAGE: &[u8] = &[0x40, 0, 0, 3, 1, 2, 3];

    fn fixture(expired: bool) -> (Arc<ServerConfig>, RootCertStore) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["relay.test".into()]).unwrap();
        if expired {
            params.not_before = rcgen::date_time_ymd(2019, 1, 1);
            params.not_after = rcgen::date_time_ymd(2020, 1, 1);
        }
        let cert = params.self_signed(&key).unwrap().der().clone();
        let private = PrivatePkcs8KeyDer::from(key.serialize_der());
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], private.into())
                .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();
        (Arc::new(config), roots)
    }

    fn bounded(socket: &Blocking) {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket.set_nodelay(true).unwrap();
    }

    // Complete real TLS in memory, without relying on kernel send-buffer sizes
    // to reproduce a blocked ciphertext writer. The TCP socket remains live.
    fn tls_pair() -> (StreamIo, ServerConnection, Blocking) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut io = StreamIo::start(listener.local_addr().unwrap(), Security::Tcp).unwrap();
        io.prepare(|| false).unwrap();
        let (peer, _) = listener.accept().unwrap();
        let (config, roots) = fixture(false);
        let settings = Tls::with_roots("relay.test", roots).unwrap();
        let mut client = ClientConnection::new(settings.config, settings.name).unwrap();
        client.set_buffer_limit(Some(TLS_BUFFER));
        let mut server = ServerConnection::new(config).unwrap();
        for _ in 0..16 {
            let mut wire = Vec::new();
            client.write_tls(&mut wire).unwrap();
            if !wire.is_empty() {
                server.read_tls(&mut std::io::Cursor::new(wire)).unwrap();
                server.process_new_packets().unwrap();
            }
            let mut wire = Vec::new();
            server.write_tls(&mut wire).unwrap();
            if !wire.is_empty() {
                client.read_tls(&mut std::io::Cursor::new(wire)).unwrap();
                client.process_new_packets().unwrap();
            }
            if !client.is_handshaking()
                && !server.is_handshaking()
                && !client.wants_write()
                && !server.wants_write()
            {
                io.tls = Some(client);
                io.writable = false;
                return (io, server, peer);
            }
        }
        panic!("in-memory TLS handshake made no progress");
    }

    fn stage_tls_frame(io: &mut StreamIo) {
        io.queue(MESSAGE, io.server).unwrap();
        // This is the boundary after rustls accepts plaintext but before its
        // ciphertext reaches the socket. No newer core output may pass it.
        assert_eq!(
            io.output
                .write_ready(&mut PlainWriter(io.tls.as_mut().unwrap()))
                .unwrap(),
            Progress::Complete
        );
        assert_eq!(io.output.pending(), 0);
        assert!(io.tls.as_ref().unwrap().wants_write());
    }

    #[test]
    fn stream_tls_ciphertext_backpressures_new_frames_until_fully_written() {
        let (mut io, mut server, _peer) = tls_pair();
        stage_tls_frame(&mut io);
        let started = io.pending_at;
        let last_progress = io.write_progress;
        for _ in 0..100 {
            assert!(io.pending());
            assert_eq!(
                io.queue(MESSAGE, io.server).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(io.pending_at, started);
            assert_eq!(io.write_progress, last_progress);
        }
        // The blocked period introduced neither duplicate frames nor drops.
        let mut wire = Vec::new();
        io.tls.as_mut().unwrap().write_tls(&mut wire).unwrap();
        server.read_tls(&mut std::io::Cursor::new(wire)).unwrap();
        assert_eq!(
            server
                .process_new_packets()
                .unwrap()
                .plaintext_bytes_to_read(),
            WIRE.len()
        );
        let mut plain = [0; 8];
        server.reader().read_exact(&mut plain).unwrap();
        assert_eq!(plain, WIRE);
        io.flush().unwrap();
        assert!(!io.pending());
        assert!(io.pending_at.is_none());
        io.queue(MESSAGE, io.server).unwrap();
    }

    #[test]
    fn stream_tls_accepting_plaintext_and_partial_wire_progress_do_not_reset_age() {
        let (mut io, _server, _peer) = tls_pair();
        stage_tls_frame(&mut io);
        let now = Instant::now();
        let started = now - WRITE_STALL;
        io.pending_at = Some(started);
        // Even physical progress must not keep an old frame alive forever.
        let tls = io.tls.as_mut().unwrap();
        let mut prefix = [0; 1];
        assert_eq!(tls.write_tls(&mut prefix.as_mut_slice()).unwrap(), 1);
        assert!(tls.wants_write());
        io.write_progress = now;
        io.flush().unwrap();
        assert_eq!(io.pending_at, Some(started));
        assert_eq!(io.wait(0.0).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(io.pending());
    }

    #[test]
    fn stream_tls_pending_deadlines_bound_long_polls_without_sleeping() {
        let (mut io, _server, _peer) = tls_pair();
        stage_tls_frame(&mut io);
        let now = Instant::now();
        io.pending_at = Some(now - WRITE_STALL + Duration::from_millis(10));
        io.write_progress = now;
        assert_eq!(
            io.output_timeout(Duration::from_secs(60), now).unwrap(),
            Duration::from_millis(10)
        );
        io.write_progress = now - WRITE_STALL + Duration::from_millis(5);
        assert_eq!(
            io.output_timeout(Duration::from_secs(60), now).unwrap(),
            Duration::from_millis(5)
        );
        assert_eq!(
            io.output_timeout(Duration::ZERO, now + Duration::from_millis(5))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn stream_tls_control_output_after_idle_gets_its_own_deadline() {
        let (mut io, _server, _peer) = tls_pair();
        let now = Instant::now();
        io.write_progress = now - WRITE_STALL;
        io.tls.as_mut().unwrap().refresh_traffic_keys().unwrap();
        assert!(io.pending());
        io.track_output(now);
        assert_eq!(io.pending_at, Some(now));
        assert_eq!(io.write_progress, now);
        assert_eq!(
            io.output_timeout(Duration::from_secs(60), now).unwrap(),
            WRITE_STALL
        );
    }

    fn echo(socket: &mut (impl Read + Write), count: usize) {
        let mut input = [0; 8];
        socket.read_exact(&mut input).unwrap();
        assert_eq!(input, WIRE);
        for _ in 0..count {
            socket.write_all(WIRE).unwrap();
        }
        socket.flush().unwrap();
        // Stay open until the client's last frame is consumed, avoiding a
        // test that relies on TCP FIN racing its final plaintext read.
        socket.read_exact(&mut input).unwrap();
        assert_eq!(input, WIRE);
    }

    fn roundtrip(tls: bool, count: usize) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (server, roots) = fixture(false);
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            bounded(&socket);
            if tls {
                let connection = ServerConnection::new(server).unwrap();
                echo(&mut StreamOwned::new(connection, socket), count);
            } else {
                echo(&mut socket, count);
            }
        });
        let security = if tls {
            Security::Tls(Tls::with_roots("relay.test", roots).unwrap())
        } else {
            Security::Tcp
        };
        let mut io = StreamIo::start(address, security).unwrap();
        io.prepare(|| false).unwrap();
        assert!(!io.connecting);
        assert!(io.socket.nodelay().unwrap());
        io.queue(MESSAGE, address).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received = 0;
        while received < count {
            assert!(Instant::now() < deadline, "stream stopped making progress");
            let _ = io.wait(20.0).unwrap();
            while io.drain().unwrap() != 0 {
                let (from, local, message) = io.frame().unwrap();
                assert_eq!(from, address);
                assert!(local.is_some());
                assert_eq!(message, MESSAGE);
                received += 1;
            }
        }
        io.queue(MESSAGE, address).unwrap();
        while io.output.pending() != 0 || io.tls.as_ref().is_some_and(|tls| tls.wants_write()) {
            assert!(Instant::now() < deadline);
            let _ = io.wait(20.0).unwrap();
        }
        peer.join().unwrap();
    }

    #[test]
    fn stream_tcp_burst_crosses_fairness_boundaries() {
        roundtrip(false, 130);
    }

    #[test]
    fn stream_output_age_is_bounded_without_discarding_a_partial_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut io = StreamIo::start(listener.local_addr().unwrap(), Security::Tcp).unwrap();
        io.prepare(|| false).unwrap();
        io.output.queue(MESSAGE).unwrap();
        io.pending_at = Some(Instant::now() - WRITE_STALL);
        assert_eq!(io.wait(0.0).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(io.output.pending(), WIRE.len());
    }

    #[test]
    fn stream_eof_after_a_frame_is_deferred_until_the_driver_can_process_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            bounded(&socket);
            socket.write_all(WIRE).unwrap();
        });
        let mut io = StreamIo::start(address, Security::Tcp).unwrap();
        io.prepare(|| false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(Instant::now() < deadline);
            io.wait(20.0).unwrap();
            if io.drain().unwrap() != 0 {
                break;
            }
        }
        assert_eq!(io.frame().unwrap().2, MESSAGE);
        while io.terminal.is_none() {
            assert!(Instant::now() < deadline);
            assert_eq!(io.drain().unwrap(), 0);
            // EOF may arrive after the data's readiness edge. Wait for it,
            // retaining this pass's received count so the driver has not yet
            // consumed its frame. Polling drain alone loses that new edge.
            if io.terminal.is_none() {
                io.poll_once(Duration::from_millis(20)).unwrap();
            }
        }
        assert_eq!(
            io.wait(0.0).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        peer.join().unwrap();
    }

    #[test]
    fn stream_tls_verified_burst_crosses_fairness_boundaries() {
        roundtrip(true, 130);
    }

    #[test]
    fn stream_tls_rejects_untrusted_wrong_name_and_expired_servers() {
        for kind in 0..3 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let (server, roots) = fixture(kind == 2);
            let peer = std::thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                bounded(&socket);
                let mut stream = StreamOwned::new(ServerConnection::new(server).unwrap(), socket);
                let mut input = [0];
                assert!(stream.read_exact(&mut input).is_err());
            });
            let roots = if kind == 0 {
                RootCertStore::empty()
            } else {
                roots
            };
            let name = if kind == 1 {
                "different.test"
            } else {
                "relay.test"
            };
            let mut io = StreamIo::start(
                address,
                Security::Tls(Tls::with_roots(name, roots).unwrap()),
            )
            .unwrap();
            assert!(io.prepare(|| false).is_err());
            drop(io);
            peer.join().unwrap();
        }
    }

    #[test]
    fn stream_handshake_teardown_wakes_without_waiting_for_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (_socket, _) = listener.accept().unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        let (_, roots) = fixture(false);
        let mut io = StreamIo::start(
            address,
            Security::Tls(Tls::with_roots("relay.test", roots).unwrap()),
        )
        .unwrap();
        let notify = io.notify();
        let stopping = Arc::new(AtomicBool::new(false));
        let theirs = Arc::clone(&stopping);
        let thread = std::thread::spawn(move || io.prepare(|| theirs.load(Ordering::Acquire)));
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let started = Instant::now();
        stopping.store(true, Ordering::Release);
        notify.notify().unwrap();
        assert_eq!(
            thread.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        release_tx.send(()).unwrap();
        peer.join().unwrap();
    }
}
