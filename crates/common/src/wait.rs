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

/// The system's address wait, which is the futex under another name.
///
/// **Its timeout is quantised to the system tick** unless the process has
/// raised the timer resolution, which is what
/// [`crate::clock::TimerResolution`] is for.
#[cfg(windows)]
mod imp {
    use super::{AtomicU32, Duration};
    use core::ffi::c_void;

    #[link(name = "synchronization")]
    unsafe extern "system" {
        fn WaitOnAddress(
            address: *const c_void,
            compare: *const c_void,
            size: usize,
            milliseconds: u32,
        ) -> i32;
        fn WakeByAddressSingle(address: *const c_void);
        fn WakeByAddressAll(address: *const c_void);
    }

    /// The largest finite timeout. One more is `INFINITE`.
    const LONGEST_MS: u32 = u32::MAX - 1;

    pub(super) fn wait(atom: &AtomicU32, expected: u32, timeout: Duration) {
        // Rounded up, so a wait never ends before the deadline it was armed
        // for and the caller pays a second wake to act on it.
        let milliseconds = u32::try_from(timeout.as_micros().div_ceil(1000))
            .map_or(LONGEST_MS, |ms| ms.min(LONGEST_MS));
        // SAFETY: the atomic outlives the call and `expected` is a live
        // local; both are four bytes, which is the size passed.
        unsafe {
            WaitOnAddress(
                core::ptr::from_ref(atom).cast(),
                core::ptr::from_ref(&expected).cast(),
                size_of::<u32>(),
                milliseconds,
            );
        }
    }

    pub(super) fn wake(atom: &AtomicU32, count: i32) {
        let address = core::ptr::from_ref(atom).cast();
        // SAFETY: waking an address nobody waits on is permitted.
        unsafe {
            if count == 1 {
                WakeByAddressSingle(address);
            } else {
                WakeByAddressAll(address);
            }
        }
    }
}

/// The system's address wait: the public one where the system has it (14.4
/// on), the private one below that, each found by name at run time, since a
/// symbol linked by name and absent stops the load on an older system.
///
/// **Its timeout is coalesced**: the system lets a timed wait run late by an
/// eighth to a quarter of its length (measured, see
/// [`crate::clock::precise_sleep`]); a wake is not delayed. Measured
/// 2026-10-29, a wake reached a waiter parked in the kernel in 17 us at the
/// median through either pair, no sooner than through the bucket table below:
/// what this buys is a wait per address, with no shared bucket and no wake of
/// every waiter on one.
#[cfg(target_os = "macos")]
mod imp {
    use super::{AtomicU32, Duration};
    use core::ffi::{CStr, c_int, c_void};
    use std::sync::OnceLock;

    type PublicWait = unsafe extern "C" fn(*mut c_void, u64, usize, u32, u32, u64) -> c_int;
    type PublicWake = unsafe extern "C" fn(*mut c_void, usize, u32) -> c_int;
    type PrivateWait = unsafe extern "C" fn(u32, *mut c_void, u64, u64, u64) -> c_int;
    type PrivateWake = unsafe extern "C" fn(u32, *mut c_void, u64) -> c_int;

    /// The clock a public wait's timeout is measured on.
    const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;
    /// The private wait's compare-and-wait on a 32-bit value.
    const UL_COMPARE_AND_WAIT: u32 = 1;
    /// The private wake's flag for every waiter rather than one.
    const ULF_WAKE_ALL: u32 = 0x100;
    /// The private calls' flag for an error returned rather than set in errno.
    const ULF_NO_ERRNO: u32 = 0x0100_0000;

    #[derive(Clone, Copy)]
    pub(super) enum System {
        Public {
            wait: PublicWait,
            wake_one: PublicWake,
            wake_all: PublicWake,
        },
        Private {
            wait: PrivateWait,
            wake: PrivateWake,
        },
    }

    fn find(name: &CStr) -> *mut c_void {
        // SAFETY: a lookup in the process's own namespace by a valid name.
        unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }
    }

    pub(super) fn public() -> Option<System> {
        let (wait, wake_one, wake_all) = (
            find(c"os_sync_wait_on_address_with_timeout"),
            find(c"os_sync_wake_by_address_any"),
            find(c"os_sync_wake_by_address_all"),
        );
        if wait.is_null() || wake_one.is_null() || wake_all.is_null() {
            return None;
        }
        // SAFETY: the signatures are the system's own, from its header.
        unsafe {
            Some(System::Public {
                wait: core::mem::transmute::<*mut c_void, PublicWait>(wait),
                wake_one: core::mem::transmute::<*mut c_void, PublicWake>(wake_one),
                wake_all: core::mem::transmute::<*mut c_void, PublicWake>(wake_all),
            })
        }
    }

    pub(super) fn private() -> Option<System> {
        let (wait, wake) = (find(c"__ulock_wait2"), find(c"__ulock_wake"));
        if wait.is_null() || wake.is_null() {
            return None;
        }
        // SAFETY: the signatures are the kernel's own, unchanged since 11.
        unsafe {
            Some(System::Private {
                wait: core::mem::transmute::<*mut c_void, PrivateWait>(wait),
                wake: core::mem::transmute::<*mut c_void, PrivateWake>(wake),
            })
        }
    }

    /// Found once. Every supported system has the private pair, so `None`
    /// is a system below the floor, where a wait sleeps out its timeout.
    fn system() -> Option<System> {
        static FOUND: OnceLock<Option<System>> = OnceLock::new();
        *FOUND.get_or_init(|| public().or_else(private))
    }

    pub(super) fn wait(atom: &AtomicU32, expected: u32, timeout: Duration) {
        match system() {
            Some(system) => wait_with(system, atom, expected, timeout),
            None => std::thread::sleep(timeout),
        }
    }

    pub(super) fn wake(atom: &AtomicU32, count: i32) {
        if let Some(system) = system() {
            wake_with(system, atom, count);
        }
    }

    pub(super) fn wait_with(system: System, atom: &AtomicU32, expected: u32, timeout: Duration) {
        let address = core::ptr::from_ref(atom).cast_mut().cast();
        // Never zero: the caller rounds a timeout up to a millisecond, and
        // zero would be an error to one call and forever to the other.
        let nanos = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX).max(1);
        // SAFETY: the atomic outlives the call and is four bytes, which is the
        // size passed; the return is ignored because every outcome -- woken,
        // changed, timed out, interrupted -- sends the caller to recheck.
        unsafe {
            match system {
                System::Public { wait, .. } => {
                    wait(
                        address,
                        u64::from(expected),
                        size_of::<u32>(),
                        0,
                        OS_CLOCK_MACH_ABSOLUTE_TIME,
                        nanos,
                    );
                }
                System::Private { wait, .. } => {
                    wait(
                        UL_COMPARE_AND_WAIT | ULF_NO_ERRNO,
                        address,
                        u64::from(expected),
                        nanos,
                        0,
                    );
                }
            }
        }
    }

    pub(super) fn wake_with(system: System, atom: &AtomicU32, count: i32) {
        let address = core::ptr::from_ref(atom).cast_mut().cast();
        // SAFETY: waking an address nobody waits on is permitted, and
        // reported as an error that means exactly that.
        unsafe {
            match system {
                System::Public {
                    wake_one, wake_all, ..
                } => {
                    let wake = if count == 1 { wake_one } else { wake_all };
                    wake(address, size_of::<u32>(), 0);
                }
                System::Private { wake, .. } => {
                    let all = if count == 1 { 0 } else { ULF_WAKE_ALL };
                    wake(UL_COMPARE_AND_WAIT | ULF_NO_ERRNO | all, address, 0);
                }
            }
        }
    }
}

/// Portable fallback: a fixed table of buckets keyed by address.
///
/// Correct but coarser than a futex, since unrelated addresses can share a
/// bucket and produce extra wakes. Callers recheck their predicate, so extra
/// wakes are harmless. Linux, Windows and macOS get the real thing above; this
/// exists so the workspace builds and tests anywhere else.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
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

    /// Both of the system's pairs wake a waiter parked on them, well before
    /// its timeout: the public one the test above runs through, and the
    /// private one below it, which a system that has the public one never
    /// runs, so it is driven here by name.
    #[cfg(target_os = "macos")]
    #[test]
    fn each_of_the_systems_waits_is_woken() {
        for (name, system) in [("public", imp::public()), ("private", imp::private())] {
            let system = system.unwrap_or_else(|| panic!("the {name} pair was not found"));
            let flag = Arc::new(AtomicU32::new(0));
            let waiter_flag = Arc::clone(&flag);
            let waiter = std::thread::spawn(move || {
                let begin = Instant::now();
                while waiter_flag.load(Ordering::Acquire) == 0 {
                    imp::wait_with(system, &waiter_flag, 0, Duration::from_secs(10));
                }
                begin.elapsed()
            });
            std::thread::sleep(Duration::from_millis(50));
            flag.store(1, Ordering::Release);
            imp::wake_with(system, &flag, 1);
            let waited = waiter.join().unwrap();
            assert!(
                waited < Duration::from_secs(5),
                "the {name} pair ran to its timeout instead of being woken: {waited:?}"
            );
        }
    }

    /// **The middle of ten waits, not one.** A wait may return early -- a
    /// spurious wake is part of the contract above, and a CI runner's single
    /// wait once ended under half a millisecond -- while a hot poll returns
    /// within microseconds every time, so the median tells the two apart
    /// where one sample cannot. The bound sits ten times above a poll and
    /// well below any wait of a millisecond, whatever the system's tick.
    #[test]
    fn sub_millisecond_timeout_is_rounded_up() {
        let flag = AtomicU32::new(0);
        let mut waited: Vec<Duration> = (0..10)
            .map(|_| {
                let begin = Instant::now();
                wait(&flag, 0, Duration::from_micros(50));
                begin.elapsed()
            })
            .collect();
        waited.sort();
        assert!(
            waited[5] >= Duration::from_micros(250),
            "sub-millisecond waits returned at once, which is a hot poll: {waited:?}"
        );
    }
}
