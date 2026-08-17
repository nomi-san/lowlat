//! Which guests receive an encoded frame.
//!
//! One encode serves every guest (D11), so delivery is decided per guest after
//! the frame exists rather than by encoding differently for each. See
//! docs/05-host.md section 6.
//!
//! **The cascade is the invariant.** A guest that misses one frame must miss
//! every frame until the next keyframe. Dropping a single dependent frame
//! breaks the reference chain silently: the decoder keeps going and produces
//! progressively wrong output rather than failing. That is the gray-frame
//! failure, and it is why skipping latches here rather than the caller being
//! trusted to remember.
//!
//! **The wrong thing is unsayable.** There is no operation that withholds one
//! frame without marking the guest pending, and a caller cannot ask which
//! guests were withheld from -- it is told only which ones take the frame. A
//! gate that exposed the other half would eventually have it called.

use lowlat_common::clock::{Time, diff_ms};

/// The compile-time cap from docs/00-overview.md D10. Ring memory scales per
/// guest, so this bounds it.
pub const MAX_GUESTS: usize = 16;

/// Roughly twice a second, which is the throttle from section 6.
const KEYFRAME_INTERVAL_MS: f64 = 500.0;

/// The absolute ceiling on outstanding fragments for a configured rate.
///
/// **A ceiling, not a proportional margin.** A margin such as "free slots must
/// exceed twice the frame" is wrong in both directions: it refuses frames at
/// low occupancy on a deep window, and admits them when the window is nearly
/// full because the room left still happens to be twice a small frame. The top
/// step is the peer's ring depth, so the highest rate may fill the peer's ring
/// and no rate may go past it.
pub fn ceiling(rate_mbps: f32) -> u32 {
    if rate_mbps >= 30.0 {
        4000
    } else if rate_mbps >= 20.0 {
        2500
    } else {
        1500
    }
}

/// One guest's delivery state.
#[derive(Debug, Clone, Copy)]
pub struct Guest {
    ceiling: u32,
    /// Outstanding fragments on this guest's path, refreshed by the caller
    /// before each pass. The transport owns it; the gate only reads it.
    outstanding: u32,
    /// **Latched.** Cleared in exactly one place: taking a keyframe.
    pending_keyframe: bool,
}

impl Guest {
    /// A guest that has just joined.
    ///
    /// **Starts pending**, which is what produces its join keyframe. Nothing
    /// separate arranges one: a guest that has received nothing is in the same
    /// position as a guest that has fallen out of the reference chain, so it
    /// is the same state.
    pub fn joining(rate_mbps: f32) -> Self {
        Self {
            ceiling: ceiling(rate_mbps),
            outstanding: 0,
            pending_keyframe: true,
        }
    }

    /// Refresh what the transport says is in flight.
    pub fn set_outstanding(&mut self, fragments: u32) {
        self.outstanding = fragments;
    }

    /// True while this guest is waiting for a keyframe and receiving nothing.
    pub fn is_skipping(&self) -> bool {
        self.pending_keyframe
    }

    /// The configured rate changed, so the ceiling moves with it.
    pub fn set_rate(&mut self, rate_mbps: f32) {
        self.ceiling = ceiling(rate_mbps);
    }

    fn has_room_for(&self, fragments: u32) -> bool {
        self.outstanding.saturating_add(fragments) <= self.ceiling
    }
}

/// Whether the encoder should be asked for a keyframe on the next frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyframe {
    /// Nothing needs one, or the throttle refused.
    NotNeeded,
    /// At least one skipping guest could now take the biggest frame the
    /// session has produced. **The request is global**, because encode is
    /// shared, and that costs a bounded spike for everyone.
    Request,
}

/// The delivery decision for a stream.
#[derive(Debug)]
pub struct Gate {
    /// **The session high-water mark, in fragments, not this frame's count.**
    /// A skipping guest is retested against this. Testing against the frame in
    /// hand lets a guest out of the cascade on a small predicted frame,
    /// whereupon the keyframe it actually needs does not fit, the keyframe
    /// grant is spent, and every guest pays the spike for a recovery that did
    /// not happen.
    largest: u32,
    /// When a keyframe was last asked for, so the throttle can refuse.
    last_request: Option<Time>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Self {
            largest: 0,
            last_request: None,
        }
    }

    /// The biggest frame the session has produced, in fragments.
    pub fn largest(&self) -> u32 {
        self.largest
    }

    /// Decide delivery of one encoded frame across every guest.
    ///
    /// `deliver` is called with the index of each guest that takes the frame,
    /// and is not called at all for the others. **There is no way to learn
    /// that a guest was withheld from without the gate having latched it**,
    /// which is what keeps the cascade from being bypassed by a caller that
    /// forgets.
    ///
    /// `now` is passed rather than read so the throttle is testable without a
    /// clock.
    pub fn admit(
        &mut self,
        fragments: u32,
        keyframe: bool,
        now: Time,
        guests: &mut [Guest],
        mut deliver: impl FnMut(usize),
    ) -> Keyframe {
        // This frame counts toward the mark before anything is tested against
        // it, because a guest that cannot hold the frame in hand certainly
        // cannot hold the biggest one yet seen.
        self.largest = self.largest.max(fragments);

        let mut wanted = false;
        for (index, guest) in guests.iter_mut().enumerate() {
            if guest.pending_keyframe {
                if !keyframe {
                    // Retested against the mark, never against this frame.
                    wanted |= guest.has_room_for(self.largest);
                    continue;
                }
                if !guest.has_room_for(fragments) {
                    // The keyframe itself does not fit. Still pending, and
                    // still asking.
                    wanted |= guest.has_room_for(self.largest);
                    continue;
                }
                deliver(index);
                // The one place this clears.
                guest.pending_keyframe = false;
                continue;
            }
            if !guest.has_room_for(fragments) {
                // Latches. A guest that misses one frame misses every frame
                // until a keyframe, because everything after this one refers
                // back through it.
                guest.pending_keyframe = true;
                continue;
            }
            deliver(index);
        }

        if wanted && self.throttle_allows(now) {
            self.last_request = Some(now);
            Keyframe::Request
        } else {
            Keyframe::NotNeeded
        }
    }

    fn throttle_allows(&self, now: Time) -> bool {
        match self.last_request {
            None => true,
            Some(last) => diff_ms(last, now) >= KEYFRAME_INTERVAL_MS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect which guests took a frame, so the tests read as the algorithm
    /// does.
    fn pass(
        gate: &mut Gate,
        fragments: u32,
        keyframe: bool,
        now: Time,
        guests: &mut [Guest],
    ) -> (Vec<usize>, Keyframe) {
        let mut took = Vec::new();
        let request = gate.admit(fragments, keyframe, now, guests, |index| took.push(index));
        (took, request)
    }

    #[test]
    fn the_ceiling_steps_at_twenty_and_thirty() {
        assert_eq!(ceiling(5.0), 1500);
        assert_eq!(ceiling(19.99), 1500);
        assert_eq!(ceiling(20.0), 2500);
        assert_eq!(ceiling(29.99), 2500);
        // The top step is the peer's ring depth; nothing goes past it.
        assert_eq!(ceiling(30.0), 4000);
        assert_eq!(ceiling(500.0), 4000);
    }

    #[test]
    fn a_joining_guest_waits_for_a_keyframe_and_takes_the_first_that_fits() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(20.0)];
        let now = Time::now();

        // Predicted frames are not for a guest with no reference chain.
        let (took, _) = pass(&mut gate, 10, false, now, &mut guests);
        assert!(
            took.is_empty(),
            "a joining guest was sent a predicted frame"
        );
        assert!(guests[0].is_skipping());

        let (took, _) = pass(&mut gate, 40, true, now, &mut guests);
        assert_eq!(took, vec![0]);
        assert!(
            !guests[0].is_skipping(),
            "the keyframe did not clear the wait"
        );

        let (took, _) = pass(&mut gate, 10, false, now, &mut guests);
        assert_eq!(took, vec![0], "a guest in the chain stopped receiving");
    }

    /// **The gray-frame regression.** A guest whose window fills must not
    /// receive anything until a keyframe, however much room it recovers. A
    /// dependent frame across the gap decodes into progressively wrong output
    /// rather than failing, which is the whole reason the latch exists.
    #[test]
    fn a_starved_guest_that_recovers_never_receives_a_dependent_frame() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(20.0)];
        let now = Time::now();

        // Get it into the chain first.
        pass(&mut gate, 40, true, now, &mut guests);
        assert!(!guests[0].is_skipping());

        // The window fills: acknowledgements stop and outstanding climbs past
        // the ceiling.
        guests[0].set_outstanding(2490);
        let (took, _) = pass(&mut gate, 20, false, now, &mut guests);
        assert!(took.is_empty(), "a frame went out past the ceiling");
        assert!(guests[0].is_skipping(), "the miss did not latch");

        // The window drains completely. **Every one of these must be
        // withheld**: they all refer back through the frame that was missed.
        guests[0].set_outstanding(0);
        for size in [5u32, 10, 5, 8, 12] {
            let (took, _) = pass(&mut gate, size, false, now, &mut guests);
            assert!(
                took.is_empty(),
                "a dependent frame was delivered across the gap"
            );
            assert!(guests[0].is_skipping());
        }

        // Only a keyframe ends it.
        let (took, _) = pass(&mut gate, 40, true, now, &mut guests);
        assert_eq!(took, vec![0]);
        assert!(!guests[0].is_skipping());
    }

    /// A skipping guest is retested against the biggest frame the session has
    /// produced, not against the frame in hand. Otherwise a small predicted
    /// frame spends the keyframe grant on a recovery that cannot happen.
    #[test]
    fn the_retest_uses_the_high_water_mark_not_the_frame_in_hand() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(20.0)];
        let now = Time::now();

        // A big keyframe sets the mark, and the guest takes it.
        pass(&mut gate, 2000, true, now, &mut guests);
        assert_eq!(gate.largest(), 2000);

        // It then falls out of the chain.
        guests[0].set_outstanding(2400);
        pass(&mut gate, 200, false, now, &mut guests);
        assert!(guests[0].is_skipping());

        // Room for a small frame, but nowhere near room for the biggest the
        // session makes. No request: the keyframe would not fit either.
        guests[0].set_outstanding(1000);
        let (_, request) = pass(&mut gate, 5, false, now, &mut guests);
        assert_eq!(
            request,
            Keyframe::NotNeeded,
            "a small frame released a guest that cannot hold a keyframe"
        );

        // Room for the mark, so now it is worth asking.
        guests[0].set_outstanding(400);
        let (_, request) = pass(&mut gate, 5, false, now, &mut guests);
        assert_eq!(request, Keyframe::Request);
    }

    #[test]
    fn skipping_is_per_guest_and_one_guest_falling_out_does_not_stop_another() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(20.0), Guest::joining(20.0)];
        let now = Time::now();
        pass(&mut gate, 40, true, now, &mut guests);

        guests[0].set_outstanding(2490);
        let (took, _) = pass(&mut gate, 20, false, now, &mut guests);
        assert_eq!(
            took,
            vec![1],
            "the healthy guest was punished for the other"
        );
        assert!(guests[0].is_skipping());
        assert!(!guests[1].is_skipping());
    }

    #[test]
    fn the_keyframe_request_is_throttled() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(20.0)];
        let start = Time::now();

        pass(&mut gate, 100, true, start, &mut guests);
        guests[0].set_outstanding(2490);
        pass(&mut gate, 20, false, start, &mut guests);
        guests[0].set_outstanding(0);

        // The first ask is granted.
        let (_, request) = pass(&mut gate, 5, false, start, &mut guests);
        assert_eq!(request, Keyframe::Request);
        // Immediately after, refused, however many frames ask.
        for _ in 0..10 {
            let (_, request) = pass(&mut gate, 5, false, start, &mut guests);
            assert_eq!(request, Keyframe::NotNeeded, "the throttle let one past");
        }
    }

    /// A keyframe that does not fit leaves the guest pending. Delivering it
    /// anyway would overrun the window; clearing the latch without delivering
    /// would resume a chain the guest has no start for.
    #[test]
    fn a_keyframe_too_big_for_the_window_neither_delivers_nor_clears() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(20.0)];
        let now = Time::now();

        guests[0].set_outstanding(2400);
        let (took, _) = pass(&mut gate, 500, true, now, &mut guests);
        assert!(took.is_empty(), "a keyframe went out past the ceiling");
        assert!(
            guests[0].is_skipping(),
            "the wait was cleared without a frame"
        );

        guests[0].set_outstanding(0);
        let (took, _) = pass(&mut gate, 500, true, now, &mut guests);
        assert_eq!(took, vec![0]);
        assert!(!guests[0].is_skipping());
    }

    #[test]
    fn no_guests_is_not_a_special_case() {
        let mut gate = Gate::new();
        let now = Time::now();
        let (took, request) = pass(&mut gate, 10, true, now, &mut []);
        assert!(took.is_empty());
        assert_eq!(request, Keyframe::NotNeeded);
        assert_eq!(gate.largest(), 10, "the mark still moves with the stream");
    }
}
