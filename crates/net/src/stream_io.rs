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
        socket.set_nodelay(true)?;
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
                    self.socket.peer_addr()?;
                    self.local = Some(self.socket.local_addr()?.ip());
                    self.connecting = false;
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
        let write = self.connecting
            || self.output.pending() != 0
            || self.tls.as_ref().is_some_and(|tls| tls.wants_write());
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
        if self
            .pending_at
            .is_some_and(|at| at.elapsed() >= WRITE_STALL)
            || ((self.output.pending() != 0
                || self.tls.as_ref().is_some_and(|tls| tls.wants_write()))
                && self.write_progress.elapsed() >= WRITE_STALL)
        {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.received = 0;
        let immediate = self.continuation
            || self.start < self.end
            || self.plaintext > 0
            || self.readable
            || (self.writable && self.output.pending() != 0);
        self.continuation = false;
        let timeout = if immediate {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(timeout_ms / 1000.0)
        };
        self.poll_once(timeout)?;
        self.flush()?;
        Ok(crate::shell::Ready {
            socket: self.readable || self.plaintext > 0 || self.start < self.end || immediate,
            wake: self.notify.armed.load(Ordering::Acquire),
        })
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
        self.output.pending() != 0
    }
    pub(crate) fn queue(&mut self, message: &[u8], to: SocketAddr) -> io::Result<()> {
        if to != self.server {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let idle =
            self.output.pending() == 0 && self.tls.as_ref().is_none_or(|tls| !tls.wants_write());
        self.output.queue(message)?;
        self.pending_at = Some(Instant::now());
        if idle {
            self.write_progress = Instant::now();
        }
        self.flush()
    }
    pub(crate) fn continue_soon(&mut self) {
        self.continuation = true;
    }

    pub(crate) fn finish_output(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        while self.output.pending() != 0 || self.tls.as_ref().is_some_and(|tls| tls.wants_write()) {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(io::ErrorKind::TimedOut)?;
            self.wait(remaining.min(Duration::from_millis(50)).as_secs_f64() * 1000.0)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.writable || self.connecting {
            return Ok(());
        }
        if let Some(tls) = &mut self.tls {
            let progress = self.output.write_ready(&mut PlainWriter(tls))?;
            if progress == Progress::Complete {
                self.pending_at = None;
            }
            if progress == Progress::Yielded {
                self.continuation = true;
            }
            for _ in 0..PASSES {
                if !tls.wants_write() {
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
