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

/// Now, in microseconds of `CLOCK_MONOTONIC`: the reading an application
/// takes of the same clock by that name, for a time handed across the
/// boundary. Read directly rather than converted from a [`Time`], whose
/// base is not promised to be that clock.
#[cfg(unix)]
pub fn monotonic_us() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, properly aligned timespec we own.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now);
    }
    u64::try_from(now.tv_sec).unwrap_or(0) * 1_000_000
        + u64::try_from(now.tv_nsec).unwrap_or(0) / 1000
}

/// Now, in microseconds of the performance counter: the clock an application
/// reads here for the same purpose.
#[cfg(windows)]
pub fn monotonic_us() -> u64 {
    let (mut counter, mut frequency) = (0i64, 0i64);
    // SAFETY: both are valid, properly aligned integers we own.
    unsafe {
        imp::QueryPerformanceCounter(&raw mut counter);
        imp::QueryPerformanceFrequency(&raw mut frequency);
    }
    let (Ok(counter), Ok(frequency)) = (u128::try_from(counter), u128::try_from(frequency)) else {
        return 0;
    };
    if frequency == 0 {
        return 0;
    }
    u64::try_from(counter * 1_000_000 / frequency).unwrap_or(u64::MAX)
}

/// No clock by that name here: zero, which a reader takes as not known.
#[cfg(not(any(unix, windows)))]
pub fn monotonic_us() -> u64 {
    0
}

/// The system timer raised to one millisecond for as long as this lives.
///
/// **Only Windows needs it, and there it is not optional.** Every timeout a
/// loop waits with -- a completion port, an address wait, an object wait --
/// is quantised to the system tick, about 15.6 ms unless raised, and the
/// request is per process, so another application having made it does not
/// help. The system counts the requests, so each owner holds one of its own
/// and the last one released lowers it.
#[derive(Debug)]
pub struct TimerResolution(());

impl TimerResolution {
    /// Raise it, until the value returned is dropped.
    #[must_use]
    pub fn raise() -> Self {
        #[cfg(windows)]
        // SAFETY: one millisecond is within every system's supported range;
        // a refusal leaves the resolution where it was, which is harmless.
        unsafe {
            imp::timeBeginPeriod(1);
        }
        Self(())
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: pairs the request made in `raise`.
        unsafe {
            imp::timeEndPeriod(1);
        }
    }
}

#[cfg(windows)]
mod imp {
    unsafe extern "system" {
        pub(super) fn QueryPerformanceCounter(counter: *mut i64) -> i32;
        pub(super) fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
    }

    #[link(name = "winmm")]
    unsafe extern "system" {
        pub(super) fn timeBeginPeriod(period: u32) -> u32;
        pub(super) fn timeEndPeriod(period: u32) -> u32;
    }
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

/// The standard library's sleep, which is a high-resolution waitable timer on
/// Windows: already the deadline-bounded sleep this needs.
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

    /// The named clock in microseconds, not another unit: an interval read
    /// on it agrees with the same interval read on ours.
    #[cfg(any(unix, windows))]
    #[test]
    fn the_named_clock_counts_microseconds() {
        let begin = Time::now();
        let first = monotonic_us();
        precise_sleep(Duration::from_millis(20));
        let second = monotonic_us();
        let ours_us = elapsed_ms(begin) * 1000.0;
        let named_us = (second - first) as f64;
        assert!(
            named_us >= 20_000.0,
            "read {named_us} us over a 20 ms sleep"
        );
        assert!(
            (ours_us - named_us).abs() < 2_000.0,
            "{named_us} us against {ours_us}"
        );
    }
}
