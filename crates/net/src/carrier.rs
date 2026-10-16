//! Dispatch at the IO boundary; UDP batching and platform receives stay intact.
use crate::shell::Ready;
use crate::{Socket, Wake, WakeHandle};
use std::io;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug)]
pub(crate) enum Io {
    Datagram(crate::sys::Io),
    #[cfg(feature = "turn-stream")]
    Stream {
        stream: Box<crate::stream_io::StreamIo>,
        socket: Socket,
    },
}

impl Io {
    pub(crate) fn new(socket: Socket, wake: Wake) -> Self {
        match wake.kind {
            crate::wake::Kind::Datagram(wake) => Self::Datagram(crate::sys::Io::new(socket, wake)),
            #[cfg(feature = "turn-stream")]
            crate::wake::Kind::Stream(stream) => Self::Stream { stream, socket },
        }
    }
    pub(crate) fn socket(&self) -> &Socket {
        match self {
            Self::Datagram(io) => io.socket(),
            #[cfg(feature = "turn-stream")]
            Self::Stream { socket, .. } => socket,
        }
    }
    pub(crate) fn handle(&self) -> io::Result<WakeHandle> {
        match self {
            Self::Datagram(io) => Ok(WakeHandle::datagram(io.wake().handle()?)),
            #[cfg(feature = "turn-stream")]
            Self::Stream { stream, .. } => Ok(WakeHandle::stream(stream.notify())),
        }
    }
    pub(crate) fn wait(&mut self, timeout: f64) -> io::Result<Ready> {
        match self {
            Self::Datagram(io) => io.wait(timeout),
            #[cfg(feature = "turn-stream")]
            Self::Stream { stream, .. } => stream.wait(timeout),
        }
    }
    pub(crate) fn take_wake(&mut self) -> io::Result<bool> {
        match self {
            Self::Datagram(io) => io.take_wake(),
            #[cfg(feature = "turn-stream")]
            Self::Stream { stream, .. } => Ok(stream.take_wake()),
        }
    }
    pub(crate) fn drain(&mut self) -> io::Result<usize> {
        match self {
            Self::Datagram(io) => io.drain(),
            #[cfg(feature = "turn-stream")]
            Self::Stream { stream, .. } => stream.drain(),
        }
    }
    pub(crate) fn saturated(&self) -> bool {
        match self {
            Self::Datagram(io) => io.saturated(),
            #[cfg(feature = "turn-stream")]
            Self::Stream { .. } => true,
        }
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = (SocketAddr, Option<IpAddr>, &[u8])> {
        let udp = match self {
            Self::Datagram(io) => Some(io),
            #[cfg(feature = "turn-stream")]
            Self::Stream { .. } => None,
        };
        let iterator = udp.into_iter().flat_map(|io| io.iter());
        #[cfg(feature = "turn-stream")]
        let iterator = iterator.chain(match self {
            Self::Stream { stream, .. } => stream.frame(),
            Self::Datagram(_) => None,
        });
        iterator
    }
    pub(crate) fn mark(&mut self, to: SocketAddr) {
        match self {
            Self::Datagram(io) => io.mark(to),
            #[cfg(feature = "turn-stream")]
            Self::Stream { .. } => {}
        }
    }
}
