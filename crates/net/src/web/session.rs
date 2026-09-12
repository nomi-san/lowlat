//! A browser's session: the record layer and the association behind the
//! same six calls the native session answers.
//!
//! The message mapping is the whole of the wire contract, and it is the
//! browser client's rather than ours: one association message per protocol
//! message with no length prefix; the control channel's 13-byte header kept
//! and the video and audio headers dropped, so the payload travels alone; the
//! stream number is the channel number; one payload identifier; every stream
//! reliable and ordered. The rest of this file is plumbing between two state
//! machines and the accounting the congestion controller reads.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use dcsctp::api::{SendError, SocketEvent};
use dimpl::{Config, DtlsCertificate};
use lowlat_core::channel::Drops;
use lowlat_core::congestion;
use lowlat_core::endpoint::{Fault, Media};
use lowlat_core::error::{Error, Result};
use lowlat_core::packet::CHANNEL_COUNT;
use lowlat_core::session::{
    DELIVERY_DEADLINE_MS, Health, LIVENESS_HARD_MS, LIVENESS_SOFT_MS, Pressure,
};
use lowlat_crypto::cert::{Certificate, FINGERPRINT_LEN, fingerprint_of};

use super::dtls::{Link, Out};
use super::sctp::{Assoc, MessageId};
use super::{INBOX_DEPTH, PPID_BINARY};

/// Which side of the handshake this session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Fires the first flight and begins the association. The host.
    Client,
    /// Waits for both. A browser, or the peer in a test.
    Server,
}

/// What one record turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inbound {
    /// Part of the handshake.
    Handshake,
    /// A record that completed no message.
    Record,
    /// A record that completed at least one message.
    Data,
}

/// Where the session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No path yet. Records are refused; nothing is sent.
    AwaitingPath,
    /// The record layer is negotiating.
    Handshaking,
    /// Records flow.
    Up,
    /// Over, one way or another. [`WebSession::fault`] says which.
    Closed,
}

/// A message handed to the association and not yet accounted for.
#[derive(Debug, Clone, Copy)]
struct Sent {
    id: MessageId,
    channel: u8,
    len: usize,
    fragments: u32,
    queued_ms: f64,
    /// When the association reported it fully on the wire, if it has.
    sent_ms: Option<f64>,
}

/// One channel's accounting and inbox.
#[derive(Debug, Default)]
struct Stream {
    inbox: VecDeque<Vec<u8>>,
    /// Messages queued on this channel, which is what [`Media::send_message`]
    /// hands back.
    sequence: u32,
    /// Messages handed to the reader.
    received: u32,
    offered: u64,
    packets_sent: u64,
    acked_bytes: u64,
    drops: Drops,
}

/// The smoothed round trips kept for the minimum.
const RTT_WINDOW: usize = 64;

/// A message's size in the unit the window and the gate count in.
fn fragments_of(len: usize) -> u32 {
    u32::try_from(len.div_ceil(lowlat_core::DEFAULT_BODY)).unwrap_or(u32::MAX)
}

/// A browser's session.
pub struct WebSession {
    role: Role,
    /// The digest the peer's certificate must carry. `None` accepts any,
    /// which only a test peer may do.
    expect: Option<[u8; FINGERPRINT_LEN]>,
    certificate: DtlsCertificate,
    config: Arc<Config>,
    epoch: Instant,
    state: State,
    link: Option<Link>,
    assoc: Assoc,
    /// Records the layer produced while input was being fed, waiting for
    /// the shell to ask for output.
    staged: VecDeque<Vec<u8>>,
    inflight: VecDeque<Sent>,
    streams: Box<[Stream; CHANNEL_COUNT]>,
    level: congestion::Level,
    peer_verified: bool,
    fault: Option<Fault>,
    /// The last reading of the clock any call carried. Queueing a message
    /// carries none, so it is stamped with this.
    now_ms: f64,
    last_progress_ms: f64,
    last_ack_in_ms: f64,
    srtt_ms: f64,
    rtt_samples: VecDeque<f64>,
    /// Messages refused for their payload identifier or their stream.
    skipped: u64,
}

impl core::fmt::Debug for WebSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WebSession")
            .field("role", &self.role)
            .field("state", &self.state)
            .field("link", &self.link)
            .field("assoc", &self.assoc)
            .field("inflight", &self.inflight.len())
            .field("fault", &self.fault)
            .finish()
    }
}

impl WebSession {
    /// A session in either role, with the certificate it will present and the
    /// digest it expects from the peer.
    pub fn new(
        role: Role,
        expect: Option<[u8; FINGERPRINT_LEN]>,
        certificate: &Certificate,
        level: usize,
        now_ms: f64,
    ) -> std::result::Result<Self, dimpl::Error> {
        Self::build(role, expect, certificate, level, now_ms, Link::config()?)
    }

    /// [`WebSession::new`] with the handshake's randomness fixed. **For a
    /// fuzzer**, which wants the same bytes to walk the same path twice, and
    /// for nothing that faces a peer.
    #[doc(hidden)]
    pub fn new_seeded(
        role: Role,
        expect: Option<[u8; FINGERPRINT_LEN]>,
        certificate: &Certificate,
        level: usize,
        now_ms: f64,
        seed: u64,
    ) -> std::result::Result<Self, dimpl::Error> {
        Self::build(
            role,
            expect,
            certificate,
            level,
            now_ms,
            Link::config_seeded(seed)?,
        )
    }

    fn build(
        role: Role,
        expect: Option<[u8; FINGERPRINT_LEN]>,
        certificate: &Certificate,
        level: usize,
        now_ms: f64,
        config: Arc<Config>,
    ) -> std::result::Result<Self, dimpl::Error> {
        let certificate = DtlsCertificate {
            certificate: certificate.der().to_vec(),
            private_key: certificate.key_pkcs8().to_vec(),
        };
        let epoch = Instant::now();
        // A server link sends nothing until a first flight reaches it and
        // runs no timer until then, so it is made at once: the peer's first
        // flight can land before this side's own punch has settled, and a
        // link that did not exist yet would drop it and cost the peer a
        // whole retransmission interval.
        let (link, state) = match role {
            Role::Client => (None, State::AwaitingPath),
            Role::Server => (
                Some(Link::server(
                    Arc::clone(&config),
                    certificate.clone(),
                    epoch,
                    now_ms,
                )),
                State::Handshaking,
            ),
        };
        Ok(Self {
            role,
            expect,
            certificate,
            config,
            epoch,
            state,
            link,
            assoc: Assoc::new(now_ms),
            staged: VecDeque::new(),
            inflight: VecDeque::new(),
            streams: Box::new(core::array::from_fn(|_| Stream::default())),
            level: congestion::level(level),
            peer_verified: false,
            fault: None,
            now_ms,
            last_progress_ms: now_ms,
            last_ack_in_ms: now_ms,
            srtt_ms: 0.0,
            rtt_samples: VecDeque::with_capacity(RTT_WINDOW),
            skipped: 0,
        })
    }

    /// The host's side: the process certificate, and the peer's digest from
    /// the credential exchange.
    pub fn client(
        expect: [u8; FINGERPRINT_LEN],
        level: usize,
        now_ms: f64,
    ) -> std::result::Result<Self, lowlat_crypto::Error> {
        let certificate = lowlat_crypto::cert::certificate()?;
        Self::new(Role::Client, Some(expect), certificate, level, now_ms)
            .map_err(|_| lowlat_crypto::Error::Certificate)
    }

    /// Where the session is, for a log line.
    pub fn is_up(&self) -> bool {
        self.state == State::Up
    }

    /// The association's own figures, for a diagnostic line: congestion
    /// window, smoothed round trip, unacknowledged chunks, retransmitted
    /// packets, and bytes still queued on a stream.
    pub fn figures(&self, channel: u8) -> Option<(usize, f64, usize, usize, usize)> {
        let m = self.assoc.metrics()?;
        Some((
            m.cwnd_bytes,
            m.srtt.as_secs_f64() * 1000.0,
            m.unack_data_count,
            m.rtx_packets_count,
            self.assoc.buffered(channel),
        ))
    }

    /// Messages refused for their payload identifier or their stream.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Hand the record layer bytes to carry as application data, around the
    /// association. **For a fuzzer**, which wants the far side's association
    /// parser fed through real records; nothing that faces a peer calls it.
    #[doc(hidden)]
    pub fn inject_application_data(&mut self, data: &[u8]) -> bool {
        match self.link.as_mut() {
            Some(link) if self.state == State::Up => link.send(data).is_ok(),
            _ => false,
        }
    }

    /// Begin the association from this side whatever the role, which a peer
    /// racing the client to it would do. The client begins it on its own.
    pub fn begin_association(&mut self) {
        if self.state == State::Up {
            self.assoc.connect();
        }
    }

    /// Say goodbye to the peer. The alert leaves on the next output.
    pub fn close(&mut self) {
        if let Some(link) = self.link.as_mut()
            && self.state == State::Up
        {
            let _ = link.close();
        }
    }

    fn end(&mut self, fault: Option<Fault>) {
        if self.state != State::Closed {
            self.state = State::Closed;
            self.fault = fault;
        }
    }

    /// Run the record layer until it has nothing more, staging every record
    /// it produces. Application data goes to the association as it appears.
    fn pump(&mut self, now_ms: f64, buf: &mut [u8]) {
        loop {
            match self.step(now_ms, buf) {
                Step::Packet(len) => {
                    let record = buf.get(..len).unwrap_or_default().to_vec();
                    self.staged.push_back(record);
                }
                Step::Idle | Step::TooSmall => return,
            }
        }
    }

    /// One turn of the record layer, and the association behind it.
    ///
    /// The layer is polled first; when it is idle the association is asked
    /// for a packet, which goes back into the layer and comes out as a
    /// record on the next poll. Transitions land here.
    fn step(&mut self, now_ms: f64, buf: &mut [u8]) -> Step {
        loop {
            let Some(link) = self.link.as_mut() else {
                return Step::Idle;
            };
            if self.state == State::Closed {
                return Step::Idle;
            }
            match link.poll(buf) {
                Out::Packet(len) => return Step::Packet(len),
                Out::Skip => continue,
                Out::TooSmall => return Step::TooSmall,
                Out::Data(data) => {
                    self.assoc.input(data);
                    self.last_progress_ms = now_ms;
                    continue;
                }
                Out::PeerCert(der) => {
                    let ours = fingerprint_of(der);
                    match self.expect {
                        Some(expect) if !digests_match(&ours, &expect) => {
                            lowlat_common::log_warn!(
                                "web: peer certificate is not the one signaled"
                            );
                            self.end(Some(Fault::Handshake));
                            return Step::Idle;
                        }
                        _ => self.peer_verified = true,
                    }
                    continue;
                }
                Out::Connected => {
                    if self.expect.is_some() && !self.peer_verified {
                        lowlat_common::log_warn!("web: peer presented no certificate");
                        self.end(Some(Fault::Handshake));
                        return Step::Idle;
                    }
                    self.state = State::Up;
                    self.last_progress_ms = now_ms;
                    if self.role == Role::Client {
                        self.assoc.connect();
                    }
                    continue;
                }
                Out::Closed => {
                    self.end(None);
                    return Step::Idle;
                }
                Out::Idle => {}
            }

            // The layer is idle. Anything the association wants sent goes in
            // now, and the loop polls it back out as a record.
            if self.state != State::Up {
                return Step::Idle;
            }
            match self.assoc.next_event() {
                None => return Step::Idle,
                Some(SocketEvent::SendPacket(packet)) => {
                    if let Some(link) = self.link.as_mut()
                        && link.send(&packet).is_err()
                    {
                        self.end(Some(Fault::Aborted));
                        return Step::Idle;
                    }
                }
                Some(event) => self.on_event(event, now_ms),
            }
        }
    }

    fn on_event(&mut self, event: SocketEvent, now_ms: f64) {
        match event {
            SocketEvent::SendPacket(_) => {}
            SocketEvent::OnConnected() => {
                lowlat_common::log_info!("web: association up");
                self.last_progress_ms = now_ms;
            }
            SocketEvent::OnClosed() => {
                lowlat_common::log_info!("web: association closed by the peer");
                self.end(None);
            }
            SocketEvent::OnAborted(kind, reason) => {
                lowlat_common::log_warn!("web: association aborted, kind={kind:?} reason={reason}");
                self.end(Some(Fault::Aborted));
            }
            // A refused send has already been answered to its caller and
            // counted there; the association's own line about it is noise
            // at anything above debug.
            SocketEvent::OnError(kind, reason) => {
                lowlat_common::log_debug!("web: association error, kind={kind:?} reason={reason}");
            }
            SocketEvent::OnLifecycleMessageFullySent(id) => {
                let id = id.value();
                if let Some(sent) = self.inflight.iter_mut().find(|s| s.id == id) {
                    sent.sent_ms = Some(now_ms);
                    let fragments = u64::from(sent.fragments);
                    if let Some(stream) = self.streams.get_mut(usize::from(sent.channel)) {
                        stream.packets_sent += fragments;
                    }
                }
            }
            SocketEvent::OnLifecycleMessageDelivered(id) => {
                let id = id.value();
                if let Some(index) = self.inflight.iter().position(|s| s.id == id) {
                    if let Some(sent) = self.inflight.remove(index)
                        && let Some(stream) = self.streams.get_mut(usize::from(sent.channel))
                    {
                        stream.acked_bytes += sent.len as u64;
                    }
                    self.last_ack_in_ms = now_ms;
                    self.last_progress_ms = now_ms;
                }
            }
            SocketEvent::OnLifecycleMessageExpired(id)
            | SocketEvent::OnLifecycleMessageMaybeExpired(id)
            | SocketEvent::OnLifecycleEnd(id) => {
                let id = id.value();
                self.inflight.retain(|s| s.id != id);
            }
            _ => {}
        }
    }

    /// Move every reassembled message into its channel's inbox.
    fn collect(&mut self) -> bool {
        let mut any = false;
        while let Some(message) = self.assoc.next_message() {
            let channel = usize::from(message.stream_id.0);
            let Some(stream) = self
                .streams
                .get_mut(channel)
                .filter(|_| message.ppid.0 == PPID_BINARY)
            else {
                self.skipped += 1;
                continue;
            };
            if stream.inbox.len() >= INBOX_DEPTH {
                stream.inbox.pop_front();
                stream.drops.out_of_window += 1;
            }
            stream.inbox.push_back(message.payload);
            any = true;
        }
        any
    }

    fn refresh_rtt(&mut self) {
        if let Some(metrics) = self.assoc.metrics() {
            let srtt = metrics.srtt.as_secs_f64() * 1000.0;
            if srtt > 0.0 {
                self.srtt_ms = srtt;
                if self.rtt_samples.len() == RTT_WINDOW {
                    self.rtt_samples.pop_front();
                }
                self.rtt_samples.push_back(srtt);
            }
        }
    }

    /// True when a message has been on the wire, unacknowledged, for the
    /// whole of the delivery deadline.
    fn undeliverable(&self, now_ms: f64) -> bool {
        self.inflight
            .iter()
            .any(|sent| sent.sent_ms.is_some() && now_ms - sent.queued_ms >= DELIVERY_DEADLINE_MS)
    }
}

/// What one turn of [`WebSession::step`] produced.
enum Step {
    Packet(usize),
    Idle,
    TooSmall,
}

/// Equality that takes the same time whichever byte differs.
fn digests_match(ours: &[u8; FINGERPRINT_LEN], expect: &[u8; FINGERPRINT_LEN]) -> bool {
    ours.iter()
        .zip(expect.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

impl Media for WebSession {
    type Inbound = Inbound;

    fn path_ready(&mut self, now_ms: f64) {
        self.now_ms = now_ms;
        if self.state != State::AwaitingPath {
            return;
        }
        let config = Arc::clone(&self.config);
        let certificate = self.certificate.clone();
        // A client link fires its first flight the moment it exists, which
        // is why it waits for the path: made earlier, its retries run out
        // while the punch is still finding one.
        match Link::client(config, certificate, self.epoch, now_ms) {
            Ok(link) => {
                self.link = Some(link);
                self.state = State::Handshaking;
                self.last_progress_ms = now_ms;
            }
            Err(error) => {
                lowlat_common::log_warn!("web: record layer would not start, error={error:?}");
                self.end(Some(Fault::Handshake));
            }
        }
    }

    fn process_input(
        &mut self,
        datagram: &[u8],
        now_ms: f64,
        scratch: &mut [u8],
    ) -> Result<Inbound> {
        // A record is at least its header, and its first byte names one of
        // the four record kinds. Anything else is refused before the layer
        // sees it, so what the layer refuses is what it parsed.
        if datagram.len() < 13 || !matches!(datagram.first(), Some(0x14..=0x17)) {
            return Err(Error::Malformed);
        }
        self.now_ms = now_ms;
        let Some(link) = self.link.as_mut() else {
            return Err(Error::Malformed);
        };
        if self.state == State::Closed {
            return Err(Error::Malformed);
        }
        if let Err(error) = link.feed(datagram) {
            lowlat_common::log_warn!("web: record layer failed, error={error:?}");
            self.end(Some(Fault::Handshake));
            return Err(Error::Decrypt);
        }
        let handshaking = self.state == State::Handshaking;
        self.pump(now_ms, scratch);
        if self.collect() {
            self.last_progress_ms = now_ms;
            return Ok(Inbound::Data);
        }
        Ok(if handshaking {
            Inbound::Handshake
        } else {
            Inbound::Record
        })
    }

    fn poll(&mut self, now_ms: f64) {
        self.now_ms = now_ms;
        if self.state == State::Closed {
            return;
        }
        self.assoc.advance(now_ms);
        if let Some(link) = self.link.as_mut()
            && let Err(error) = link.tick(now_ms)
        {
            lowlat_common::log_warn!("web: record layer timed out, error={error:?}");
            self.end(Some(Fault::Handshake));
        }
        self.refresh_rtt();
    }

    fn next_timer_ms(&self, now_ms: f64) -> f64 {
        match self.state {
            State::AwaitingPath => f64::INFINITY,
            State::Closed => 0.0,
            State::Handshaking | State::Up => {
                let link = self
                    .link
                    .as_ref()
                    .map_or(f64::INFINITY, |l| l.timer_ms(now_ms));
                link.min(self.assoc.timer_ms(now_ms))
            }
        }
    }

    fn get_output(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<usize>> {
        self.now_ms = now_ms;
        if let Some(record) = self.staged.pop_front() {
            let Some(room) = out.get_mut(..record.len()) else {
                self.staged.push_front(record);
                return Some(Err(Error::BufferTooSmall));
            };
            room.copy_from_slice(&record);
            return Some(Ok(record.len()));
        }
        match self.step(now_ms, out) {
            Step::Packet(len) => Some(Ok(len)),
            Step::TooSmall => Some(Err(Error::BufferTooSmall)),
            Step::Idle => {
                self.collect();
                None
            }
        }
    }

    fn send_message(&mut self, channel: u8, header: &[u8], payload: &[u8]) -> Result<u32> {
        if self.state == State::Closed {
            return Err(Error::Malformed);
        }
        let Some(stream) = self.streams.get_mut(usize::from(channel)) else {
            return Err(Error::Malformed);
        };
        // The one place the contract is applied: the control channel keeps
        // its header, every other channel carries the payload alone.
        let body = if channel == 0 {
            let mut body = Vec::with_capacity(header.len() + payload.len());
            body.extend_from_slice(header);
            body.extend_from_slice(payload);
            body
        } else {
            payload.to_vec()
        };
        let len = body.len();
        match self.assoc.send(channel, body) {
            Ok(id) => {
                let sequence = stream.sequence;
                stream.sequence = stream.sequence.wrapping_add(1);
                stream.offered += len as u64;
                self.inflight.push_back(Sent {
                    id,
                    channel,
                    len,
                    fragments: fragments_of(len),
                    queued_ms: self.now_ms,
                    sent_ms: None,
                });
                Ok(sequence)
            }
            Err(SendError::MessageTooLarge { .. } | SendError::ResourceExhaustion) => {
                Err(Error::Oversized)
            }
            Err(SendError::EmptyPayload | SendError::ShuttingDown) => Err(Error::Malformed),
        }
    }

    fn take_message(&mut self, channel: u8, out: &mut [u8]) -> Option<Result<usize>> {
        let stream = self.streams.get_mut(usize::from(channel))?;
        let message = stream.inbox.pop_front()?;
        let Some(room) = out.get_mut(..message.len()) else {
            stream.inbox.push_front(message);
            return Some(Err(Error::BufferTooSmall));
        };
        room.copy_from_slice(&message);
        stream.received = stream.received.wrapping_add(1);
        Some(Ok(message.len()))
    }

    fn health(&self, now_ms: f64) -> Health {
        if self.state == State::Closed {
            return Health::Dead;
        }
        let idle = now_ms - self.last_progress_ms;
        if idle >= LIVENESS_HARD_MS {
            return Health::Dead;
        }
        if self.undeliverable(now_ms) {
            return Health::Undeliverable;
        }
        if idle >= LIVENESS_SOFT_MS {
            Health::Stalled
        } else {
            Health::Alive
        }
    }

    fn fault(&self) -> Option<Fault> {
        self.fault
    }

    fn send_pressure(&self, channel: u8) -> Option<Pressure> {
        let stream = self.streams.get(usize::from(channel))?;
        let threshold = self.level.rtt_mult * self.srtt_ms + self.level.base_ms;
        let now_ms = self.now_ms;
        // The window is everything queued and not yet delivered, in
        // fragments. Stale within it: what has not been handed to the path
        // at all, as an unsent fragment is natively, and what was handed
        // over whole longer ago than the level's threshold. A message part
        // way onto the wire is fresh for the part that is there.
        let (mut window, mut pending, mut stale) = (0u32, 0u32, 0u32);
        for sent in self.inflight.iter().filter(|s| s.channel == channel) {
            window = window.saturating_add(sent.fragments);
            match sent.sent_ms {
                None => pending = pending.saturating_add(sent.fragments),
                Some(sent_ms) if now_ms - sent_ms > threshold => {
                    stale = stale.saturating_add(sent.fragments);
                }
                Some(_) => {}
            }
        }
        let unsent = fragments_of(self.assoc.buffered(channel));
        stale = stale.saturating_add(unsent.min(pending));
        let metrics = self.assoc.metrics();
        let rtx = metrics.as_ref().map_or(0, |m| m.rtx_packets_count as u64);
        let rtx_bytes = metrics.as_ref().map_or(0, |m| m.rtx_bytes_count);
        Some(Pressure {
            window,
            stale,
            // Retransmitted bytes are not attributed per stream by the
            // association; they are charged to the channel that carries
            // nearly all of them.
            bytes_sent: stream.offered + if channel == 1 { rtx_bytes } else { 0 },
            packets_sent: stream.packets_sent,
            acked_bytes: stream.acked_bytes,
            nack_resends: if channel == 1 { rtx } else { 0 },
            timeout_resends: 0,
        })
    }

    fn srtt_ms(&self) -> f64 {
        self.srtt_ms
    }

    fn rtt_min_ms(&self) -> f64 {
        self.rtt_samples
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
            .min(self.srtt_ms)
    }

    fn last_ack_in_ms(&self) -> f64 {
        self.last_ack_in_ms
    }

    fn recv_cumulative(&self, channel: u8) -> Option<u32> {
        Some(self.streams.get(usize::from(channel))?.received)
    }

    fn recv_drops(&self, channel: u8) -> Option<Drops> {
        Some(self.streams.get(usize::from(channel))?.drops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIRE: usize = lowlat_core::MAX_DATAGRAM;

    /// Two sessions facing each other, each with its own identity and the
    /// other's digest, both told the path exists at `now`.
    fn pair(now: f64) -> (WebSession, WebSession) {
        let (client, server) = pair_expecting(now, None);
        (client, server)
    }

    /// The same, with the client told to expect `expect` instead of the
    /// server's real digest when one is given.
    fn pair_expecting(now: f64, expect: Option<[u8; FINGERPRINT_LEN]>) -> (WebSession, WebSession) {
        let ours = Certificate::generate().unwrap();
        let theirs = Certificate::generate().unwrap();
        let mut client = WebSession::new(
            Role::Client,
            Some(expect.unwrap_or(*theirs.fingerprint())),
            &ours,
            1,
            now,
        )
        .unwrap();
        let mut server =
            WebSession::new(Role::Server, Some(*ours.fingerprint()), &theirs, 1, now).unwrap();
        client.path_ready(now);
        server.path_ready(now);
        (client, server)
    }

    /// Move everything `from` has to say to `to`. Returns how many records.
    fn exchange(from: &mut WebSession, to: &mut WebSession, now: f64) -> usize {
        let mut wire = [0u8; WIRE];
        let mut scratch = [0u8; WIRE];
        let mut moved = 0;
        while let Some(result) = from.get_output(now, &mut wire) {
            let len = result.unwrap();
            let _ = to.process_input(&wire[..len], now, &mut scratch);
            moved += 1;
        }
        moved
    }

    /// Run both sides for `steps` ticks of `step_ms`, exchanging each way.
    fn run(client: &mut WebSession, server: &mut WebSession, now: &mut f64, steps: usize) {
        for _ in 0..steps {
            client.poll(*now);
            server.poll(*now);
            exchange(client, server, *now);
            exchange(server, client, *now);
            *now += 10.0;
        }
    }

    fn take(session: &mut WebSession, channel: u8) -> Option<Vec<u8>> {
        let mut out = vec![0u8; MAX_MESSAGE_TEST];
        let len = session.take_message(channel, &mut out)?.unwrap();
        out.truncate(len);
        Some(out)
    }

    const MAX_MESSAGE_TEST: usize = 1 << 20;

    #[test]
    fn a_web_session_refuses_records_before_a_path() {
        let certificate = Certificate::generate().unwrap();
        let mut session =
            WebSession::new(Role::Client, Some([0; 32]), &certificate, 1, 0.0).unwrap();
        let mut scratch = [0u8; WIRE];
        let record = [0x16u8; 64];
        assert_eq!(
            session
                .process_input(&record, 0.0, &mut scratch)
                .unwrap_err(),
            Error::Malformed
        );
        assert!(session.next_timer_ms(0.0).is_infinite());
        let mut wire = [0u8; WIRE];
        assert!(session.get_output(0.0, &mut wire).is_none());
        assert_eq!(session.health(0.0), Health::Alive);
    }

    /// The process certificate must load into the record layer and produce
    /// a first flight: a key encoding the layer cannot read would show up
    /// here as a panic rather than in a session.
    #[test]
    fn the_certificate_loads_into_the_dtls_client() {
        let mut session = WebSession::client([0x42; 32], 1, 0.0).unwrap();
        session.path_ready(0.0);
        let mut wire = [0u8; WIRE];
        let len = session
            .get_output(0.0, &mut wire)
            .expect("a flight")
            .unwrap();
        assert_eq!(wire[0], 0x16, "the first record is a handshake record");
        assert!(len >= 13);
        // Drained to the end, as the shell drains, the retransmission
        // deadline is known.
        while session.get_output(0.0, &mut wire).is_some() {}
        let due = session.next_timer_ms(0.0);
        assert!(due.is_finite() && due <= 1000.0, "flight timer reads {due}");
    }

    #[test]
    fn a_pair_handshakes_and_associates() {
        let (mut client, mut server) = pair(0.0);
        let mut now = 0.0;
        run(&mut client, &mut server, &mut now, 20);
        assert!(client.is_up(), "{client:?}");
        assert!(server.is_up(), "{server:?}");
        assert_eq!(client.fault(), None);
        assert_eq!(server.fault(), None);
        assert!(
            client.assoc.metrics().is_some(),
            "no association on the client"
        );
        assert!(
            server.assoc.metrics().is_some(),
            "no association on the server"
        );
    }

    #[test]
    fn a_control_message_keeps_its_header_and_media_loses_theirs() {
        let (mut client, mut server) = pair(0.0);
        let mut now = 0.0;
        run(&mut client, &mut server, &mut now, 20);

        client.send_message(0, b"HDR-13-BYTES!", b"body").unwrap();
        client.send_message(1, b"VIDEO-HDR!", b"bitstream").unwrap();
        client.send_message(2, b"AUDIO-HEADER-15", b"opus").unwrap();
        run(&mut client, &mut server, &mut now, 5);

        assert_eq!(take(&mut server, 0).unwrap(), b"HDR-13-BYTES!body");
        assert_eq!(take(&mut server, 1).unwrap(), b"bitstream");
        assert_eq!(take(&mut server, 2).unwrap(), b"opus");
        assert_eq!(server.recv_cumulative(1), Some(1));

        // And back the other way, the same rule.
        server.send_message(0, b"HDR-13-BYTES!", b"").unwrap();
        server.send_message(1, b"VIDEO-HDR!", b"reply").unwrap();
        run(&mut client, &mut server, &mut now, 5);
        assert_eq!(take(&mut client, 0).unwrap(), b"HDR-13-BYTES!");
        assert_eq!(take(&mut client, 1).unwrap(), b"reply");
    }

    #[test]
    fn a_foreign_ppid_is_skipped_and_counted() {
        let (mut client, mut server) = pair(0.0);
        let mut now = 0.0;
        run(&mut client, &mut server, &mut now, 20);

        server.assoc.send_raw(1, 51, b"a string".to_vec());
        server.assoc.send_raw(
            u16::from(CHANNEL_COUNT as u8),
            PPID_BINARY,
            b"nineteen".to_vec(),
        );
        server.assoc.send_raw(1, PPID_BINARY, b"binary".to_vec());
        run(&mut client, &mut server, &mut now, 5);

        assert_eq!(take(&mut client, 1).unwrap(), b"binary");
        assert!(take(&mut client, 1).is_none());
        assert_eq!(client.skipped(), 2);
    }

    #[test]
    fn the_fingerprint_is_checked_before_connected_is_believed() {
        let (mut client, mut server) = pair_expecting(0.0, Some([0xEE; 32]));
        let mut now = 0.0;
        run(&mut client, &mut server, &mut now, 20);
        assert!(!client.is_up());
        assert_eq!(client.fault(), Some(Fault::Handshake));
        assert_eq!(client.health(now), Health::Dead);
        assert!(client.next_timer_ms(now) <= 0.0);
        assert!(
            client.assoc.metrics().is_none(),
            "an association was made anyway"
        );
    }

    #[test]
    fn timers_fold_the_sooner_of_dtls_and_sctp() {
        let (mut client, mut server) = pair(0.0);
        let mut now = 0.0;
        run(&mut client, &mut server, &mut now, 20);
        // Both engines armed: the association keeps a heartbeat, the record
        // layer keeps whatever it asked for, and the session reports the
        // sooner of the two.
        let link = client.link.as_ref().unwrap().timer_ms(now);
        let assoc = client.assoc.timer_ms(now);
        let due = client.next_timer_ms(now);
        assert!(
            (due - link.min(assoc)).abs() < 1e-9,
            "{due} vs {link} and {assoc}"
        );
        assert!(due.is_finite(), "nothing armed after the handshake");
    }

    #[test]
    fn a_large_message_crosses_whole() {
        let (mut client, mut server) = pair(0.0);
        let mut now = 0.0;
        run(&mut client, &mut server, &mut now, 20);
        let frame: Vec<u8> = (0..600 * 1024).map(|i| (i % 251) as u8).collect();
        client.send_message(1, b"VIDEO-HDR!", &frame).unwrap();
        run(&mut client, &mut server, &mut now, 200);
        assert_eq!(take(&mut server, 1).unwrap(), frame);
        let pressure = client.send_pressure(1).unwrap();
        assert_eq!(pressure.acked_bytes, frame.len() as u64);
        assert_eq!(pressure.window, 0, "delivered but still counted in flight");
    }

    #[test]
    fn a_message_beyond_the_ceiling_is_refused_as_oversized() {
        let (mut client, mut server) = pair(0.0);
        let mut now = 0.0;
        run(&mut client, &mut server, &mut now, 20);
        let too_big = vec![0u8; super::super::MAX_MESSAGE + 1];
        assert_eq!(
            client.send_message(1, b"", &too_big).unwrap_err(),
            Error::Oversized
        );
        assert_eq!(
            client.send_message(19, b"", b"x").unwrap_err(),
            Error::Malformed
        );
    }

    #[test]
    fn a_close_notify_ends_the_peer_cleanly() {
        let (mut client, mut server) = pair(0.0);
        let mut now = 0.0;
        run(&mut client, &mut server, &mut now, 20);
        client.close();
        run(&mut client, &mut server, &mut now, 5);
        assert_eq!(server.health(now), Health::Dead);
        assert_eq!(server.fault(), None);
    }
}
