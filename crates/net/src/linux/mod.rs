//! The system calls under the shell, on Linux: the socket and its options,
//! the wait and batched receive, segmentation offload and source-pinned
//! sends, the eventfd wake, and the interface walk.
//!
//! **Everything above this module is written against these names**, so a
//! platform is ported by writing this module again and nothing else changes.
//! It is also the only part of the crate containing `unsafe`: every block is a
//! thin wrapper with a local safety argument, and `miri` cannot reach any of
//! them because it cannot execute a syscall, so the sanitizer build carries
//! that weight instead (docs/08-testing.md 7).

mod addrs;
mod io;
mod send;
mod socket;
mod wake;

pub(crate) use addrs::interface_v4;
pub(crate) use io::Io;
pub(crate) use send::{offload_send, offload_unsupported, pinned_send};
pub use socket::Socket;
pub use wake::{Wake, WakeHandle};
