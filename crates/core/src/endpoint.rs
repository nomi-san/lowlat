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
//!     for each datagram:  endpoint.process_input(bytes, from, now, scratch)
//!     endpoint.poll(now)
//!     drain:              while let Some(e) = endpoint.get_output(now, buf) { send(e) }
//! ```
//!
//! An output carries where it goes and how it must be sent, because a mapping
//! probe leaves at a TTL that must be restored afterwards and a shell cannot be
//! trusted to remember an obligation that is not in the type.

use core::net::SocketAddr;

use crate::conn::{self, Conn, Egress, Ttl};
use crate::demux::{self, Datagram};
use crate::error::Result;
use crate::session::{self, Health, Session};

/// What an inbound datagram turned out to be.
///
/// The two engines keep their own vocabularies; nothing is gained by flattening
/// them into one enum that half the callers would have to ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inbound {
    /// A connectivity check or its answer.
    Connectivity(conn::Inbound),
    /// An encrypted record.
    Media(session::Inbound),
}

/// A peer: the punch and the session it hands over to.
#[derive(Debug)]
pub struct Endpoint<'a> {
    conn: Conn<'a>,
    session: Session<'a>,
}

impl<'a> Endpoint<'a> {
    /// Pair a connectivity attempt with the session that will use its path.
    ///
    /// Both are built by the caller, because the session needs ring storage and
    /// key material that arrive from different places at different times.
    pub fn new(conn: Conn<'a>, session: Session<'a>) -> Self {
        Self { conn, session }
    }

    /// The connectivity engine, for candidates and outcome.
    pub fn conn(&mut self) -> &mut Conn<'a> {
        &mut self.conn
    }

    /// The session, for messages.
    pub fn session(&mut self) -> &mut Session<'a> {
        &mut self.session
    }

    /// The chosen path, once there is one. Media flows only after this.
    pub fn path(&self) -> Option<SocketAddr> {
        self.conn.path()
    }

    /// Liveness of the media session.
    pub fn health(&self, now_ms: f64) -> Health {
        self.session.health(now_ms)
    }

    /// Feed one received datagram, whatever it is.
    ///
    /// Classification happens here, on the first two bytes, before either
    /// engine sees the bytes. Anything not shaped like a check goes to the
    /// record layer, where authentication rejects it, so the check parser is
    /// never handed input that was not already check-shaped.
    pub fn process_input(
        &mut self,
        datagram: &[u8],
        from: SocketAddr,
        now_ms: f64,
        scratch: &mut [u8],
    ) -> Result<Inbound> {
        match demux::classify(datagram) {
            Datagram::Check => Ok(Inbound::Connectivity(
                self.conn.process_input(datagram, from)?,
            )),
            Datagram::Record => Ok(Inbound::Media(
                self.session.process_input(datagram, now_ms, scratch)?,
            )),
        }
    }

    /// Housekeeping for both engines.
    pub fn poll(&mut self, now_ms: f64) {
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
        self.conn
            .next_timer_ms(now_ms)
            .min(self.session.next_timer_ms(now_ms))
    }

    /// Emit the next datagram, with where it goes and how to send it.
    ///
    /// Connectivity drains first. Its datagrams are small, time critical, and
    /// owed to a peer that reads silence as unreachable; and until a path
    /// exists there is nowhere to send media anyway.
    pub fn get_output(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<Egress>> {
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
            }),
            Err(error) => Err(error),
        })
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

    fn endpoint<'a>(
        arena: &'a mut Arena,
        ours: (&'a str, &'a str),
        theirs: (&'a str, &'a str),
        seed: u8,
    ) -> Endpoint<'a> {
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
        Endpoint::new(conn, session)
    }

    /// Move everything one side wants to send to the other, as the shell would.
    fn pump(
        from: &mut Endpoint<'_>,
        from_addr: SocketAddr,
        to: &mut Endpoint<'_>,
        now: f64,
    ) -> usize {
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let mut moved = 0;
        while let Some(result) = from.get_output(now, &mut wire) {
            let egress = result.unwrap();
            to.process_input(&wire[..egress.len], from_addr, now, &mut scratch)
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
        left.conn().add_candidate(right_addr).unwrap();
        right.conn().add_candidate(left_addr).unwrap();

        // Media queued before a path exists must wait, not vanish.
        left.session()
            .send_message(CHANNEL, b"hdr", b"body")
            .unwrap();

        let mut now = 0.0;
        while now < 2_000.0 && (left.path().is_none() || right.path().is_none()) {
            pump(&mut left, left_addr, &mut right, now);
            pump(&mut right, right_addr, &mut left, now);
            now += 10.0;
            left.poll(now);
            right.poll(now);
        }

        assert_eq!(left.path(), Some(right_addr), "left found no path");
        assert_eq!(right.path(), Some(left_addr), "right found no path");

        // Now the queued message crosses, addressed to the chosen path.
        for _ in 0..8 {
            pump(&mut left, left_addr, &mut right, now);
            pump(&mut right, right_addr, &mut left, now);
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

        // A fresh candidate is due immediately, well inside the acknowledgement
        // cadence, so connectivity sets the deadline.
        endpoint.conn().add_candidate(addr(20, 6000)).unwrap();
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
        left.conn().add_candidate(addr(20, 6000)).unwrap();

        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let egress = left.get_output(0.0, &mut wire).unwrap().unwrap();
        assert!(matches!(
            right
                .process_input(&wire[..egress.len], left_addr, 0.0, &mut scratch)
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
                .process_input(&wire[..len], left_addr, 0.0, &mut scratch)
                .unwrap(),
            Inbound::Media(_)
        ));
    }
}
