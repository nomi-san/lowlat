//! The system calls under the shell, on macOS: the socket and its options,
//! the kernel queue that is the loop's one wait -- the socket, the wake's user
//! event and the pass's deadline as a timer marked critical -- the batched
//! receive with each datagram's arrival address, sends that carry their own
//! mark and source, and the interface walk.
//!
//! **Everything above this module is written against these names**, the same
//! ones every platform's module carries, so nothing above it changes with the
//! platform. It is the only part of the crate containing `unsafe` here: every
//! block is a thin wrapper with a local safety argument, and `miri` cannot
//! reach them for the reason it cannot on Linux -- it does not execute a
//! system call.

// The same call over the same structures as on Linux.
#[path = "../linux/addrs.rs"]
mod addrs;
mod io;
mod queue;
mod send;
mod socket;
mod stream;
mod wake;

pub(crate) use addrs::interface_v4;
pub(crate) use io::Io;
pub(crate) use send::{OFFLOAD, offload_send, offload_unsupported, pinned_send};
pub use socket::Socket;
pub(crate) use stream::StreamIo;
pub use wake::{Wake, WakeHandle};
