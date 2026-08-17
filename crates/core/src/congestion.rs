//! Host-local congestion control (docs/01-protocol.md 10).
//!
//! Every input is local transport state. **There is no congestion feedback
//! message in either direction and none may be added.** The output actuates the
//! encoder's bitrate through a live reconfigure; it does not pace the socket.
//!
//! The stale count this consumes is produced by the retransmission scan
//! ([`crate::send`]). The two are one loop split across two modules, not
//! independent subsystems: changing the scan changes this controller's input.

/// Window below which congestion is never declared, and the cap on outstanding
/// fragments. One constant, three consumers; tuning it in one place alone
/// desynchronises the other two.
pub const WINDOW_FLOOR: u32 = 100;

/// Consecutive clean ticks between rate increases.
const INCREASE_PERIOD: u32 = 30;
/// Consecutive congested ticks between rate decreases.
const DECREASE_PERIOD: u32 = 60;
/// Multiplicative decrease.
const DECREASE_FACTOR: f64 = 0.7;
/// Additive increase per step unit.
const INCREASE_STEP_MBPS: f64 = 0.15;
/// Step growth per increase, and the value it is capped at when applied.
const STEP_GROWTH: u32 = 2;
const STEP_CAP: u32 = 5;

/// Per-level tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// Multiplier on the smoothed round trip when classifying staleness.
    pub rtt_mult: f64,
    /// Constant added to the staleness threshold, in milliseconds.
    pub base_ms: f64,
    /// Stale-to-window ratio above which the channel is congested.
    pub stale_ratio: f64,
}

/// Level 0 is **not** "disabled".
///
/// Its threshold of zero means any stale fragment declares congestion once the
/// window exceeds the floor, which makes it the most aggressive setting rather
/// than the least. It exists for compatibility with an older scheme and must
/// never be used as a fallback for an out-of-range value.
pub const LEVELS: [Level; 3] = [
    Level {
        rtt_mult: 0.0,
        base_ms: 0.0,
        stale_ratio: 0.0,
    },
    Level {
        rtt_mult: 1.1,
        base_ms: 20.0,
        stale_ratio: 0.15,
    },
    Level {
        rtt_mult: 1.5,
        base_ms: 50.0,
        stale_ratio: 0.35,
    },
];

/// The default. Not level 0; see [`LEVELS`].
pub const DEFAULT_LEVEL: usize = 1;

/// Resolve a level index, clamping to the default rather than to zero.
pub fn level(index: usize) -> Level {
    *LEVELS.get(index).unwrap_or(&LEVELS[DEFAULT_LEVEL])
}

/// Rate controller for one channel.
#[derive(Debug, Clone)]
pub struct Controller {
    level: usize,
    min_mbps: f64,
    max_mbps: f64,
    current_mbps: f64,
    peak_mbps: f64,
    increase_ticks: u32,
    decrease_ticks: u32,
    step: u32,
    /// Set until the first increase, which snaps the rate to the floor rather
    /// than creeping up from wherever it started.
    reset_pending: bool,
    total_decreases: u32,
}

impl Controller {
    pub fn new(level: usize, min_mbps: f64, max_mbps: f64) -> Self {
        Self {
            level,
            min_mbps,
            max_mbps,
            current_mbps: min_mbps,
            peak_mbps: min_mbps,
            increase_ticks: 0,
            decrease_ticks: 0,
            step: 1,
            reset_pending: true,
            total_decreases: 0,
        }
    }

    /// Move the bounds, because the budget they came from changed.
    ///
    /// **The ceiling is not a constant of the session.** It is the configured
    /// rate divided by the guests sharing the stream, so a guest arriving or
    /// leaving moves it for everyone. The current rate is pulled down to the
    /// new ceiling rather than left above it, and the reset is armed so the
    /// next increase snaps to the floor and climbs from there rather than
    /// resuming from a rate that no longer applies.
    pub fn set_bounds(&mut self, min_mbps: f64, max_mbps: f64) {
        self.min_mbps = min_mbps;
        self.max_mbps = max_mbps;
        if self.current_mbps > max_mbps {
            self.current_mbps = max_mbps;
        }
        if self.peak_mbps > max_mbps {
            self.peak_mbps = max_mbps;
        }
        self.reset_pending = true;
    }

    /// The ceiling currently in force.
    pub fn max_mbps(&self) -> f64 {
        self.max_mbps
    }

    /// How many times the rate has been cut. Surfaced for diagnostics.
    pub fn total_decreases(&self) -> u32 {
        self.total_decreases
    }

    /// Current rate, already clamped.
    pub fn rate_mbps(&self) -> f64 {
        self.current_mbps.clamp(self.min_mbps, self.max_mbps)
    }

    /// True if this window and stale count constitute congestion.
    pub fn is_congested(&self, window: u32, stale: u32) -> bool {
        if window <= WINDOW_FLOOR {
            return false;
        }
        let ratio = f64::from(stale) / f64::from(window);
        ratio > level(self.level).stale_ratio
    }

    /// One tick.
    ///
    /// `measured_mbps` is the throughput observed since the last increase, used
    /// to track the peak. Measuring it needs fractional-millisecond intervals:
    /// quantizing to whole milliseconds skips the update whenever the interval
    /// rounds to zero.
    pub fn tick(&mut self, window: u32, stale: u32, measured_mbps: f64) -> f64 {
        if self.is_congested(window, stale) {
            // The pre-increment value is tested, so the first congested tick
            // acts and then every sixtieth after it.
            let observed = self.decrease_ticks;
            self.decrease_ticks = self.decrease_ticks.wrapping_add(1);
            if observed % DECREASE_PERIOD == 0 {
                self.total_decreases = self.total_decreases.saturating_add(1);
                self.increase_ticks = 0;
                self.peak_mbps *= DECREASE_FACTOR;
                self.current_mbps = self.peak_mbps;
            }
        } else {
            // Here the post-increment value is tested, so the first action
            // lands on the thirtieth clean tick rather than the first.
            self.increase_ticks = self.increase_ticks.wrapping_add(1);
            if self.increase_ticks % INCREASE_PERIOD == 0 {
                self.decrease_ticks = 0;
                if self.reset_pending {
                    self.current_mbps = self.min_mbps;
                    self.peak_mbps = self.min_mbps;
                    self.reset_pending = false;
                } else {
                    if measured_mbps > self.peak_mbps {
                        self.peak_mbps = measured_mbps;
                    }
                    let step = self.step.min(STEP_CAP);
                    self.current_mbps += f64::from(step) * INCREASE_STEP_MBPS;
                    self.step = self.step.saturating_add(STEP_GROWTH);
                }
            }
        }
        self.rate_mbps()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> Controller {
        Controller::new(DEFAULT_LEVEL, 1.0, 100.0)
    }

    #[test]
    fn level_zero_is_the_most_aggressive_not_disabled() {
        let zero = level(0);
        assert!(zero.stale_ratio <= 0.0);
        let aggressive = Controller::new(0, 1.0, 100.0);
        // A single stale fragment past the floor is congestion at level 0.
        assert!(aggressive.is_congested(WINDOW_FLOOR + 1, 1));
        // Whereas the default tolerates it.
        assert!(!controller().is_congested(WINDOW_FLOOR + 1, 1));
    }

    #[test]
    fn an_out_of_range_level_falls_back_to_the_default_not_to_zero() {
        assert_eq!(level(99), LEVELS[DEFAULT_LEVEL]);
        assert_ne!(level(99), LEVELS[0]);
    }

    #[test]
    fn a_small_window_is_never_congested() {
        let controller = controller();
        assert!(!controller.is_congested(WINDOW_FLOOR, u32::from(u16::MAX)));
        assert!(!controller.is_congested(10, 10));
    }

    #[test]
    fn the_ratio_decides_above_the_floor() {
        let controller = controller();
        // 0.15 threshold: 15 of 200 is not above it, 31 is.
        assert!(!controller.is_congested(200, 30));
        assert!(controller.is_congested(200, 31));
    }

    /// The pre-increment test means the very first congested tick acts.
    #[test]
    fn the_first_congested_tick_cuts_the_rate() {
        let mut controller = controller();
        controller.peak_mbps = 10.0;
        controller.current_mbps = 10.0;
        controller.tick(200, 100, 0.0);
        assert_eq!(controller.total_decreases(), 1);
        assert!((controller.rate_mbps() - 7.0).abs() < 1e-9);
    }

    #[test]
    fn further_cuts_wait_for_the_period() {
        let mut controller = controller();
        controller.peak_mbps = 10.0;
        controller.current_mbps = 10.0;
        for _ in 0..DECREASE_PERIOD {
            controller.tick(200, 100, 0.0);
        }
        assert_eq!(
            controller.total_decreases(),
            1,
            "cut more than once too soon"
        );
        controller.tick(200, 100, 0.0);
        assert_eq!(controller.total_decreases(), 2);
    }

    /// The post-increment test means nothing happens until the period elapses.
    #[test]
    fn increases_wait_a_full_period() {
        let mut controller = controller();
        for _ in 0..INCREASE_PERIOD - 1 {
            controller.tick(10, 0, 5.0);
        }
        assert!(controller.reset_pending, "acted before the period elapsed");
        controller.tick(10, 0, 5.0);
        assert!(!controller.reset_pending);
    }

    #[test]
    fn the_first_increase_snaps_to_the_floor_then_creeps() {
        let mut controller = Controller::new(DEFAULT_LEVEL, 2.0, 100.0);
        controller.current_mbps = 50.0;
        for _ in 0..INCREASE_PERIOD {
            controller.tick(10, 0, 0.0);
        }
        assert!((controller.rate_mbps() - 2.0).abs() < 1e-9, "did not snap");
        for _ in 0..INCREASE_PERIOD {
            controller.tick(10, 0, 0.0);
        }
        assert!(controller.rate_mbps() > 2.0, "did not creep back up");
    }

    #[test]
    fn the_rate_stays_inside_its_bounds() {
        let mut controller = Controller::new(DEFAULT_LEVEL, 5.0, 6.0);
        for _ in 0..10_000 {
            controller.tick(10, 0, 1000.0);
        }
        assert!(controller.rate_mbps() <= 6.0);
        for _ in 0..10_000 {
            controller.tick(200, 200, 0.0);
        }
        assert!(controller.rate_mbps() >= 5.0);
    }

    #[test]
    fn congestion_resets_the_increase_counter() {
        let mut controller = controller();
        for _ in 0..INCREASE_PERIOD - 1 {
            controller.tick(10, 0, 1.0);
        }
        controller.tick(200, 100, 0.0);
        // The increase counter was cleared, so one more clean tick must not
        // trigger an increase.
        controller.tick(10, 0, 1.0);
        assert!(controller.reset_pending);
    }
}
