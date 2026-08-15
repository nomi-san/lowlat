//! Address-based wait and wake.
//!
//! **Producers must use [`notify_one`] or [`notify_all`] from this module, and
//! never the standard library's atomic notify.** The standard library keeps its
//! own waiter registry and skips the kernel wake when it sees no registered
//! waiter, so a raw futex sleeper is never woken and escapes only on timeout.
//! That turned a notify-driven pipeline into a timeout-polled one and delivered
//! frames in 104 ms bursts. The wait and the wake are one primitive; they live
//! in one module so they cannot drift apart.
//!
//! Two further rules:
//!
//! - A sub-millisecond timeout **rounds up to one millisecond**, so a wait can
//!   never degenerate into a hot poll.
//! - Spurious wakes are permitted. Callers recheck their predicate in a loop.

use core::sync::atomic::AtomicU32;
use core::time::Duration;

const MINIMUM_TIMEOUT: Duration = Duration::from_millis(1);

/// Sleep until `atom` changes away from `expected`, `timeout` elapses, or a
/// spurious wake occurs.
///
/// Returns immediately if `atom` already differs from `expected`.
pub fn wait(atom: &AtomicU32, expected: u32, timeout: Duration) {
    if timeout.is_zero() {
        return;
    }
    let timeout = if timeout < MINIMUM_TIMEOUT {
        MINIMUM_TIMEOUT
    } else {
        timeout
    };
    imp::wait(atom, expected, timeout);
}

/// Wake one waiter on `atom`.
pub fn notify_one(atom: &AtomicU32) {
    imp::wake(atom, 1);
}

/// Wake every waiter on `atom`. Used on teardown, where stranding a waiter
/// turns a clean disconnect into a multi-second hang.
pub fn notify_all(atom: &AtomicU32) {
    imp::wake(atom, i32::MAX);
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{AtomicU32, Duration};

    /// `FUTEX_WAIT` (0) or'd with `FUTEX_PRIVATE_FLAG` (128).
    const FUTEX_WAIT_PRIVATE: libc::c_int = 128;
    /// `FUTEX_WAKE` (1) or'd with `FUTEX_PRIVATE_FLAG` (128).
    const FUTEX_WAKE_PRIVATE: libc::c_int = 129;

    pub(super) fn wait(atom: &AtomicU32, expected: u32, timeout: Duration) {
        // A relative timeout, which is what the WAIT operation takes.
        let deadline = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        // SAFETY: the atomic outlives the call, the timespec is valid, and the
        // trailing arguments are ignored by the WAIT operation.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                core::ptr::from_ref(atom).cast::<u32>(),
                FUTEX_WAIT_PRIVATE,
                expected,
                core::ptr::from_ref(&deadline),
                core::ptr::null::<u32>(),
                0u32,
            );
        }
    }

    pub(super) fn wake(atom: &AtomicU32, count: i32) {
        // SAFETY: as above; WAKE ignores the timeout argument.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                core::ptr::from_ref(atom).cast::<u32>(),
                FUTEX_WAKE_PRIVATE,
                count,
                core::ptr::null::<libc::timespec>(),
                core::ptr::null::<u32>(),
                0u32,
            );
        }
    }
}

/// Portable fallback: a fixed table of buckets keyed by address.
///
/// Correct but coarser than a futex, since unrelated addresses can share a
/// bucket and produce extra wakes. Callers recheck their predicate, so extra
/// wakes are harmless. Linux gets the real thing above; this exists so the
/// workspace builds and tests anywhere.
#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{AtomicU32, Duration};
    use core::sync::atomic::Ordering;
    use std::sync::{Condvar, Mutex};

    const BUCKETS: usize = 64;

    struct Bucket {
        generation: Mutex<u64>,
        signal: Condvar,
    }

    impl Bucket {
        const fn new() -> Self {
            Self {
                generation: Mutex::new(0),
                signal: Condvar::new(),
            }
        }
    }

    static TABLE: [Bucket; BUCKETS] = [const { Bucket::new() }; BUCKETS];

    fn bucket(atom: &AtomicU32) -> &'static Bucket {
        let address = core::ptr::from_ref(atom) as usize;
        let index = (address / align_of::<AtomicU32>()) % BUCKETS;
        &TABLE[index]
    }

    pub(super) fn wait(atom: &AtomicU32, expected: u32, timeout: Duration) {
        let bucket = bucket(atom);
        let Ok(guard) = bucket.generation.lock() else {
            return;
        };
        // Recheck under the lock: a wake between our caller's check and here
        // would otherwise be missed.
        if atom.load(Ordering::Acquire) != expected {
            return;
        }
        let _ = bucket.signal.wait_timeout(guard, timeout);
    }

    pub(super) fn wake(atom: &AtomicU32, _count: i32) {
        let bucket = bucket(atom);
        if let Ok(mut generation) = bucket.generation.lock() {
            *generation = generation.wrapping_add(1);
        }
        // Wake everyone: bucket sharing means we cannot tell which waiter is
        // ours, and a spurious wake is permitted.
        bucket.signal.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn returns_immediately_when_the_value_already_changed() {
        let flag = AtomicU32::new(1);
        let begin = Instant::now();
        wait(&flag, 0, Duration::from_secs(5));
        assert!(
            begin.elapsed() < Duration::from_millis(500),
            "waited on a value that had already changed"
        );
    }

    #[test]
    fn zero_timeout_does_not_block() {
        let flag = AtomicU32::new(0);
        let begin = Instant::now();
        wait(&flag, 0, Duration::ZERO);
        assert!(begin.elapsed() < Duration::from_millis(100));
    }

    /// The pairing rule, as a test: a waiter parked by this module must be
    /// woken by this module's notify, well before its timeout.
    #[test]
    fn notify_wakes_a_waiter_before_the_timeout() {
        let flag = Arc::new(AtomicU32::new(0));
        let waiter_flag = Arc::clone(&flag);

        let waiter = std::thread::spawn(move || {
            let begin = Instant::now();
            while waiter_flag.load(Ordering::Acquire) == 0 {
                wait(&waiter_flag, 0, Duration::from_secs(10));
            }
            begin.elapsed()
        });

        std::thread::sleep(Duration::from_millis(50));
        flag.store(1, Ordering::Release);
        notify_all(&flag);

        let waited = waiter.join().unwrap();
        assert!(
            waited < Duration::from_secs(5),
            "waiter ran to its timeout instead of being woken: {waited:?}"
        );
    }

    #[test]
    fn sub_millisecond_timeout_is_rounded_up() {
        let flag = AtomicU32::new(0);
        let begin = Instant::now();
        wait(&flag, 0, Duration::from_micros(50));
        assert!(
            begin.elapsed() >= Duration::from_micros(500),
            "a sub-millisecond wait returned instantly, which is a hot poll"
        );
    }
}
