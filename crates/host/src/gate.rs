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

/// The compile-time cap from docs/00-overview.md D10. Ring memory scales per
/// guest, so this bounds it.
pub const MAX_GUESTS: usize = 16;

/// Roughly twice a second, which is the throttle from section 6.
const KEYFRAME_INTERVAL_MS: f64 = 500.0;

/// How long a frame's size goes on constraining who may be admitted.
///
/// **Two seconds, and the mark spans two of these**, so a spike is remembered
/// for between two and four. Long enough that a real keyframe is still the
/// mark when the guest it was sent for is retested; short enough that a size
/// the stream has stopped producing stops deciding anything.
const MARK_WINDOW_MS: f64 = 2_000.0;

/// Whether `interval` has passed between two readings, **or the clock they
/// were taken against has been replaced.**
///
/// This gate outlives the encoder: it is the session's, so that a guest the
/// rebuild latched is retested against a size the stream really produced
/// rather than against the first frame of the new one. The loop's clock does
/// not outlive the encoder -- it starts again from nothing on every rebuild --
/// so a stamp taken before one is a stamp from a clock that no longer exists.
/// Subtracted plainly it reports that the interval will elapse in however long
/// the previous run lasted: a keyframe nobody may ask for, a guest that waits
/// for ever, and not one counter moved because the refusal happens before any
/// of them are reached.
///
/// **A reading before the one on record means the clock restarted**, and the
/// only honest answer then is that the interval is unmeasurable and the thing
/// being throttled should go ahead.
fn elapsed(then: f64, now_ms: f64, interval: f64) -> bool {
    now_ms < then || now_ms - then >= interval
}

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

    /// This guest missed a frame for a reason the room test cannot see.
    ///
    /// **The only way to say a frame was withheld**, and it latches, which is
    /// the point: a caller that drops a frame for its own reasons -- no pool
    /// slot free, a publish ring that refused -- has broken the reference
    /// chain exactly as a full window does, and the recovery is the same. An
    /// operation that skipped one frame without this would eventually be
    /// called.
    pub fn mark_skipping(&mut self) {
        self.pending_keyframe = true;
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
    /// **The high-water mark for the window now running, in fragments, not
    /// this frame's count.** A skipping guest is retested against this rather
    /// than against the frame in hand: testing against the frame in hand lets
    /// a guest out of the cascade on a small predicted frame, whereupon the
    /// keyframe it actually needs does not fit, the keyframe grant is spent,
    /// and every guest pays the spike for a recovery that did not happen.
    largest: u32,
    /// The window before this one, so the mark spans a window and a bit
    /// rather than dropping to nothing the instant one rolls.
    ///
    /// **The mark decays, and that is the whole point of keeping two.** As a
    /// mark over the whole session it only ever grows, and one frame larger
    /// than a guest's ceiling then means no guest may be admitted or even ask
    /// for the keyframe it needs, for as long as the stream lives -- a host
    /// that encodes perfectly and delivers to nobody, which is what this cost
    /// before it decayed.
    previous: u32,
    /// When the window now running began, or nothing before the first frame.
    window_began_ms: Option<f64>,
    /// When a keyframe was last asked for, so the throttle can refuse.
    ///
    /// **Milliseconds, passed in.** Time is a parameter here as it is in the
    /// core, so the interval is testable rather than a test that sleeps half
    /// a second to find out whether it elapsed.
    last_request_ms: Option<f64>,
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
            previous: 0,
            window_began_ms: None,
            last_request_ms: None,
        }
    }

    /// The biggest frame the recent stream has produced, in fragments.
    pub fn largest(&self) -> u32 {
        self.largest.max(self.previous)
    }

    /// Roll the window if this frame belongs to the next one.
    ///
    /// **Measured in time rather than in frames**, because the frame rate is
    /// not a constant here: a still desktop sends one picture a second, and a
    /// window counted in frames would hold a spike for ten minutes on exactly
    /// the stream where a guest is most likely to be joining.
    fn roll(&mut self, now_ms: f64) {
        match self.window_began_ms {
            None => self.window_began_ms = Some(now_ms),
            Some(began) if elapsed(began, now_ms, MARK_WINDOW_MS) => {
                self.previous = self.largest;
                self.largest = 0;
                self.window_began_ms = Some(now_ms);
            }
            Some(_) => {}
        }
    }

    /// Decide delivery of one encoded frame across every guest.
    ///
    /// `deliver` is called with the index of each guest that takes the frame,
    /// and is not called at all for the others. **There is no way to learn
    /// that a guest was withheld from without the gate having latched it**,
    /// which is what keeps the cascade from being bypassed by a caller that
    /// forgets.
    ///
    /// `now_ms` is passed rather than read so the throttle is testable without
    /// a clock.
    pub fn admit(
        &mut self,
        fragments: u32,
        keyframe: bool,
        now_ms: f64,
        guests: &mut [Guest],
        mut deliver: impl FnMut(usize),
    ) -> Keyframe {
        // This frame counts toward the mark before anything is tested against
        // it, because a guest that cannot hold the frame in hand certainly
        // cannot hold the biggest one recently seen.
        self.roll(now_ms);
        self.largest = self.largest.max(fragments);
        let mark = self.largest();

        let mut wanted = false;
        for (index, guest) in guests.iter_mut().enumerate() {
            if guest.pending_keyframe {
                if !keyframe {
                    // Retested against the mark, never against this frame.
                    wanted |= guest.has_room_for(mark);
                    continue;
                }
                if !guest.has_room_for(fragments) {
                    // The keyframe itself does not fit. Still pending, and
                    // still asking.
                    wanted |= guest.has_room_for(mark);
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

        if wanted {
            self.request_keyframe(now_ms)
        } else {
            Keyframe::NotNeeded
        }
    }

    /// Ask for a refresh outside a delivery pass, subject to the same throttle.
    ///
    /// **Every dropped frame has to reach recovery, not only the ones the room
    /// test refuses.** A frame lost because no pool slot was free latches its
    /// guests exactly as a full window does, and there is no delivery pass to
    /// carry the request: the pass that would make it is the one that could
    /// not take a slot. Without this a guest latched that way waits for a
    /// keyframe nothing will ever ask for.
    pub fn request_keyframe(&mut self, now_ms: f64) -> Keyframe {
        if !self.throttle_allows(now_ms) {
            return Keyframe::NotNeeded;
        }
        self.last_request_ms = Some(now_ms);
        Keyframe::Request
    }

    fn throttle_allows(&self, now_ms: f64) -> bool {
        match self.last_request_ms {
            None => true,
            Some(last) => elapsed(last, now_ms, KEYFRAME_INTERVAL_MS),
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
        now_ms: f64,
        guests: &mut [Guest],
    ) -> (Vec<usize>, Keyframe) {
        let mut took = Vec::new();
        let request = gate.admit(fragments, keyframe, now_ms, guests, |index| {
            took.push(index)
        });
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
        let now = 0.0;

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
        let now = 0.0;

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
        let now = 0.0;

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
        let now = 0.0;
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

    /// **A rebuild gives the loop a new clock and this gate keeps its old
    /// stamps**, which stopped every keyframe request for as long as the
    /// previous run had lasted.
    ///
    /// The guest was admitted, seated and waiting; the host captured,
    /// converted and encoded; and every refresh counter read zero, because the
    /// throttle refuses before it reaches one. Switching displays appeared to
    /// cure it, since a rebuild that granted a keyframe left a small stamp
    /// behind instead of a large one.
    #[test]
    fn a_clock_that_restarted_does_not_hold_a_keyframe_for_ever() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(10.0)];

        // A keyframe is asked for late in a long run.
        assert_eq!(gate.request_keyframe(71_000.0), Keyframe::Request);

        // The encoder is rebuilt and the loop starts timing from nothing
        // again. The guest is latched by the rebuild and needs a picture with
        // no history behind it.
        guests[0].mark_skipping();
        assert_eq!(
            gate.admit(10, false, 0.0, &mut guests, |_| {}),
            Keyframe::Request,
            "a stamp from a clock that no longer exists refused every request"
        );

        // The mark keeps its own stamp and must not freeze either.
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(10.0)];
        let spike = ceiling(10.0) + 500;
        let _ = gate.admit(spike, true, 71_000.0, &mut guests, |_| {});
        let _ = gate.admit(10, false, 0.0, &mut guests, |_| {});
        assert_eq!(
            gate.admit(10, false, MARK_WINDOW_MS + 1.0, &mut guests, |_| {}),
            Keyframe::Request,
            "the mark never rolled again, so the spike went on refusing everyone"
        );
    }

    /// **One frame bigger than a guest's ceiling used to end delivery for the
    /// life of the stream.**
    ///
    /// The mark was the largest frame of the whole session and only ever grew,
    /// and a pending guest may only *ask* for the keyframe it needs when it
    /// has room for the mark. So a single spike -- a coded refresh on a larger
    /// screen is enough -- meant no guest could be admitted and none could ask
    /// either: the host went on capturing, converting and encoding, every
    /// refresh counter stayed at zero, and every guest that joined saw a black
    /// screen until the stream was rebuilt under it.
    #[test]
    fn a_spike_stops_deciding_who_may_be_admitted() {
        let mut gate = Gate::new();
        // Ten megabits gives a ceiling of 1500 fragments; the spike is over it.
        let mut guests = [Guest::joining(10.0)];
        let spike = ceiling(10.0) + 500;

        assert_eq!(
            gate.admit(spike, true, 0.0, &mut guests, |_| {}),
            Keyframe::NotNeeded,
            "a keyframe that does not fit is not a keyframe anybody was given"
        );
        assert!(
            guests[0].pending_keyframe,
            "the guest still needs one, since it could not take that one"
        );

        // Inside the window the spike still decides, which is intended: the
        // guest is retested against a size the stream really just produced.
        assert_eq!(
            gate.admit(10, false, 100.0, &mut guests, |_| {}),
            Keyframe::NotNeeded
        );

        // Once the stream has stopped producing that size, it stops deciding.
        let _ = gate.admit(10, false, MARK_WINDOW_MS + 100.0, &mut guests, |_| {});
        assert_eq!(
            gate.admit(10, false, MARK_WINDOW_MS * 2.0 + 200.0, &mut guests, |_| {}),
            Keyframe::Request,
            "a spike the stream left behind was still refusing every guest a keyframe"
        );
    }

    #[test]
    fn the_keyframe_request_is_throttled() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(20.0)];
        let start = 0.0;

        pass(&mut gate, 100, true, start, &mut guests);
        guests[0].set_outstanding(2490);
        pass(&mut gate, 20, false, start, &mut guests);
        guests[0].set_outstanding(0);

        // The first ask is granted.
        let (_, request) = pass(&mut gate, 5, false, start, &mut guests);
        assert_eq!(request, Keyframe::Request);
        // Refused for the whole interval, however many frames ask. At sixty a
        // second that is thirty asks, so a throttle that leaked would show.
        for frame in 1..30u32 {
            let at = start + f64::from(frame) * (1000.0 / 60.0);
            let (_, request) = pass(&mut gate, 5, false, at, &mut guests);
            assert_eq!(
                request,
                Keyframe::NotNeeded,
                "the throttle let one past at frame {frame}"
            );
        }
        // **And granted once the interval has actually passed**, which is the
        // half a clock-free test could never reach before.
        let (_, request) = pass(&mut gate, 5, false, start + 500.0, &mut guests);
        assert_eq!(request, Keyframe::Request, "the throttle never reopened");
    }

    /// A keyframe that does not fit leaves the guest pending. Delivering it
    /// anyway would overrun the window; clearing the latch without delivering
    /// would resume a chain the guest has no start for.
    #[test]
    fn a_keyframe_too_big_for_the_window_neither_delivers_nor_clears() {
        let mut gate = Gate::new();
        let mut guests = [Guest::joining(20.0)];
        let now = 0.0;

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
        let now = 0.0;
        let (took, request) = pass(&mut gate, 10, true, now, &mut []);
        assert!(took.is_empty());
        assert_eq!(request, Keyframe::NotNeeded);
        assert_eq!(gate.largest(), 10, "the mark still moves with the stream");
    }
}
