//! The host's event queue: the shared queue over the host's own event type.
//!
//! The queue itself is `lowlat_common::events`; what the host adds is which
//! of its events carries a body and which one survives a full queue.

use crate::admission::Event;
use lowlat_common::events::{self, Queued};

pub type Sender = events::Sender<Event>;
pub type Receiver = events::Receiver<Event>;
pub type Received = events::Received<Event>;
pub type Delivery = events::Delivery<Event>;

impl Queued for Event {
    fn body(&self) -> &[u8] {
        match self {
            Event::UserData { text, .. } => text,
            _ => &[],
        }
    }

    /// **Only the fatal one does.** It is the only explanation for everything
    /// that stopped, and no dropped count can convey that.
    fn undroppable(&self) -> bool {
        matches!(self, Event::Fatal { .. })
    }
}

/// One queue, as the two ends of it.
pub fn queue() -> (Sender, Receiver) {
    events::queue()
}
