//! The system calls under the shell, on Windows: the socket and its options,
//! the completion port that is both the wait and the receive, message sends
//! with a claimed source and segmentation offload, the posted wake, the
//! interface walk, and the mark on the established path.
//!
//! **Everything above this module is written against these names**, the same
//! ones every platform's module carries, so nothing above it changes with the
//! platform. It is the only part of the crate containing `unsafe` here: every
//! block is a thin wrapper with a local safety argument, and `miri` cannot
//! reach them for the reason it cannot on Linux -- it does not execute a
//! system call.

mod addrs;
mod control;
mod io;
mod qos;
mod send;
mod socket;
mod wake;

pub(crate) use addrs::interface_v4;
pub(crate) use io::Io;
pub(crate) use send::{offload_send, offload_unsupported, pinned_send};
pub use socket::Socket;
pub use wake::{Wake, WakeHandle};

/// Completion keys: which of the two things the port carries an entry is.
const KEY_RECV: usize = 1;
const KEY_WAKE: usize = 2;
