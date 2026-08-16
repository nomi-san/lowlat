//! Signaling client. The SDK does not link this crate.
//!
//! Async is permitted here and nowhere else. See docs/04-signaling.md section 1.
//!
//! Two things live here that must not confuse each other: the protocol an
//! application speaks to the service, and the seam between any signaling
//! implementation and the SDK. Only the second is normative.

pub mod client;
pub mod message;
pub mod url;

pub use client::{Client, Connect, Error};
pub use message::{ConnUpdate, Credentials, Guest, HostDataBase, Versions};
pub use url::Role;
