//! Who has the pointer.
//!
//! **The pointer is the one input device guests genuinely share.** Each guest
//! has its own keyboard, its own pointers and its own pads, but the display
//! stack merges every pointer on a seat into a single cursor, so two guests
//! moving at once fight over it and neither can aim. Keyboards do not conflict
//! that way and pads do not conflict at all, so neither is arbitrated: with
//! this on, two guests can still type at the same time and each drive its own
//! controller, which is usually what a room full of people wants.
//!
//! **Off by default.** One person driving is a configuration, not a law, and a
//! host that imposed it would break the ordinary case of two people sharing a
//! desktop.

use std::sync::{Arc, Mutex};

/// How long a guest keeps the pointer after its last movement.
///
/// **A pause is not a handover.** Somewhere in a dragged window or a held
/// aim there is a moment of stillness, and a figure short enough to catch
/// those hands the pointer to somebody else mid-gesture. A figure much longer
/// makes taking over feel broken. Half a second is comfortably past any pause
/// inside a gesture and well under the point where waiting feels like a fault.
pub const HOLD_MS: f64 = 500.0;

/// The pointer, and who last moved it.
#[derive(Debug)]
pub struct Floor {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    /// **Absent means nobody arbitrates**, which is the default and makes
    /// every question below answer yes.
    enabled: bool,
    state: Mutex<State>,
}

#[derive(Debug, Default, Clone, Copy)]
struct State {
    holder: Option<u32>,
    since_ms: f64,
}

impl Clone for Floor {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Floor {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            shared: Arc::new(Shared {
                enabled,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Ask for the pointer, taking it if it is free or already this guest's.
    ///
    /// **Called when a guest actually moves something**, not on a timer: the
    /// pointer belongs to whoever is using it, and using it is the only
    /// evidence there is.
    ///
    /// An owner takes it from whoever has it. Everybody else waits for it to
    /// lapse.
    pub fn claim(&self, guest: u32, owner: bool, now_ms: f64) -> bool {
        if !self.shared.enabled {
            return true;
        }
        let Ok(mut state) = self.shared.state.lock() else {
            // A poisoned lock means another guest's thread panicked while
            // holding it. Refusing every guest the pointer from then on turns
            // one thread's fault into a dead session for everybody.
            return true;
        };
        match state.holder {
            Some(held) if held != guest && !owner && now_ms - state.since_ms < HOLD_MS => false,
            _ => {
                if state.holder != Some(guest) {
                    lowlat_common::log_info!("input: the pointer is guest {guest}'s");
                }
                state.holder = Some(guest);
                state.since_ms = now_ms;
                true
            }
        }
    }

    /// Whether this guest has the pointer without asking for it.
    ///
    /// **This is what a guest that stopped moving is asked**, once a pass, so
    /// that losing the pointer releases what it was holding rather than
    /// waiting for a message that is never going to arrive.
    #[must_use]
    pub fn holds(&self, guest: u32, now_ms: f64) -> bool {
        if !self.shared.enabled {
            return true;
        }
        let Ok(state) = self.shared.state.lock() else {
            return true;
        };
        match state.holder {
            Some(held) => held == guest || now_ms - state.since_ms >= HOLD_MS,
            None => true,
        }
    }

    /// Give the pointer up, when a guest leaves.
    pub fn release(&self, guest: u32) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        if state.holder == Some(guest) {
            state.holder = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off is off: nobody waits for anybody.
    #[test]
    fn nothing_is_arbitrated_when_it_is_disabled() {
        let floor = Floor::new(false);
        assert!(floor.claim(1, false, 0.0));
        assert!(floor.claim(2, false, 0.0));
        assert!(floor.holds(1, 0.0));
        assert!(floor.holds(2, 0.0));
    }

    #[test]
    fn the_first_to_move_takes_it_and_keeps_it_while_moving() {
        let floor = Floor::new(true);
        assert!(floor.claim(1, false, 0.0));
        assert!(!floor.claim(2, false, 10.0), "a second guest took it");
        assert!(
            floor.claim(1, false, 400.0),
            "the holder lost its own pointer"
        );
        // Refreshed at 400, so the other one still cannot have it at 800.
        assert!(!floor.claim(2, false, 800.0));
    }

    /// **The figure is the whole behaviour, so it is pinned and not merely
    /// used.** Every other test here is written against `HOLD_MS`, which
    /// means all of them scale with it and none of them would notice it
    /// being wrong. Below roughly a fifth of a second the pointer is taken
    /// away mid-gesture, because there is a still moment inside any drag or
    /// held aim; above about a second, taking over reads as the session
    /// having stopped responding.
    #[test]
    fn the_hold_is_long_enough_for_a_pause_and_short_enough_to_hand_over() {
        assert!(
            (200.0..=1000.0).contains(&HOLD_MS),
            "HOLD_MS is {HOLD_MS}, outside the band that makes it work"
        );
    }

    /// **A pause is not a handover, and stopping is.** The two are told apart
    /// by the hold alone, so the figure is the whole behaviour.
    #[test]
    fn it_lapses_once_the_holder_stops() {
        let floor = Floor::new(true);
        assert!(floor.claim(1, false, 0.0));
        assert!(
            !floor.claim(2, false, HOLD_MS - 1.0),
            "it lapsed inside a pause"
        );
        assert!(floor.claim(2, false, HOLD_MS), "it never lapsed");
        // And now it is the second guest's.
        assert!(!floor.claim(1, false, HOLD_MS + 1.0));
    }

    /// The owner does not wait.
    #[test]
    fn an_owner_takes_it_from_whoever_has_it() {
        let floor = Floor::new(true);
        assert!(floor.claim(1, false, 0.0));
        assert!(floor.claim(2, true, 10.0), "the owner had to wait");
        assert!(!floor.claim(1, false, 20.0), "the owner did not keep it");
    }

    /// A guest that stopped moving must find out it no longer has the
    /// pointer, because it has nothing else to tell it.
    #[test]
    fn a_guest_that_stopped_moving_learns_it_lost_the_pointer() {
        let floor = Floor::new(true);
        assert!(floor.claim(1, false, 0.0));
        assert!(floor.holds(1, 10.0));
        assert!(!floor.holds(2, 10.0));

        assert!(floor.claim(2, false, HOLD_MS));
        assert!(
            !floor.holds(1, HOLD_MS),
            "the old holder still thinks it has it"
        );
        assert!(floor.holds(2, HOLD_MS));
    }

    /// Nobody holding it is not the same as somebody holding it: a lapsed
    /// pointer is free for the asking, including to the guest that had it.
    #[test]
    fn a_lapsed_pointer_is_free_to_everyone() {
        let floor = Floor::new(true);
        assert!(floor.claim(1, false, 0.0));
        assert!(floor.holds(2, HOLD_MS));
        assert!(floor.holds(1, HOLD_MS));
    }

    #[test]
    fn a_guest_that_leaves_gives_it_up() {
        let floor = Floor::new(true);
        assert!(floor.claim(1, false, 0.0));
        assert!(!floor.claim(2, false, 10.0));
        floor.release(1);
        assert!(floor.claim(2, false, 20.0), "it was not given up");
    }

    /// Leaving must not take the pointer away from whoever has it now.
    #[test]
    fn a_guest_that_leaves_takes_nothing_that_is_not_its_own() {
        let floor = Floor::new(true);
        assert!(floor.claim(1, false, 0.0));
        assert!(floor.claim(2, true, 10.0));
        floor.release(1);
        assert!(
            !floor.claim(1, false, 20.0),
            "the leaver freed another guest's pointer"
        );
    }

    /// Every guest thread holds its own handle onto one arbiter.
    #[test]
    fn a_clone_is_the_same_pointer_and_not_another_one() {
        let floor = Floor::new(true);
        let other = floor.clone();
        assert!(floor.claim(1, false, 0.0));
        assert!(!other.claim(2, false, 10.0));
    }
}
