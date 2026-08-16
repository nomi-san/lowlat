//! Bounded exponential backoff with jitter.
//!
//! A reconnect loop without jitter is worse than one without backoff when it
//! matters most: if a service restarts, every host that was connected to it
//! wakes on the same schedule and arrives together, which is the load pattern
//! the restart was already struggling with. Jitter spreads them.
//!
//! The ceiling matters more than the growth rate. A host that is out of the
//! discovery listing is invisible, so the cost of retrying a little too often
//! is small and the cost of a long ceiling is a host nobody can find.

use core::time::Duration;

/// First delay after a connection is lost.
const INITIAL: Duration = Duration::from_secs(1);

/// Longest delay between attempts.
const CEILING: Duration = Duration::from_secs(5);

/// Where the next delay is drawn from, as a fraction of the current step.
///
/// Equal jitter: half the step is fixed and half is random, so the wait is
/// bounded below as well as above. Drawing from the whole interval would let a
/// run of unlucky draws hammer a service that is trying to come back.
const JITTER_FLOOR: u32 = 2;

/// One reconnect schedule.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    step: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub fn new() -> Self {
        Self { step: INITIAL }
    }

    /// Back to the first delay. Called on a connection that succeeded, so a
    /// host that drops once an hour never accumulates a long wait.
    pub fn reset(&mut self) {
        self.step = INITIAL;
    }

    /// How long to wait before the next attempt, then double the step.
    pub fn next_delay(&mut self) -> Duration {
        let step = self.step;
        self.step = (self.step * 2).min(CEILING);

        let half = step / JITTER_FLOOR;
        let spread = step.saturating_sub(half);
        half + jitter(spread)
    }
}

/// A uniform draw in `[0, span]`.
///
/// Entropy rather than a counter, because the whole point is that two hosts
/// reconnecting from the same event do not agree on when.
fn jitter(span: Duration) -> Duration {
    let micros = u64::try_from(span.as_micros()).unwrap_or(u64::MAX);
    if micros == 0 {
        return Duration::ZERO;
    }
    let mut bytes = [0u8; 8];
    if lowlat_crypto::fill(&mut bytes).is_err() {
        // Never blocks the reconnect on the absence of entropy; the schedule
        // degrades to its floor rather than stopping.
        return Duration::ZERO;
    }
    Duration::from_micros(u64::from_le_bytes(bytes) % (micros + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_grows_and_is_capped() {
        let mut backoff = Backoff::new();
        let mut last = Duration::ZERO;
        for _ in 0..12 {
            let delay = backoff.next_delay();
            assert!(delay <= CEILING, "{delay:?} exceeded the ceiling");
            last = delay;
        }
        // Twelve doublings from one second is well past the ceiling, so the
        // schedule must have settled at it rather than still growing.
        assert!(
            last >= CEILING / JITTER_FLOOR,
            "the schedule collapsed instead of reaching its ceiling: {last:?}"
        );
    }

    /// A connection that succeeds clears the debt. Without this a host that
    /// drops briefly once an hour eventually waits the ceiling every time.
    #[test]
    fn a_successful_connection_clears_the_schedule() {
        let mut backoff = Backoff::new();
        for _ in 0..8 {
            backoff.next_delay();
        }
        backoff.reset();
        assert!(backoff.next_delay() <= INITIAL);
    }

    /// The point of jitter is that two hosts reconnecting from one event do
    /// not agree on when. A fixed schedule passes every bound above.
    /// *Named regression test.*
    #[test]
    fn two_schedules_do_not_agree() {
        let draws = |()| {
            let mut backoff = Backoff::new();
            (0..6).map(|_| backoff.next_delay()).collect::<Vec<_>>()
        };
        let first = draws(());
        let second = draws(());
        assert_ne!(first, second, "the schedule is not jittered");
    }

    /// Bounded below as well as above, so an unlucky run cannot hammer a
    /// service that is trying to come back.
    #[test]
    fn no_delay_collapses_to_nothing() {
        let mut backoff = Backoff::new();
        for _ in 0..10 {
            assert!(backoff.next_delay() >= INITIAL / JITTER_FLOOR);
        }
    }
}
