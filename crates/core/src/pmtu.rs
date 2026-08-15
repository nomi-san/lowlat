//! Path probing (docs/01-protocol.md 8).
//!
//! The datagram size is **not negotiated and cannot be**. No field in
//! signaling or on the wire carries it, and an endpoint's configured size
//! bounds only what that endpoint emits. So peer capacity is unknowable in
//! advance and the only sound way to use headroom is to probe for it.
//!
//! One mechanism covers two unknowns. A probe that goes unacknowledged while
//! smaller packets are acknowledged is indistinguishable from a peer whose
//! receive buffer is smaller than the path allows, and the correct response is
//! identical in both cases: stop here and keep what works.

use crate::envelope::ENVELOPE_LEN;
use crate::packet::HEADER_LEN;
use crate::seq;

/// Default and floor: 1200 of cleartext plus the envelope.
pub const FLOOR: usize = crate::DEFAULT_DATAGRAM;

/// Absolute ceiling. **Never emit above this**, probe or not: a peer that
/// cannot accept it discards the whole datagram rather than truncating, so the
/// failure is total and silent, and its reassembly copy is bounded only by its
/// receive buffer.
pub const CEILING: usize = crate::MAX_DATAGRAM;

/// Largest datagram that fits a 1500-byte path without fragmenting.
pub const DIRECT_CLAMP: usize = 1472;

/// Rungs, in the order they are attempted.
pub const LADDER: [usize; 3] = [1280, 1350, 1400];

/// Bytes a relay adds ahead of our datagram.
const RELAY_INDICATION_OVERHEAD: usize = 36;
const RELAY_CHANNEL_OVERHEAD: usize = 4;

/// How the session currently reaches its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Direct,
    /// Relayed, framed as a data indication.
    RelayIndication,
    /// Relayed, framed as channel data.
    RelayChannel,
}

impl Path {
    const fn overhead(self) -> usize {
        match self {
            Path::Direct => 0,
            Path::RelayIndication => RELAY_INDICATION_OVERHEAD,
            Path::RelayChannel => RELAY_CHANNEL_OVERHEAD,
        }
    }
}

/// Probe state for one path.
#[derive(Debug, Clone, Copy)]
pub struct PathMtu {
    current: usize,
    clamp: usize,
    rung: usize,
    /// The fragment carrying the in-flight probe, if any.
    probe: Option<(u8, u32, usize)>,
    settled: bool,
}

impl PathMtu {
    /// Start at the floor on `path`.
    pub fn new(path: Path) -> Self {
        Self {
            current: FLOOR,
            clamp: Self::clamp_for(path),
            rung: 0,
            probe: None,
            settled: false,
        }
    }

    fn clamp_for(path: Path) -> usize {
        DIRECT_CLAMP.saturating_sub(path.overhead()).min(CEILING)
    }

    /// Datagram size currently in use.
    pub fn datagram_size(&self) -> usize {
        self.current
    }

    /// Bytes of message body one fragment can carry at the current size.
    pub fn body_capacity(&self) -> usize {
        self.current.saturating_sub(ENVELOPE_LEN + HEADER_LEN)
    }

    /// True once probing has stopped, either by exhausting the ladder or by a
    /// probe failing.
    pub fn settled(&self) -> bool {
        self.settled
    }

    /// Size of the next probe, or `None` if there is nothing left to try.
    ///
    /// Rungs at or below the current size, or above the clamp, are skipped
    /// rather than attempted: probing downward proves nothing and probing past
    /// the clamp fragments.
    pub fn next_probe_size(&self) -> Option<usize> {
        if self.settled || self.probe.is_some() {
            return None;
        }
        let mut rung = self.rung;
        while let Some(&size) = LADDER.get(rung) {
            if size > self.current && size <= self.clamp {
                return Some(size);
            }
            rung += 1;
        }
        None
    }

    /// Record that a probe of `size` went out as `channel`/`sequence`.
    pub fn on_probe_sent(&mut self, channel: u8, sequence: u32, size: usize) {
        if size > self.clamp || size > CEILING {
            return;
        }
        self.probe = Some((channel, sequence, size));
    }

    /// Apply an acknowledgement. Returns true if it confirmed a probe.
    ///
    /// Confirmation is cumulative: the probe's fragment being acknowledged
    /// means the peer received a datagram of that size intact.
    pub fn on_ack(&mut self, channel: u8, cumulative: u32) -> bool {
        let Some((probe_channel, probe_seq, size)) = self.probe else {
            return false;
        };
        if probe_channel != channel || !seq::gt(cumulative, probe_seq) {
            return false;
        }
        self.probe = None;
        self.current = size.min(self.clamp).min(CEILING);
        self.rung += 1;
        if self.next_probe_size().is_none() {
            self.settled = true;
        }
        true
    }

    /// The in-flight probe failed. Keep what works and stop climbing.
    pub fn on_probe_lost(&mut self) {
        if self.probe.take().is_some() {
            self.settled = true;
        }
    }

    /// The path changed. Everything learned about the old one is void.
    pub fn reset(&mut self, path: Path) {
        *self = Self::new(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_the_floor() {
        let mtu = PathMtu::new(Path::Direct);
        assert_eq!(mtu.datagram_size(), FLOOR);
        assert_eq!(mtu.body_capacity(), 1193);
        assert!(!mtu.settled());
    }

    #[test]
    fn climbs_the_ladder_on_success() {
        let mut mtu = PathMtu::new(Path::Direct);
        for (index, &expected) in LADDER.iter().enumerate() {
            assert_eq!(mtu.next_probe_size(), Some(expected), "rung {index}");
            mtu.on_probe_sent(1, index as u32, expected);
            assert!(mtu.on_ack(1, index as u32 + 1));
            assert_eq!(mtu.datagram_size(), expected);
        }
        assert!(mtu.settled(), "ladder exhausted without settling");
        assert_eq!(mtu.next_probe_size(), None);
    }

    #[test]
    fn a_failed_probe_settles_at_the_last_good_size() {
        let mut mtu = PathMtu::new(Path::Direct);
        mtu.on_probe_sent(1, 0, 1280);
        assert!(mtu.on_ack(1, 1));
        assert_eq!(mtu.datagram_size(), 1280);

        mtu.on_probe_sent(1, 5, 1350);
        mtu.on_probe_lost();
        assert!(mtu.settled());
        assert_eq!(mtu.datagram_size(), 1280, "lost the size that worked");
        assert_eq!(mtu.next_probe_size(), None);
    }

    /// An acknowledgement that does not cover the probe proves nothing.
    #[test]
    fn an_unrelated_acknowledgement_does_not_confirm() {
        let mut mtu = PathMtu::new(Path::Direct);
        mtu.on_probe_sent(1, 10, 1280);
        assert!(!mtu.on_ack(2, 11), "wrong channel confirmed a probe");
        assert!(!mtu.on_ack(1, 10), "an ack below the probe confirmed it");
        assert_eq!(mtu.datagram_size(), FLOOR);
        assert!(mtu.on_ack(1, 11));
    }

    #[test]
    fn a_relay_reduces_the_clamp() {
        let direct = PathMtu::new(Path::Direct);
        let indication = PathMtu::new(Path::RelayIndication);
        let channel = PathMtu::new(Path::RelayChannel);
        assert_eq!(direct.clamp, DIRECT_CLAMP);
        assert_eq!(indication.clamp, DIRECT_CLAMP - 36);
        assert_eq!(channel.clamp, DIRECT_CLAMP - 4);
    }

    /// The clamp must actually stop the ladder, not merely be recorded.
    #[test]
    fn rungs_above_the_clamp_are_never_attempted() {
        let mut mtu = PathMtu::new(Path::Direct);
        mtu.clamp = 1300;
        assert_eq!(mtu.next_probe_size(), Some(1280));
        mtu.on_probe_sent(1, 0, 1280);
        mtu.on_ack(1, 1);
        assert_eq!(mtu.next_probe_size(), None, "probed past the clamp");
        assert!(mtu.settled());
    }

    #[test]
    fn nothing_can_be_adopted_above_the_ceiling() {
        let mut mtu = PathMtu::new(Path::Direct);
        mtu.clamp = CEILING + 500;
        mtu.on_probe_sent(1, 0, CEILING + 400);
        // The probe is refused outright, so no acknowledgement can adopt it.
        assert!(!mtu.on_ack(1, 1));
        assert!(mtu.datagram_size() <= CEILING);
    }

    #[test]
    fn a_path_change_forgets_everything() {
        let mut mtu = PathMtu::new(Path::Direct);
        mtu.on_probe_sent(1, 0, 1400);
        mtu.on_ack(1, 1);
        assert_eq!(mtu.datagram_size(), 1400);

        mtu.reset(Path::RelayIndication);
        assert_eq!(mtu.datagram_size(), FLOOR);
        assert!(!mtu.settled());
        assert_eq!(mtu.clamp, DIRECT_CLAMP - 36);
    }

    #[test]
    fn only_one_probe_is_in_flight_at_a_time() {
        let mut mtu = PathMtu::new(Path::Direct);
        assert!(mtu.next_probe_size().is_some());
        mtu.on_probe_sent(1, 0, 1280);
        assert_eq!(mtu.next_probe_size(), None, "started a second probe");
    }

    #[test]
    fn body_capacity_tracks_the_datagram_size() {
        let mut mtu = PathMtu::new(Path::Direct);
        mtu.on_probe_sent(1, 0, 1400);
        mtu.on_ack(1, 1);
        assert_eq!(mtu.body_capacity(), 1400 - ENVELOPE_LEN - HEADER_LEN);
        assert_eq!(mtu.body_capacity(), 1364);
    }
}
