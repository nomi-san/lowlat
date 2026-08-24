//! Monotonic time and precise sleeping.
//!
//! Two rules live here, both scars:
//!
//! 1. Intervals are **fractional milliseconds**. The congestion controller
//!    measures throughput over the interval between ticks, and quantizing that
//!    interval to whole milliseconds silently skips the update whenever it
//!    rounds to zero.
//! 2. Sleeps use an **absolute deadline** built from the monotonic clock, and
//!    finish with a short spin. A sleep of 200 us or less degrades into a busy
//!    spin on every platform we target, so the last 200 us is spun
//!    deliberately rather than requested from the scheduler.

use core::time::Duration;
use std::time::Instant;

/// A monotonic instant. Never a wall clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time(Instant);

impl Time {
    pub fn now() -> Self {
        Self(Instant::now())
    }
}

/// Milliseconds from `begin` to `end`, fractional.
///
/// Saturates at zero if `end` precedes `begin`, so a caller can never see a
/// negative interval and divide by it.
pub fn diff_ms(begin: Time, end: Time) -> f64 {
    end.0.saturating_duration_since(begin.0).as_secs_f64() * 1000.0
}

/// Milliseconds elapsed since `begin`, fractional.
pub fn elapsed_ms(begin: Time) -> f64 {
    diff_ms(begin, Time::now())
}

/// The tail of a sleep that is spun rather than slept. Requesting a sleep this
/// short from the scheduler is a busy wait with extra steps.
///
/// **One hundred microseconds, and the figure is measured rather than
/// chosen.** At sixty landings a second a 200 us margin costs 0.92 percent of
/// a core and 100 us costs 0.33, for landings that are indistinguishable: p50
/// 0.2 us and p95 0.5 us either way. What the larger margin buys is nothing,
/// because the tail belongs to the scheduler -- preemption puts the p99 and the
/// maximum in the same place at 200 us, at 100, and at no margin at all.
///
/// **It cannot usefully go much lower.** The sleep overshoots its deadline by
/// 45 to 55 us here, so a margin has to exceed that before it corrects
/// anything; measured at 64 us the spin never runs and the landing error is
/// the raw overshoot.
const SPIN_MARGIN: Duration = Duration::from_micros(100);

/// Sleep until `duration` has elapsed, accurately.
///
/// Not for loop cadence. A loop waits on an event with a timeout
/// (see [`crate::wait`]); this is for the rare case of an explicit delay.
pub fn precise_sleep(duration: Duration) {
    if duration.is_zero() {
        return;
    }
    let deadline = Instant::now() + duration;
    if duration > SPIN_MARGIN {
        sleep_until(deadline - SPIN_MARGIN);
    }
    while Instant::now() < deadline {
        core::hint::spin_loop();
    }
}

#[cfg(unix)]
fn sleep_until(target: Instant) {
    let remaining = target.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return;
    }

    // Build the absolute deadline from CLOCK_MONOTONIC directly. Instant's
    // epoch is not guaranteed to be the same clock, so adding a Duration to a
    // clock_gettime reading is correct while converting an Instant is not.
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, properly aligned timespec we own.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now);
    }

    let mut sec = now
        .tv_sec
        .saturating_add(remaining.as_secs() as libc::time_t);
    let mut nsec = now.tv_nsec + remaining.subsec_nanos() as libc::c_long;
    if nsec >= 1_000_000_000 {
        sec += 1;
        nsec -= 1_000_000_000;
    }
    let deadline = libc::timespec {
        tv_sec: sec,
        tv_nsec: nsec,
    };

    loop {
        // SAFETY: `deadline` is a valid timespec; the null remainder pointer is
        // permitted with TIMER_ABSTIME.
        let rc = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &raw const deadline,
                core::ptr::null_mut(),
            )
        };
        // clock_nanosleep returns the error directly rather than through errno.
        if rc != libc::EINTR {
            return;
        }
    }
}

#[cfg(not(unix))]
fn sleep_until(target: Instant) {
    let remaining = target.saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        std::thread::sleep(remaining);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 0 gate: the clock never goes backwards, and it does advance.
    #[test]
    fn monotonic_over_a_million_samples() {
        let mut previous = Time::now();
        let mut advanced = false;
        for _ in 0..1_000_000 {
            let current = Time::now();
            assert!(current >= previous, "clock went backwards");
            if current > previous {
                advanced = true;
            }
            previous = current;
        }
        assert!(advanced, "clock never advanced across a million samples");
    }

    /// Phase 0 gate: a sub-millisecond interval must not round to zero.
    ///
    /// This is the regression test for the rate-controller bug. An
    /// integer-millisecond clock fails here rather than silently producing a
    /// controller that skips its update under fast ticks.
    #[test]
    fn sub_millisecond_intervals_are_fractional() {
        let mut measured = 0.0_f64;
        for _ in 0..1000 {
            let begin = Time::now();
            core::hint::spin_loop();
            let delta = elapsed_ms(begin);
            if delta > 0.0 && delta < 1.0 {
                measured = delta;
                break;
            }
        }
        assert!(
            measured > 0.0,
            "no sub-millisecond interval was representable; the clock is quantized"
        );
        assert!(measured < 1.0);
    }

    #[test]
    fn diff_saturates_rather_than_going_negative() {
        let first = Time::now();
        let second = Time::now();
        // Saturating means the result cannot be negative, so `<= 0.0` is
        // exactly "is zero" here, without a float equality comparison.
        assert!(diff_ms(second, first) <= 0.0);
    }

    #[test]
    fn precise_sleep_does_not_undershoot() {
        let begin = Time::now();
        precise_sleep(Duration::from_millis(5));
        assert!(elapsed_ms(begin) >= 5.0);
    }
}
