//! The association: one SCTP socket, timed against the session clock.
//!
//! The socket keeps its own timeline, relative to its creation; every reading
//! it is given is the session's clock minus the moment the association was
//! opened, so a fake clock advances retransmission timers the way it advances
//! everything else.

use std::time::Duration;

use dcsctp::api::{
    DcSctpSocket, LifecycleId, Message, Metrics, Options, PpId, SendError, SendOptions, Socket,
    SocketEvent, SocketTime, StreamId,
};

use super::{
    MAX_MESSAGE, PPID_BINARY, PRIORITY_AUDIO, PRIORITY_CONTROL, PRIORITY_VIDEO, SCTP_MTU,
    SCTP_PORT, STREAM_QUEUE_FRAGMENTS,
};

/// A stream's message counter, and the identifier handed to the socket so
/// its lifecycle events can be attributed back to the message.
pub(super) type MessageId = u64;

/// One association.
pub(super) struct Assoc {
    socket: Socket,
    opened_ms: f64,
    next_id: MessageId,
}

impl core::fmt::Debug for Assoc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Assoc")
            .field("state", &self.socket.state())
            .field("opened_ms", &self.opened_ms)
            .finish()
    }
}

impl Assoc {
    /// The options every association shares.
    pub(super) fn options() -> Options {
        Options {
            local_port: SCTP_PORT,
            remote_port: SCTP_PORT,
            mtu: SCTP_MTU,
            max_message_size: MAX_MESSAGE,
            per_stream_send_queue_limit: STREAM_QUEUE_FRAGMENTS * lowlat_core::DEFAULT_BODY,
            // Room for every stream at the gate's deepest ceiling.
            max_send_buffer_size: 4 * STREAM_QUEUE_FRAGMENTS * lowlat_core::DEFAULT_BODY,
            ..Options::default()
        }
    }

    pub(super) fn new(now_ms: f64) -> Self {
        let mut socket = Socket::new("web", &Self::options());
        socket.set_stream_priority(StreamId(0), PRIORITY_CONTROL);
        socket.set_stream_priority(StreamId(1), PRIORITY_VIDEO);
        socket.set_stream_priority(StreamId(2), PRIORITY_AUDIO);
        Self {
            socket,
            opened_ms: now_ms,
            next_id: 1,
        }
    }

    /// Begin the association from this side.
    pub(super) fn connect(&mut self) {
        self.socket.connect();
    }

    /// Feed one decrypted packet.
    pub(super) fn input(&mut self, packet: &[u8]) {
        self.socket.handle_input(packet);
    }

    /// Run the timers up to now.
    pub(super) fn advance(&mut self, now_ms: f64) {
        self.socket.advance_time(self.time(now_ms));
    }

    /// Milliseconds until the timers next want running.
    pub(super) fn timer_ms(&self, now_ms: f64) -> f64 {
        let due = self.socket.poll_timeout();
        if due == SocketTime::infinite_future() {
            return f64::INFINITY;
        }
        let now = self.time(now_ms);
        if due <= now {
            0.0
        } else {
            (due - now).as_secs_f64() * 1000.0
        }
    }

    /// Queue one message on a stream, reliable and ordered, and hand back the
    /// identifier its lifecycle events will carry.
    pub(super) fn send(&mut self, stream: u8, payload: Vec<u8>) -> Result<MessageId, SendError> {
        let id = self.next_id;
        let options = SendOptions {
            lifecycle_id: LifecycleId::new(id),
            ..SendOptions::default()
        };
        self.socket.send(
            Message::new(StreamId(u16::from(stream)), PpId(PPID_BINARY), payload),
            &options,
        )?;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        Ok(id)
    }

    /// The next event, if any.
    pub(super) fn next_event(&mut self) -> Option<SocketEvent> {
        self.socket.poll_event()
    }

    /// The next reassembled message, if any.
    pub(super) fn next_message(&mut self) -> Option<Message> {
        self.socket.get_next_message()
    }

    /// Path figures, once the association is up.
    pub(super) fn metrics(&self) -> Option<Metrics> {
        self.socket.get_metrics()
    }

    /// Bytes queued on a stream and not yet handed to the path.
    pub(super) fn buffered(&self, stream: u8) -> usize {
        self.socket.buffered_amount(StreamId(u16::from(stream)))
    }

    /// A message with any stream and identifier, for a test that needs a
    /// peer to misbehave.
    #[cfg(test)]
    pub(super) fn send_raw(&mut self, stream: u16, ppid: u32, payload: Vec<u8>) {
        let _ = self.socket.send(
            Message::new(StreamId(stream), PpId(ppid), payload),
            &SendOptions::default(),
        );
    }

    fn time(&self, now_ms: f64) -> SocketTime {
        SocketTime::zero() + Duration::from_secs_f64((now_ms - self.opened_ms).max(0.0) / 1000.0)
    }
}
