//! What bitrate the one encoder runs at, across every guest sharing it.
//!
//! Two aggregations compose here and they do different jobs. See
//! docs/05-host.md section 5.
//!
//! **The configured rate is a ceiling divided by the guests on the stream.**
//! Every guest receives the same encoded bytes, so N guests cost N times the
//! encoded rate on the way out; skipping the division does not oversubscribe
//! one guest, it oversubscribes the host by a factor of N and the loss lands
//! on all of them at once.
//!
//! **The rate applied is the minimum of what the controllers return.** A
//! chronically slow guest does pull everyone down, and that is intended rather
//! than a flaw in the aggregation: the rate is what the transport can actually
//! carry, and sending a guest more than that produces loss, not quality. What
//! the slow guest must not do is break the others' streams, and it cannot,
//! because delivery is decided per guest by the gate.

use lowlat_core::congestion::Controller;

/// How far the rate must move before the encoder is reconfigured.
///
/// **Without it the encoder is reconfigured every frame.** The controller's
/// output wanders by tiny amounts as the window does, and a reconfigure per
/// frame is work for a change nothing can see.
pub const DEADBAND_MBPS: f64 = 0.01;

/// One guest's transport state for a tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Fragments between the send base and the send next.
    pub window: u32,
    /// How many of them are stale.
    pub stale: u32,
    /// Throughput observed since the last increase.
    ///
    /// **Needs fractional-millisecond intervals.** Quantised to whole
    /// milliseconds the measurement is skipped whenever an interval rounds to
    /// zero, which at these frame rates is most of them.
    pub measured_mbps: f64,
}

/// The bitrate budget for one stream.
#[derive(Debug)]
pub struct Budget {
    configured_mbps: f64,
    min_mbps: f64,
    guests: u32,
    applied_mbps: f64,
}

impl Budget {
    pub fn new(configured_mbps: f64, min_mbps: f64) -> Self {
        Self {
            configured_mbps,
            min_mbps,
            guests: 0,
            applied_mbps: 0.0,
        }
    }

    /// The ceiling one guest's controller may climb to.
    ///
    /// A count of zero is treated as one rather than dividing by it, which is
    /// the same guard the reference carries: the stream is torn down when the
    /// last guest leaves, so a zero here is a race rather than a state.
    pub fn ceiling(&self) -> f64 {
        self.configured_mbps / f64::from(self.guests.max(1))
    }

    /// The rate last handed to the encoder.
    pub fn applied_mbps(&self) -> f64 {
        self.applied_mbps
    }

    pub fn guests(&self) -> u32 {
        self.guests
    }

    /// The configured rate changed, or a guest arrived or left.
    ///
    /// **Both are the same event to a controller**: its ceiling moved, and it
    /// has to be told rather than discovering it on a tick, because the rate it
    /// is currently holding may be above the new one.
    pub fn rebound(&mut self, guests: u32, controllers: &mut [Controller]) {
        self.guests = guests;
        let ceiling = self.ceiling();
        for controller in controllers.iter_mut() {
            controller.set_bounds(self.min_mbps, ceiling);
        }
    }

    /// Reconfigure the rate this stream runs at.
    pub fn reconfigure(&mut self, configured_mbps: f64, controllers: &mut [Controller]) {
        self.configured_mbps = configured_mbps;
        let guests = self.guests;
        self.rebound(guests, controllers);
    }

    /// One pass, once per frame, over every guest on this stream.
    ///
    /// **The tick is the frame.** The controller's periods are counted in
    /// ticks, so at sixty a second its thirty clean ticks are half a second
    /// and its sixty congested ticks are one second. Ticking it from a timer
    /// instead would change what those numbers mean.
    ///
    /// Returns the new rate only when it moved past the deadband, so the
    /// caller reconfigures the encoder exactly when there is something to
    /// apply.
    pub fn tick(&mut self, controllers: &mut [Controller], samples: &[Sample]) -> Option<f64> {
        let mut best: Option<f64> = None;
        for (controller, sample) in controllers.iter_mut().zip(samples) {
            let rate = controller.tick(sample.window, sample.stale, sample.measured_mbps);
            best = Some(match best {
                Some(current) => current.min(rate),
                None => rate,
            });
        }

        let best = best?;
        if (best - self.applied_mbps).abs() <= DEADBAND_MBPS {
            return None;
        }
        self.applied_mbps = best;
        Some(best)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowlat_core::congestion::{DEFAULT_LEVEL, WINDOW_FLOOR};

    /// Below the window floor nothing is congestion, whatever the stale count.
    fn healthy() -> Sample {
        Sample {
            window: WINDOW_FLOOR,
            stale: 0,
            measured_mbps: 8.0,
        }
    }

    /// Well past the floor and almost entirely stale, which no level tolerates.
    fn congested() -> Sample {
        Sample {
            window: WINDOW_FLOOR * 10,
            stale: WINDOW_FLOOR * 9,
            measured_mbps: 1.0,
        }
    }

    fn controllers(count: usize, ceiling: f64) -> Vec<Controller> {
        (0..count)
            .map(|_| Controller::new(DEFAULT_LEVEL, 1.0, ceiling))
            .collect()
    }

    /// **The division is the whole point.** Four guests on one stream cost four
    /// times the encoded rate on the way out, so each one's ceiling is a
    /// quarter of the budget.
    #[test]
    fn the_ceiling_is_the_budget_divided_by_the_guests_on_the_stream() {
        let mut budget = Budget::new(20.0, 1.0);
        let mut guests = controllers(1, 20.0);

        budget.rebound(1, &mut guests);
        assert!((budget.ceiling() - 20.0).abs() < 1e-9);
        assert!((guests[0].max_mbps() - 20.0).abs() < 1e-9);

        let mut four = controllers(4, 20.0);
        budget.rebound(4, &mut four);
        assert!((budget.ceiling() - 5.0).abs() < 1e-9);
        assert!(
            four.iter().all(|c| (c.max_mbps() - 5.0).abs() < 1e-9),
            "a guest kept a ceiling the host cannot afford"
        );
    }

    /// A guest arriving lowers the ceiling for the guests already streaming,
    /// and one leaving raises it again.
    #[test]
    fn a_guest_arriving_or_leaving_moves_everyones_ceiling() {
        let mut budget = Budget::new(24.0, 1.0);
        let mut guests = controllers(3, 24.0);
        budget.rebound(3, &mut guests);
        assert!((budget.ceiling() - 8.0).abs() < 1e-9);
        budget.rebound(2, &mut guests);
        assert!((budget.ceiling() - 12.0).abs() < 1e-9);
        assert!(guests.iter().all(|c| (c.max_mbps() - 12.0).abs() < 1e-9));
    }

    /// A count of zero is a race rather than a state, and must not divide.
    #[test]
    fn no_guests_does_not_divide_by_zero() {
        let mut budget = Budget::new(20.0, 1.0);
        budget.rebound(0, &mut []);
        assert!(budget.ceiling().is_finite());
        assert!((budget.ceiling() - 20.0).abs() < 1e-9);
    }

    /// **The slowest guest sets the rate.** Not an average and not a majority:
    /// a rate above what a path carries produces loss, not quality.
    #[test]
    fn the_applied_rate_is_the_minimum_across_guests() {
        let mut budget = Budget::new(20.0, 1.0);
        let mut guests = controllers(2, 20.0);
        budget.rebound(2, &mut guests);

        // Drive one guest down and leave the other healthy.
        let mut applied = 0.0;
        for _ in 0..600 {
            if let Some(rate) = budget.tick(&mut guests, &[congested(), healthy()]) {
                applied = rate;
            }
        }
        let slow = guests[0].rate_mbps();
        let fast = guests[1].rate_mbps();
        assert!(
            slow < fast,
            "the congested guest did not fall below the healthy one: {slow} vs {fast}"
        );
        assert!(
            (applied - slow).abs() < 1e-9,
            "the applied rate {applied} is not the slowest guest's {slow}"
        );
    }

    /// The deadband is what stops a reconfigure per frame.
    #[test]
    fn a_rate_that_barely_moves_does_not_reconfigure() {
        let mut budget = Budget::new(20.0, 1.0);
        let mut guests = controllers(1, 20.0);
        budget.rebound(1, &mut guests);

        // The first pass settles a rate.
        let mut reconfigures = 0usize;
        for _ in 0..5 {
            if budget.tick(&mut guests, &[healthy()]).is_some() {
                reconfigures += 1;
            }
        }
        let settled = reconfigures;
        // Ticking a steady healthy guest must not keep reconfiguring.
        for _ in 0..20 {
            if budget.tick(&mut guests, &[healthy()]).is_some() {
                reconfigures += 1;
            }
        }
        assert!(
            reconfigures - settled <= 1,
            "{} reconfigures for a steady stream",
            reconfigures - settled
        );
    }

    #[test]
    fn a_stream_with_no_guests_produces_no_rate() {
        let mut budget = Budget::new(20.0, 1.0);
        assert_eq!(budget.tick(&mut [], &[]), None);
        assert!((budget.applied_mbps() - 0.0).abs() < 1e-9);
    }

    /// Lowering the configured rate must pull a guest already above it down,
    /// rather than leaving it there until congestion happens to act.
    #[test]
    fn lowering_the_budget_pulls_a_guest_down_at_once() {
        let mut budget = Budget::new(40.0, 1.0);
        let mut guests = controllers(1, 40.0);
        budget.rebound(1, &mut guests);
        for _ in 0..200 {
            budget.tick(&mut guests, &[healthy()]);
        }
        // Calibrated from where it actually got to, rather than from a guess
        // about how fast the controller climbs.
        let before = guests[0].rate_mbps();
        assert!(before > 1.0, "the guest never climbed at all: {before}");

        let lowered = before / 2.0;
        budget.reconfigure(lowered, &mut guests);
        assert!(
            guests[0].rate_mbps() <= lowered + 1e-9,
            "a guest kept {} after the budget dropped to {lowered}",
            guests[0].rate_mbps()
        );
    }
}
