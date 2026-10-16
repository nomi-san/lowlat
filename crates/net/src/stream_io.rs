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
        self.output.queue(message)?;
        self.flush()
    }
    pub(crate) fn continue_soon(&mut self) {
        self.continuation = true;
    }

    fn flush(&mut self) -> io::Result<()> {
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
                    self.continuation |= self.output.pending() != 0;
                    return Ok(());
                }
                match tls.write_tls(&mut self.socket) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(_) => {}
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
            match self.output.write_ready(&mut self.socket)? {
                Progress::Blocked => self.writable = false,
                Progress::Yielded => self.continuation = true,
                Progress::Complete => {}
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
