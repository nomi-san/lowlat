//! One peer, both state machines: connectivity first, then media.
//!
//! The shell drives this rather than the two engines separately. Classifying a
//! datagram and merging two timers are protocol decisions, not IO ones, and
//! keeping them here means they are exercised with injected time and replayable
//! from a seed. Left to the shell they would be the improvised glue that sinks
//! this kind of system: the part with no tests, written twice, once per
//! platform.
//!
//! The shell's whole job against this object is four calls:
//!
//! ```text
//! loop:
//!     timeout = endpoint.next_timer_ms(now)
//!     wait for a datagram, an application send, or that timeout
//!     for each datagram:  endpoint.process_input(bytes, from, local, now, scratch)
//!     endpoint.poll(now)
//!     drain:              while let Some(e) = endpoint.get_output(now, buf) { send(e) }
//! ```
//!
//! An output carries where it goes and how it must be sent, because a mapping
//! probe leaves at a TTL that must be restored afterwards and a shell cannot be
//! trusted to remember an obligation that is not in the type.
//!
//! **A relay attempt goes through the relay and nowhere else**
//! (docs/03-connectivity.md 7.2). The relay is recognised by its address
//! before anything is classified, because nothing it sends is shaped like a
//! check; what it relays is unwrapped and classified as though it had come
//! from the peer. Everything bound for a peer leaves wrapped -- checks, their
//! answers and records alike -- so no answer can leave outside the relay, and
//! nothing leaves toward a peer until the relayed address may be offered.

use core::fmt;
use core::net::{IpAddr, SocketAddr};

use crate::channel::Drops;
use crate::conn::{self, Conn, Egress, Ttl};
use crate::demux::{self, Datagram};
use crate::error::{Error, Result};
use crate::relay::{self, Relay};
use crate::session::{self, Health, Pressure, Session};
use crate::stun;
use crate::turn;

/// What an inbound datagram turned out to be.
///
/// The two engines keep their own vocabularies; nothing is gained by flattening
/// them into one enum that half the callers would have to ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inbound<I = session::Inbound> {
    /// A connectivity check or its answer.
    Connectivity(conn::Inbound),
    /// A record for the media half.
    Media(I),
    /// The relay answered one of our requests.
    Relay,
}

/// A failure of the media half that the peer did not cause by leaving.
///
/// The native session has none: its records either authenticate or they do
/// not, and a peer that stops is a matter of [`Health`]. A transport with a
/// handshake can fail before any record flows, and that failure has a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fault {
    /// The security handshake did not complete, or the peer was not who the
    /// credential exchange said it would be.
    Handshake,
    /// The peer's transport ended the association with an error.
    Aborted,
}

/// The media half of a peer: what carries messages once a path exists.
///
/// One vocabulary, more than one pipe. The shell and the guest loop are
/// written against this and instantiated per transport, so the loop that
/// drives a native session and the loop that drives a browser's are the same
/// code and cannot drift apart. The methods are exactly what the guest loop
/// reads and nothing more; anything a single transport needs beyond them is
/// reached through that transport's own type.
///
/// Every method takes time as a parameter and none touches a socket. That is
/// the property the whole core has, and a media half that lives outside the
/// core keeps it too.
pub trait Media: fmt::Debug {
    /// What one record turned out to be, in this transport's own words.
    type Inbound: Copy + fmt::Debug;

    /// The connectivity engine chose a path. Called once, before any record
    /// is emitted toward it. A transport with a handshake starts it here.
    fn path_ready(&mut self, _now_ms: f64) {}

    /// Feed one datagram the demultiplexer classed as a record.
    fn process_input(
        &mut self,
        datagram: &[u8],
        now_ms: f64,
        scratch: &mut [u8],
    ) -> Result<Self::Inbound>;

    /// Housekeeping: timers, acknowledgements, liveness.
    fn poll(&mut self, now_ms: f64);

    /// Milliseconds until this half next needs attention.
    fn next_timer_ms(&self, now_ms: f64) -> f64;

    /// Emit the next datagram into `out`. Drive until `None`.
    fn get_output(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<usize>>;

    /// Queue one message on a channel. Returns the message's number in the
    /// channel's own sequence.
    fn send_message(&mut self, channel: u8, header: &[u8], payload: &[u8]) -> Result<u32>;

    /// Take the next complete message on a channel into `out`.
    fn take_message(&mut self, channel: u8, out: &mut [u8]) -> Option<Result<usize>>;

    /// Liveness, judged in both directions.
    fn health(&self, now_ms: f64) -> Health;

    /// A failure the peer did not cause by leaving, if one has happened.
    fn fault(&self) -> Option<Fault> {
        None
    }

    /// One channel's send pressure, as the congestion controller, the
    /// delivery gate and the diagnostics read it.
    fn send_pressure(&self, channel: u8) -> Option<Pressure>;

    /// Smoothed round trip, in fractional milliseconds.
    fn srtt_ms(&self) -> f64;

    /// The smallest recent round trip beside the smoothed one.
    fn rtt_min_ms(&self) -> f64;

    /// When the peer last acknowledged anything.
    fn last_ack_in_ms(&self) -> f64;

    /// Contiguous receive frontier on a channel.
    fn recv_cumulative(&self, channel: u8) -> Option<u32>;

    /// Stores the receive side refused on a channel, counted per kind.
    fn recv_drops(&self, channel: u8) -> Option<Drops>;
}

/// A peer: the punch and the media half it hands over to.
#[derive(Debug)]
pub struct Endpoint<'a, M: Media = Session<'a>> {
    conn: Conn<'a>,
    session: M,
    /// The relay a relay attempt goes through; `None` for a direct attempt.
    relay: Option<Relay<'a>>,
}

impl<'a, M: Media> Endpoint<'a, M> {
    /// Pair a connectivity attempt with the media half that will use its path.
    ///
    /// Both are built by the caller, because a session needs ring storage and
    /// key material that arrive from different places at different times.
    pub fn new(conn: Conn<'a>, session: M) -> Self {
        Self {
            conn,
            session,
            relay: None,
        }
    }

    /// The same, for a relay attempt: everything goes through `relay`.
    pub fn relayed(conn: Conn<'a>, session: M, relay: Relay<'a>) -> Self {
        Self {
            conn,
            session,
            relay: Some(relay),
        }
    }

    /// The connectivity engine, for candidates and outcome.
    pub fn conn(&mut self) -> &mut Conn<'a> {
        &mut self.conn
    }

    /// The relay, in a relay attempt: its state, and the relayed address to
    /// offer once there is one.
    pub fn relay(&self) -> Option<&Relay<'a>> {
        self.relay.as_ref()
    }

    /// The relay, to release on a clean leave.
    pub fn relay_mut(&mut self) -> Option<&mut Relay<'a>> {
        self.relay.as_mut()
    }

    /// Offer a remote candidate.
    ///
    /// In a relay attempt only what the relay can reach is kept, and the
    /// relay is asked to admit its address: an IPv6 address is out of an IPv4
    /// allocation's reach, and nothing is ever relayed toward loopback.
    pub fn add_candidate(&mut self, addr: SocketAddr, kind: conn::Kind) -> Result<()> {
        if let Some(relay) = self.relay.as_mut() {
            if !relay::reachable(addr) {
                return Ok(());
            }
            relay.permit(stun::canonical(addr).ip());
        }
        self.conn.add_candidate(addr, kind)
    }

    /// The media half, for messages.
    pub fn session(&mut self) -> &mut M {
        &mut self.session
    }

    /// The chosen path, once there is one. Media flows only after this.
    pub fn path(&self) -> Option<SocketAddr> {
        self.conn.path()
    }

    /// Liveness of the media half.
    pub fn health(&self, now_ms: f64) -> Health {
        self.session.health(now_ms)
    }

    /// A failure of the media half the peer did not cause by leaving.
    pub fn fault(&self) -> Option<Fault> {
        self.session.fault()
    }

    /// Feed one received datagram, whatever it is.
    ///
    /// Classification happens here, on the first two bytes, before either
    /// engine sees the bytes. Anything not shaped like a check goes to the
    /// record layer, where authentication rejects it, so the check parser is
    /// never handed input that was not already check-shaped.
    ///
    /// `local` is the address the datagram arrived at, when the shell can
    /// say. The connectivity engine answers a check from it and latches the
    /// winning answer's; the record layer has no use for it.
    pub fn process_input(
        &mut self,
        datagram: &[u8],
        from: SocketAddr,
        local: Option<IpAddr>,
        now_ms: f64,
        scratch: &mut [u8],
    ) -> Result<Inbound<M::Inbound>> {
        let Some(relay) = self.relay.as_mut() else {
            return self.deliver(datagram, from, local, now_ms, scratch);
        };
        // A relay attempt talks to the relay alone.
        if stun::canonical(from) != relay.server() {
            return Err(Error::Malformed);
        }
        let Some((peer, data)) = relay.unwrap(datagram, local, now_ms)? else {
            return Ok(Inbound::Relay);
        };
        let inbound = self.deliver(data, peer, None, now_ms, scratch)?;
        // The path follows the host: once there is one, media goes wherever
        // the host's authenticated traffic comes from.
        if let (Inbound::Media(_), Some(_), Some(relay)) =
            (&inbound, self.conn.path(), self.relay.as_mut())
        {
            relay.follow(peer);
        }
        Ok(inbound)
    }

    /// Classify a datagram as the peer sent it and hand it to its engine.
    fn deliver(
        &mut self,
        datagram: &[u8],
        from: SocketAddr,
        local: Option<IpAddr>,
        now_ms: f64,
        scratch: &mut [u8],
    ) -> Result<Inbound<M::Inbound>> {
        match demux::classify(datagram) {
            Datagram::Check => {
                let inbound = self.conn.process_input(datagram, from, local)?;
                if let conn::Inbound::PathEstablished(path) = inbound {
                    self.session.path_ready(now_ms);
                    if let Some(relay) = self.relay.as_mut() {
                        relay.follow(path);
                    }
                }
                Ok(Inbound::Connectivity(inbound))
            }
            Datagram::Record => Ok(Inbound::Media(
                self.session.process_input(datagram, now_ms, scratch)?,
            )),
        }
    }

    /// Housekeeping for both engines, and the relay.
    pub fn poll(&mut self, now_ms: f64) {
        if let Some(relay) = self.relay.as_mut() {
            relay.poll(now_ms);
        }
        self.conn.poll(now_ms);
        self.session.poll(now_ms);
    }

    /// Milliseconds until either engine next needs attention.
    ///
    /// The shell arms one wait from this. Taking the minimum is the whole
    /// reason it lives here: a shell that armed from the session alone would
    /// miss every connectivity deadline, and one that armed from the
    /// connectivity engine alone would poll pointlessly once a path was chosen,
    /// because a finished attempt asks for no wakeups at all.
    pub fn next_timer_ms(&self, now_ms: f64) -> f64 {
        let session = self.session.next_timer_ms(now_ms);
        match &self.relay {
            None => self.conn.next_timer_ms(now_ms).min(session),
            // The punch waits for the relayed address, so until there is one
            // its deadlines would only wake the loop to send nothing.
            Some(relay) if relay.relayed().is_none() => relay.next_timer_ms(now_ms).min(session),
            Some(relay) => relay
                .next_timer_ms(now_ms)
                .min(self.conn.next_timer_ms(now_ms))
                .min(session),
        }
    }

    /// Emit the next datagram, with where it goes and how to send it.
    ///
    /// Connectivity drains first. Its datagrams are small, time critical, and
    /// owed to a peer that reads silence as unreachable; and until a path
    /// exists there is nowhere to send media anyway.
    pub fn get_output(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<Egress>> {
        if self.relay.is_some() {
            return self.relayed_output(now_ms, out);
        }
        if let Some(result) = self.conn.get_output(now_ms, out) {
            return Some(result);
        }

        // No path, no destination. The session may have output ready; it waits.
        let to = self.conn.path()?;
        Some(match self.session.get_output(now_ms, out)? {
            Ok(len) => Ok(Egress {
                to,
                ttl: Ttl::Default,
                len,
                // The address the path was proven at. Left to itself the
                // kernel re-selects per send, and on a multi-homed host a
                // routing change moves the source mid-session -- the peer's
                // filter then sees a stranger where its session was.
                from: self.conn.local(),
            }),
            Err(error) => Err(error),
        })
    }

    /// Everything a relay attempt sends goes to the relay: its own requests
    /// first, then checks and their answers as indications, then media, on
    /// a channel once one is bound to where it goes.
    fn relayed_output(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<Egress>> {
        let relay = self.relay.as_mut()?;
        if let Some(result) = relay.get_output(now_ms, out) {
            return Some(result.map(|len| to_relay(relay, len)));
        }
        // Nothing goes toward a peer before the relayed address may be offered.
        relay.relayed()?;

        let head = turn::INDICATION_HEADER_V4;
        loop {
            // Room for any check is asked for first: the engine gives up an
            // owed answer as it emits it, and one written into too little
            // room would be lost.
            let Some(body) = out.get_mut(head..head + stun::MAX_BUILT) else {
                return Some(Err(Error::BufferTooSmall));
            };
            let Some(result) = self.conn.get_output(now_ms, body) else {
                break;
            };
            let egress = match result {
                Ok(egress) => egress,
                Err(error) => return Some(Err(error)),
            };
            // The mapping probe opens a mapping on the path that crosses
            // translation. Through the relay there is none to open, and it
            // would reach the host at full length ahead of the readiness
            // marker.
            if egress.ttl == Ttl::Probe || !relay.relayable(egress.to) {
                continue;
            }
            let tid = relay.indication_id();
            return Some(
                turn::wrap_indication(out, tid, egress.to, egress.len)
                    .map(|len| to_relay(relay, len)),
            );
        }
        self.relayed_media(now_ms, out)
    }

    /// Media toward the peer, once there is a path, framed for the relay.
    fn relayed_media(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<Egress>> {
        let path = self.conn.path()?;
        let relay = self.relay.as_mut()?;
        let to = relay.destination().unwrap_or(path);
        let channel = relay.channel_for(to, now_ms);
        let head = match channel {
            Some(_) => turn::CHANNEL_HEADER_LEN,
            None => turn::indication_header_len(to),
        };
        let Some(body) = out.get_mut(head..) else {
            return Some(Err(Error::BufferTooSmall));
        };
        let len = match self.session.get_output(now_ms, body)? {
            Ok(len) => len,
            Err(error) => return Some(Err(error)),
        };
        let framed = match channel {
            Some(number) => turn::wrap_channel(out, number, len),
            None => turn::wrap_indication(out, relay.indication_id(), to, len),
        };
        Some(framed.map(|len| to_relay(relay, len)))
    }
}

/// A datagram for the relay: to its address, from the local address its own
/// datagrams arrive at.
fn to_relay(relay: &Relay<'_>, len: usize) -> Egress {
    Egress {
        to: relay.server(),
        ttl: Ttl::Default,
        len,
        from: relay.local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{RecvRing, SlotMeta};
    use crate::conn::{Credentials, State};
    use crate::envelope::Envelope;
    use crate::send::{SendRing, SendSlot};
    use core::net::{IpAddr, Ipv4Addr};
    use std::vec::Vec;

    const SLOT: usize = 128;
    const SLOTS: usize = 64;
    const KEY: [u8; 32] = [0x2Bu8; 32];
    const CHANNEL: u8 = 1;

    const LEFT_UFRAG: &str = "aaaa";
    const LEFT_PWD: &str = "passwordforaaaa";
    const RIGHT_UFRAG: &str = "bbbb";
    const RIGHT_PWD: &str = "passwordforbbbb";

    struct Arena {
        recv_bodies: Vec<u8>,
        recv_meta: Vec<SlotMeta>,
        send_bodies: Vec<u8>,
        send_meta: Vec<SendSlot>,
    }

    impl Arena {
        fn new() -> Self {
            Self {
                recv_bodies: std::vec![0u8; SLOT * SLOTS],
                recv_meta: std::vec![SlotMeta::default(); SLOTS],
                send_bodies: std::vec![0u8; SLOT * SLOTS],
                send_meta: std::vec![SendSlot::default(); SLOTS],
            }
        }
    }

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, last)), port)
    }

    fn parts<'a>(
        arena: &'a mut Arena,
        ours: (&'a str, &'a str),
        theirs: (&'a str, &'a str),
        seed: u8,
    ) -> (Conn<'a>, Session<'a>) {
        let conn = Conn::new(
            Credentials {
                local_ufrag: ours.0,
                local_pwd: ours.1,
                remote_ufrag: theirs.0,
                remote_pwd: theirs.1,
            },
            [seed; 16],
            0.0,
        );
        let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
        session
            .attach_recv(
                CHANNEL,
                RecvRing::new(&mut arena.recv_bodies, &mut arena.recv_meta, SLOT).unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                CHANNEL,
                SendRing::new(&mut arena.send_bodies, &mut arena.send_meta, SLOT, CHANNEL).unwrap(),
            )
            .unwrap();
        (conn, session)
    }

    fn endpoint<'a>(
        arena: &'a mut Arena,
        ours: (&'a str, &'a str),
        theirs: (&'a str, &'a str),
        seed: u8,
    ) -> Endpoint<'a> {
        let (conn, session) = parts(arena, ours, theirs, seed);
        Endpoint::new(conn, session)
    }

    /// Move everything one side wants to send to the other, as the shell would.
    ///
    /// `local` is the address `to` receives at, handed to its input exactly as
    /// a shell reading its own socket would.
    fn pump(
        from: &mut Endpoint<'_>,
        from_addr: SocketAddr,
        to: &mut Endpoint<'_>,
        local: Option<IpAddr>,
        now: f64,
    ) -> usize {
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let mut moved = 0;
        while let Some(result) = from.get_output(now, &mut wire) {
            let egress = result.unwrap();
            to.process_input(&wire[..egress.len], from_addr, local, now, &mut scratch)
                .unwrap();
            moved += 1;
        }
        moved
    }

    /// The whole point of the facade: one object goes from punching to carrying
    /// media without the caller sequencing the two engines by hand.
    #[test]
    fn an_endpoint_punches_and_then_carries_a_message() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(
            &mut left_arena,
            (LEFT_UFRAG, LEFT_PWD),
            (RIGHT_UFRAG, RIGHT_PWD),
            0xA1,
        );
        let mut right = endpoint(
            &mut right_arena,
            (RIGHT_UFRAG, RIGHT_PWD),
            (LEFT_UFRAG, LEFT_PWD),
            0xB2,
        );

        let left_addr = addr(10, 5000);
        let right_addr = addr(20, 6000);
        left.conn()
            .add_candidate(right_addr, conn::Kind::Reflexive)
            .unwrap();
        right
            .conn()
            .add_candidate(left_addr, conn::Kind::Reflexive)
            .unwrap();
        left.conn().set_peer_ready();
        right.conn().set_peer_ready();

        // Media queued before a path exists must wait, not vanish.
        left.session()
            .send_message(CHANNEL, b"hdr", b"body")
            .unwrap();

        let mut now = 0.0;
        while now < 2_000.0 && (left.path().is_none() || right.path().is_none()) {
            pump(&mut left, left_addr, &mut right, None, now);
            pump(&mut right, right_addr, &mut left, None, now);
            now += 10.0;
            left.poll(now);
            right.poll(now);
        }

        assert_eq!(left.path(), Some(right_addr), "left found no path");
        assert_eq!(right.path(), Some(left_addr), "right found no path");

        // Now the queued message crosses, addressed to the chosen path.
        for _ in 0..8 {
            pump(&mut left, left_addr, &mut right, None, now);
            pump(&mut right, right_addr, &mut left, None, now);
            now += 10.0;
            left.poll(now);
            right.poll(now);
        }

        let mut out = [0u8; 256];
        let len = right
            .session()
            .take_message(CHANNEL, &mut out)
            .expect("no message arrived")
            .unwrap();
        assert_eq!(&out[..len], b"hdrbody");
    }

    /// Every record leaves from the address the path was proven at. The
    /// winning answer's arrival address rides the whole session, so the
    /// peer's filter keeps seeing the pair it admitted whatever the routing
    /// table would rather pick.
    #[test]
    fn media_leaves_from_the_address_the_path_was_proven_at() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(
            &mut left_arena,
            (LEFT_UFRAG, LEFT_PWD),
            (RIGHT_UFRAG, RIGHT_PWD),
            0xA1,
        );
        let mut right = endpoint(
            &mut right_arena,
            (RIGHT_UFRAG, RIGHT_PWD),
            (LEFT_UFRAG, LEFT_PWD),
            0xB2,
        );

        let left_addr = addr(10, 5000);
        let right_addr = addr(20, 6000);
        // What each side's own socket would report its arrivals at.
        let left_local = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3));
        let right_local = IpAddr::V4(Ipv4Addr::new(20, 0, 0, 7));
        left.conn()
            .add_candidate(right_addr, conn::Kind::Reflexive)
            .unwrap();
        right
            .conn()
            .add_candidate(left_addr, conn::Kind::Reflexive)
            .unwrap();
        left.conn().set_peer_ready();
        right.conn().set_peer_ready();

        let mut now = 0.0;
        while now < 2_000.0 && (left.path().is_none() || right.path().is_none()) {
            pump(&mut left, left_addr, &mut right, Some(right_local), now);
            pump(&mut right, right_addr, &mut left, Some(left_local), now);
            now += 10.0;
            left.poll(now);
            right.poll(now);
        }
        assert!(left.path().is_some() && right.path().is_some(), "no path");

        left.session()
            .send_message(CHANNEL, &[], b"pinned")
            .unwrap();
        let mut wire = [0u8; 512];
        let mut media_from = None;
        while let Some(result) = left.get_output(now, &mut wire) {
            let egress = result.unwrap();
            if demux::classify(&wire[..egress.len]) == Datagram::Record {
                media_from = Some(egress.from);
                break;
            }
        }
        assert_eq!(
            media_from,
            Some(Some(left_local)),
            "a record did not claim the address the path was proven at"
        );
    }

    /// Media has nowhere to go before a path is chosen, and must not be emitted
    /// to some default destination or silently dropped.
    #[test]
    fn nothing_media_shaped_leaves_before_a_path_exists() {
        let mut arena = Arena::new();
        let mut endpoint = endpoint(
            &mut arena,
            (LEFT_UFRAG, LEFT_PWD),
            (RIGHT_UFRAG, RIGHT_PWD),
            0xA1,
        );
        endpoint.session().send_message(CHANNEL, &[], b"x").unwrap();

        // No candidate, so connectivity has nothing to emit either.
        let mut wire = [0u8; 512];
        assert!(endpoint.get_output(0.0, &mut wire).is_none());
        assert_eq!(endpoint.path(), None);
    }

    /// A shell arming from one engine alone gets the wrong answer in both
    /// directions, which is why the minimum is taken here rather than there.
    #[test]
    fn the_timer_is_the_sooner_of_the_two() {
        let mut arena = Arena::new();
        let mut endpoint = endpoint(
            &mut arena,
            (LEFT_UFRAG, LEFT_PWD),
            (RIGHT_UFRAG, RIGHT_PWD),
            0xA1,
        );

        // A fresh candidate is due immediately once the peer says it is
        // listening, well inside the acknowledgement cadence, so connectivity
        // sets the deadline.
        endpoint
            .conn()
            .add_candidate(addr(20, 6000), conn::Kind::Reflexive)
            .unwrap();
        endpoint.conn().set_peer_ready();
        assert!(endpoint.next_timer_ms(0.0).abs() < 1e-9);

        // Once the attempt is over it asks for nothing, and the session's
        // cadence is all that remains. An endpoint that kept the connectivity
        // timer here would poll forever.
        endpoint.poll(conn::PUNCH_WINDOW_MS);
        assert!(matches!(endpoint.conn().state(), State::Failed(_)));
        let timer = endpoint.next_timer_ms(conn::PUNCH_WINDOW_MS);
        assert!(
            timer.is_finite() && timer <= session::ACK_CADENCE_MS,
            "expected the session cadence, got {timer}"
        );
    }

    /// Classification decides which engine sees a datagram, and a record must
    /// never reach the check parser however it is shaped.
    #[test]
    fn a_record_and_a_check_reach_different_engines() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(
            &mut left_arena,
            (LEFT_UFRAG, LEFT_PWD),
            (RIGHT_UFRAG, RIGHT_PWD),
            0xA1,
        );
        let mut right = endpoint(
            &mut right_arena,
            (RIGHT_UFRAG, RIGHT_PWD),
            (LEFT_UFRAG, LEFT_PWD),
            0xB2,
        );

        let left_addr = addr(10, 5000);
        left.conn()
            .add_candidate(addr(20, 6000), conn::Kind::Reflexive)
            .unwrap();

        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let egress = left.get_output(0.0, &mut wire).unwrap().unwrap();
        assert!(matches!(
            right
                .process_input(&wire[..egress.len], left_addr, None, 0.0, &mut scratch)
                .unwrap(),
            Inbound::Connectivity(_)
        ));

        // And a sealed record classifies the other way.
        let mut left2_arena = Arena::new();
        let mut solo = endpoint(
            &mut left2_arena,
            (LEFT_UFRAG, LEFT_PWD),
            (RIGHT_UFRAG, RIGHT_PWD),
            0xC3,
        );
        solo.session().send_message(CHANNEL, &[], b"x").unwrap();
        let len = solo.session().get_output(0.0, &mut wire).unwrap().unwrap();
        assert!(matches!(
            right
                .process_input(&wire[..len], left_addr, None, 0.0, &mut scratch)
                .unwrap(),
            Inbound::Media(_)
        ));
    }

    mod relayed {
        use super::*;
        use crate::conn::Kind;
        use crate::turn::Key;
        use crate::turn::testing::{
            allocated, challenge, channel_data, channel_of, granted, kind_of, peer_of,
            relayed as relayed_datagram, signed, unwrapped,
        };

        const USER: &str = "user";
        const PASS: &str = "password";
        const REALM: &[u8] = b"relay.example";
        const NONCE: &[u8] = b"5d1b0a4f3c2e7a90";

        fn server() -> SocketAddr {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)), 3478)
        }

        /// The relayed address: the relay's own machine.
        fn relayed_addr() -> SocketAddr {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 50_048)
        }

        /// The host, on the relay's machine.
        fn host_addr() -> SocketAddr {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 22_974)
        }

        /// The client, relayed; the host, direct and none the wiser.
        fn client<'a>(arena: &'a mut Arena) -> Endpoint<'a> {
            let (conn, session) = parts(
                arena,
                (LEFT_UFRAG, LEFT_PWD),
                (RIGHT_UFRAG, RIGHT_PWD),
                0xA1,
            );
            Endpoint::relayed(
                conn,
                session,
                Relay::new(server(), USER, PASS, [0x5E; 16], 0.0),
            )
        }

        fn host<'a>(arena: &'a mut Arena) -> Endpoint<'a> {
            endpoint(
                arena,
                (RIGHT_UFRAG, RIGHT_PWD),
                (LEFT_UFRAG, LEFT_PWD),
                0xB2,
            )
        }

        enum Carried {
            Answer(Vec<u8>),
            ToPeer(SocketAddr, Vec<u8>),
            Dropped,
        }

        /// A relay as far as these tests need one: every request answered, and
        /// datagrams carried both ways as a relay frames them.
        struct FakeRelay {
            key: Key,
            permitted: Vec<IpAddr>,
            channels: Vec<(u16, SocketAddr)>,
            /// Every peer a datagram was carried to, and whether it was media.
            carried: Vec<(SocketAddr, bool)>,
            /// Every channel data message the client sent.
            channel_messages: usize,
        }

        impl FakeRelay {
            fn new() -> Self {
                Self {
                    key: Key::long_term(USER, REALM, PASS),
                    permitted: Vec::new(),
                    channels: Vec::new(),
                    carried: Vec::new(),
                    channel_messages: 0,
                }
            }

            fn carry(&mut self, sent: &[u8]) -> Carried {
                if sent[0] >> 6 == 0b01 {
                    self.channel_messages += 1;
                    let number = u16::from_be_bytes([sent[0], sent[1]]);
                    return match self.channels.iter().find(|(bound, _)| *bound == number) {
                        Some((_, peer)) => Carried::ToPeer(*peer, unwrapped(sent).1),
                        None => Carried::Dropped,
                    };
                }
                match kind_of(sent) {
                    0x0003 if !signed(sent) => Carried::Answer(challenge(sent, 401, REALM, NONCE)),
                    0x0003 => {
                        Carried::Answer(allocated(sent, relayed_addr(), Some(600), &self.key))
                    }
                    0x0004 => Carried::Answer(granted(sent, Some(600), &self.key)),
                    0x0008 => {
                        self.permitted.push(peer_of(sent).unwrap().ip());
                        Carried::Answer(granted(sent, None, &self.key))
                    }
                    0x0009 => {
                        let peer = peer_of(sent).unwrap();
                        self.channels.push((channel_of(sent).unwrap(), peer));
                        self.permitted.push(peer.ip());
                        Carried::Answer(granted(sent, None, &self.key))
                    }
                    0x0016 => {
                        let (peer, data) = unwrapped(sent);
                        let peer = peer.unwrap();
                        if self.permitted.contains(&peer.ip()) {
                            Carried::ToPeer(peer, data)
                        } else {
                            Carried::Dropped
                        }
                    }
                    other => panic!("the relay was sent {other:#06x}"),
                }
            }

            /// A peer's datagram for the client, framed as a relay frames it:
            /// on a channel once one is bound to the peer. Dropped without a
            /// permission, as a relay drops it.
            fn to_client(&self, from: SocketAddr, data: &[u8]) -> Option<Vec<u8>> {
                if !self.permitted.contains(&from.ip()) {
                    return None;
                }
                Some(match self.channels.iter().find(|(_, peer)| *peer == from) {
                    Some((number, _)) => channel_data(*number, data),
                    None => relayed_datagram(from, data),
                })
            }
        }

        /// One pass through the relay in both directions. Everything the
        /// client sends must go to the relay; the host's datagrams reach the
        /// client as the relay frames them, seen from `host_seen`.
        fn through(
            client: &mut Endpoint<'_>,
            host: &mut Endpoint<'_>,
            relay: &mut FakeRelay,
            host_seen: SocketAddr,
            now: f64,
        ) {
            let mut wire = [0u8; 2048];
            let mut scratch = [0u8; 2048];
            while let Some(result) = client.get_output(now, &mut wire) {
                let egress = result.unwrap();
                assert_eq!(
                    egress.to,
                    server(),
                    "a relay attempt sent outside the relay"
                );
                match relay.carry(&wire[..egress.len]) {
                    Carried::Answer(answer) => {
                        client
                            .process_input(&answer, server(), None, now, &mut scratch)
                            .unwrap();
                    }
                    Carried::ToPeer(peer, data) => {
                        relay
                            .carried
                            .push((peer, demux::classify(&data) == Datagram::Record));
                        let _ = host.process_input(&data, relayed_addr(), None, now, &mut scratch);
                    }
                    Carried::Dropped => {}
                }
            }
            while let Some(result) = host.get_output(now, &mut wire) {
                let egress = result.unwrap();
                if let Some(framed) = relay.to_client(host_seen, &wire[..egress.len]) {
                    let _ = client.process_input(&framed, server(), None, now, &mut scratch);
                }
            }
        }

        /// Run until the relayed address may be offered, then hand it to the
        /// host as signaling would, with the readiness marker after it.
        fn offer(client: &mut Endpoint<'_>, host: &mut Endpoint<'_>, relay: &mut FakeRelay) -> f64 {
            let mut now = 0.0;
            while client.relay().unwrap().relayed().is_none() {
                assert!(now < 1_000.0, "the relay never became ready");
                through(client, host, relay, host_addr(), now);
                now += 10.0;
            }
            host.conn()
                .add_candidate(client.relay().unwrap().relayed().unwrap(), Kind::Reflexive)
                .unwrap();
            host.conn().set_peer_ready();
            client.conn().set_peer_ready();
            now
        }

        /// Punch through the relay: both sides find a path.
        fn punch(
            client: &mut Endpoint<'_>,
            host: &mut Endpoint<'_>,
            relay: &mut FakeRelay,
            mut now: f64,
        ) -> f64 {
            while client.path().is_none() || host.path().is_none() {
                assert!(now < 5_000.0, "no path through the relay");
                through(client, host, relay, host_addr(), now);
                now += 10.0;
                client.poll(now);
                host.poll(now);
            }
            now
        }

        /// The whole of a relay attempt against a host that knows nothing of
        /// it: the host is handed one ordinary candidate, and a message
        /// crosses each way.
        #[test]
        fn a_relay_attempt_punches_and_carries_a_message_through_the_relay() {
            let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
            let mut client = client(&mut client_arena);
            let mut host = host(&mut host_arena);
            let mut relay = FakeRelay::new();
            client.add_candidate(host_addr(), Kind::Direct).unwrap();

            let now = offer(&mut client, &mut host, &mut relay);
            let mut now = punch(&mut client, &mut host, &mut relay, now);
            assert_eq!(client.path(), Some(host_addr()));
            assert_eq!(host.path(), Some(relayed_addr()));

            client
                .session()
                .send_message(CHANNEL, b"to", b"host")
                .unwrap();
            host.session()
                .send_message(CHANNEL, b"to", b"client")
                .unwrap();
            for _ in 0..8 {
                through(&mut client, &mut host, &mut relay, host_addr(), now);
                now += 10.0;
            }
            let mut out = [0u8; 64];
            let len = host
                .session()
                .take_message(CHANNEL, &mut out)
                .unwrap()
                .unwrap();
            assert_eq!(&out[..len], b"tohost");
            let len = client
                .session()
                .take_message(CHANNEL, &mut out)
                .unwrap()
                .unwrap();
            assert_eq!(&out[..len], b"toclient");
        }

        /// Before the relayed address may be offered, nothing goes toward a
        /// peer, whatever candidates have arrived and however long the relay
        /// takes to answer: only the allocation and then the relay's own
        /// machine's permission, in that order.
        #[test]
        fn nothing_leaves_toward_a_peer_before_the_relay_is_ready() {
            let mut arena = Arena::new();
            let mut client = client(&mut arena);
            client.add_candidate(host_addr(), Kind::Direct).unwrap();
            client.conn().set_peer_ready();
            let mut relay = FakeRelay::new();
            let mut wire = [0u8; 2048];
            let mut scratch = [0u8; 2048];

            // Everything sent at `now`, each kept for an answer given later.
            let mut drain = |client: &mut Endpoint<'_>, now: f64| {
                let mut sent = Vec::new();
                while let Some(result) = client.get_output(now, &mut wire) {
                    sent.push(wire[..result.unwrap().len].to_vec());
                }
                sent
            };
            let mut answer = |client: &mut Endpoint<'_>, sent: &[u8], now: f64| {
                let Carried::Answer(answer) = relay.carry(sent) else {
                    panic!("not a request");
                };
                client
                    .process_input(&answer, server(), None, now, &mut scratch)
                    .unwrap();
            };

            let first = drain(&mut client, 0.0);
            assert_eq!(
                first.iter().map(|sent| kind_of(sent)).collect::<Vec<_>>(),
                [0x0003]
            );
            answer(&mut client, &first[0], 30.0);
            let second = drain(&mut client, 30.0);
            assert_eq!(
                second.iter().map(|sent| kind_of(sent)).collect::<Vec<_>>(),
                [0x0003]
            );
            answer(&mut client, &second[0], 60.0);
            let third = drain(&mut client, 60.0);
            assert_eq!(
                third.iter().map(|sent| kind_of(sent)).collect::<Vec<_>>(),
                [0x0008]
            );

            // The permission goes unanswered past the check cadence, and still
            // nothing goes toward the host.
            assert!(drain(&mut client, 60.0 + conn::CHECK_CADENCE_MS + 100.0).is_empty());
            answer(&mut client, &third[0], 700.0);
            let checks = drain(&mut client, 700.0);
            assert_eq!(
                checks.iter().map(|sent| kind_of(sent)).collect::<Vec<_>>(),
                [0x0016]
            );
            assert_eq!(unwrapped(&checks[0]).0, Some(host_addr()));
        }

        /// A check that came through the relay is answered through the relay.
        /// Answered from the socket straight to the host's address, it goes
        /// where the host cannot be reached, and a host whose checks go
        /// unanswered withholds media.
        #[test]
        fn a_check_through_the_relay_is_answered_through_the_relay() {
            let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
            let mut client = client(&mut client_arena);
            let mut host = host(&mut host_arena);
            let mut relay = FakeRelay::new();
            let now = offer(&mut client, &mut host, &mut relay);

            let mut wire = [0u8; 2048];
            let mut scratch = [0u8; 2048];
            let check = host.get_output(now, &mut wire).unwrap().unwrap();
            let framed = relay.to_client(host_addr(), &wire[..check.len]).unwrap();
            assert!(matches!(
                client
                    .process_input(&framed, server(), None, now, &mut scratch)
                    .unwrap(),
                Inbound::Connectivity(conn::Inbound::CheckAnswered)
            ));

            let answer = client.get_output(now, &mut wire).unwrap().unwrap();
            assert_eq!(answer.to, server());
            let (peer, data) = unwrapped(&wire[..answer.len]);
            assert_eq!(peer, Some(host_addr()));
            assert_eq!(&data[..2], &[0x01, 0x01], "not a check's answer");
        }

        /// A deployed relay destroys the allocation that sends toward
        /// loopback. A loopback candidate is not permitted and never checked,
        /// and a datagram the relay claims came from loopback is not taken.
        #[test]
        fn a_loopback_candidate_is_never_relayed() {
            let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
            let mut client = client(&mut client_arena);
            let mut host = host(&mut host_arena);
            let mut relay = FakeRelay::new();
            let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 22_974);
            client.add_candidate(loopback, Kind::Direct).unwrap();
            client.add_candidate(host_addr(), Kind::Direct).unwrap();
            assert_eq!(client.conn().candidate_count(), 1, "loopback was kept");

            let now = offer(&mut client, &mut host, &mut relay);
            let now = punch(&mut client, &mut host, &mut relay, now);
            assert!(
                relay.permitted.iter().all(|ip| !ip.is_loopback()),
                "loopback permitted"
            );
            assert!(
                relay
                    .carried
                    .iter()
                    .all(|(peer, _)| !peer.ip().is_loopback()),
                "relayed toward loopback"
            );

            // A relay that relays from loopback anyway is not believed.
            relay.permitted.push(loopback.ip());
            let mut wire = [0u8; 2048];
            let mut scratch = [0u8; 2048];
            host.conn()
                .add_candidate(addr(99, 9), Kind::Direct)
                .unwrap();
            let check = host.get_output(now, &mut wire).unwrap().unwrap();
            let framed = relay.to_client(loopback, &wire[..check.len]).unwrap();
            assert!(
                client
                    .process_input(&framed, server(), None, now, &mut scratch)
                    .is_err()
            );
            assert_eq!(client.conn().candidate_count(), 1);
        }

        /// Media goes where the host's authenticated traffic comes from. A
        /// host behind a translator on the relay's network is checked at one
        /// address and speaks from another, and media sent to the first goes
        /// nowhere.
        #[test]
        fn the_path_follows_the_host() {
            let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
            let mut client = client(&mut client_arena);
            let mut host = host(&mut host_arena);
            let mut relay = FakeRelay::new();
            let translated = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 40_000);
            client.add_candidate(host_addr(), Kind::Direct).unwrap();
            client.add_candidate(translated, Kind::Direct).unwrap();

            let now = offer(&mut client, &mut host, &mut relay);
            let mut now = punch(&mut client, &mut host, &mut relay, now);
            assert_eq!(client.path(), Some(host_addr()));

            host.session()
                .send_message(CHANNEL, &[], b"from elsewhere")
                .unwrap();
            for _ in 0..4 {
                through(&mut client, &mut host, &mut relay, translated, now);
                now += 10.0;
            }
            relay.carried.clear();
            client
                .session()
                .send_message(CHANNEL, &[], b"after it")
                .unwrap();
            for _ in 0..4 {
                through(&mut client, &mut host, &mut relay, translated, now);
                now += 10.0;
            }
            let media: Vec<_> = relay.carried.iter().filter(|(_, media)| *media).collect();
            assert!(!media.is_empty(), "no media went");
            assert!(
                media.iter().all(|(peer, _)| *peer == translated),
                "media did not follow: {media:?}"
            );
        }

        /// Media rides a channel once one is bound to where it goes: four
        /// bytes of framing rather than thirty-six. Until the binding is
        /// answered it goes as indications, and nothing waits for it.
        #[test]
        fn media_rides_a_channel_once_bound() {
            let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
            let mut client = client(&mut client_arena);
            let mut host = host(&mut host_arena);
            let mut relay = FakeRelay::new();
            client.add_candidate(host_addr(), Kind::Direct).unwrap();
            let now = offer(&mut client, &mut host, &mut relay);
            let mut now = punch(&mut client, &mut host, &mut relay, now);
            for _ in 0..4 {
                through(&mut client, &mut host, &mut relay, host_addr(), now);
                now += 10.0;
            }
            assert_eq!(relay.channels, [(turn::FIRST_CHANNEL, host_addr())]);

            let before = relay.channel_messages;
            client
                .session()
                .send_message(CHANNEL, &[], b"on the channel")
                .unwrap();
            let mut wire = [0u8; 2048];
            let mut sent = None;
            while let Some(result) = client.get_output(now, &mut wire) {
                let egress = result.unwrap();
                if wire[0] >> 6 == 0b01 {
                    sent = Some(egress.len);
                }
                let _ = relay.carry(&wire[..egress.len]);
            }
            let len = sent.expect("no channel data");
            assert!(relay.channel_messages > before);
            let (_, data) = unwrapped(&wire[..len]);
            assert_eq!(len, turn::CHANNEL_HEADER_LEN + data.len());
        }

        /// A relay attempt talks to the relay alone. A datagram from anywhere
        /// else is not taken, however it is shaped -- not even one framed
        /// exactly as the relay frames what it relays.
        #[test]
        fn a_relay_attempt_takes_nothing_from_outside_the_relay() {
            let (mut client_arena, mut host_arena) = (Arena::new(), Arena::new());
            let mut client = client(&mut client_arena);
            let mut host = host(&mut host_arena);
            let mut relay = FakeRelay::new();
            let now = offer(&mut client, &mut host, &mut relay);

            let mut wire = [0u8; 2048];
            let mut scratch = [0u8; 2048];
            let check = host.get_output(now, &mut wire).unwrap().unwrap();
            let check = wire[..check.len].to_vec();
            let framed = relay.to_client(host_addr(), &check).unwrap();
            let stranger = addr(66, 3478);
            for datagram in [&check, &framed] {
                assert_eq!(
                    client.process_input(datagram, stranger, None, now, &mut scratch),
                    Err(Error::Malformed)
                );
            }
            assert_eq!(client.conn().candidate_count(), 0);
            // The same framing from the relay is taken.
            assert!(
                client
                    .process_input(&framed, server(), None, now, &mut scratch)
                    .is_ok()
            );
            assert_eq!(client.conn().candidate_count(), 1);
        }
    }
}
