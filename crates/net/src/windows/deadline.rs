//! The pass's deadline: a high-resolution timer tied to the loop's completion
//! port, so its expiry arrives as an entry beside the receives and the wake.
//!
//! **The port's own timeout ends on the system's tick, never on the deadline.**
//! That tick is about 15.6 ms until the process raises the timer resolution,
//! and with it raised to a millisecond the timeout can end up to a millisecond
//! *before* the time asked for, which the pass pays a second wake to reach. A
//! timer made high-resolution keeps its own time whatever the process has
//! asked of the system: tied to the port it landed 0 to 1 ms after its
//! deadline and never before it, at one wake a deadline, with the resolution
//! raised or not (measured 2026-11-14 on Windows 10 19045, docs/02-io-shell.md
//! 2). So the loop's deadlines do not depend on the request a library handle
//! makes for its process, nor on the system honouring it.
//!
//! **Tied through a wait completion packet**: the system queues the packet's
//! entry on the port when the timer fires. The packet's three calls are the
//! native layer's own and in no import library, so they are found at run time,
//! as the QoS service is. Where they are missing, or the system has no such
//! timer (before Windows 10 1803), the port's own timeout stands, as before
//! this existed, and the session says so once.
//!
//! **A tie queues one entry, and whether it has is tracked here**, since the
//! loop is the port's only consumer. Measured on the same system:
//! - setting the timer again while it is tied moves the deadline, in one call
//!   (265 ns) -- the ordinary case, a wait ended early by a datagram;
//! - a tie that finds the timer fired already says so and queues the entry at
//!   once all the same;
//! - once the deadline has passed its entry may sit on the port unseen, and a
//!   cancel takes it back, so a deadline that moved on is not announced by
//!   the one before it; the cancel, the set and the tie together cost 820 ns.

use core::ffi::c_void;
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

use lowlat_common::dynlib::Library;
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_ALL, GetLastError, HANDLE, NTSTATUS, STATUS_CANCELLED, STATUS_PENDING,
    STATUS_SUCCESS,
};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
    TIMER_ALL_ACCESS,
};

use super::KEY_DEADLINE;
use crate::shell::MIN_WAIT_MS;

/// How far past the deadline the port's own timeout is set while the timer
/// is armed: two of the system's default ticks, since that timeout may end a
/// tick early. It ends the wait only if the timer's entry never comes.
const BACKSTOP_MS: u32 = 32;

type Create = unsafe extern "system" fn(*mut HANDLE, u32, *const c_void) -> NTSTATUS;
type Associate = unsafe extern "system" fn(
    HANDLE,
    HANDLE,
    HANDLE,
    *const c_void,
    *const c_void,
    NTSTATUS,
    usize,
    *mut u8,
) -> NTSTATUS;
type Cancel = unsafe extern "system" fn(HANDLE, u8) -> NTSTATUS;

/// The packet's three calls, and the library they came from, kept together.
struct Api {
    create: Create,
    associate: Associate,
    cancel: Cancel,
    _library: Library,
}

/// The calls, resolved once per process, or `None` where any is missing.
fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let library = Library::open_system(c"ntdll.dll")?;
        // SAFETY: each type is the call's signature and calling convention as
        // the native layer defines it, and each pointer is kept beside the
        // library it came from.
        unsafe {
            Some(Api {
                create: library.symbol(c"NtCreateWaitCompletionPacket")?,
                associate: library.symbol(c"NtAssociateWaitCompletionPacket")?,
                cancel: library.symbol(c"NtCancelWaitCompletionPacket")?,
                _library: library,
            })
        }
    })
    .as_ref()
}

/// A call that failed, and what it said.
type Failed = (&'static str, i32);

/// What arming left the wait to do.
enum Armed {
    /// The entry will come when the timer fires.
    Tied,
    /// The timer had fired by the time it was tied: its entry is on the port.
    Due,
    /// The entry before could not be taken back and is on its way.
    Unsettled,
}

/// The timer, its packet, and whether the packet's entry is still to be seen.
struct Timer {
    api: &'static Api,
    timer: HANDLE,
    packet: HANDLE,
    /// The packet is tied to the timer: its entry is still to come, or sits
    /// on the port unseen.
    tied: bool,
    /// No sooner than this does the timer fire. Past it, the entry may be on
    /// the port.
    due: Instant,
}

impl Timer {
    fn new() -> Option<Self> {
        let api = api()?;
        // SAFETY: an unnamed timer with default security, ours alone.
        let timer = unsafe {
            CreateWaitableTimerExW(
                core::ptr::null(),
                core::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if timer.is_null() {
            return None;
        }
        let mut packet: HANDLE = core::ptr::null_mut();
        // SAFETY: writes one handle; no attributes are named.
        let status = unsafe { (api.create)(&raw mut packet, GENERIC_ALL, core::ptr::null()) };
        if status != STATUS_SUCCESS || packet.is_null() {
            // SAFETY: the timer made above, closed once.
            unsafe { CloseHandle(timer) };
            return None;
        }
        Some(Self {
            api,
            timer,
            packet,
            tied: false,
            due: Instant::now(),
        })
    }

    /// Set the timer `timeout_ms` from now and see it tied to `port`.
    fn arm(&mut self, port: HANDLE, timeout_ms: f64) -> Result<Armed, Failed> {
        let now = Instant::now();
        if self.tied && now >= self.due {
            // SAFETY: our own packet; the flag takes back an entry already
            // queued, as well as a tie not yet fired.
            match unsafe { (self.api.cancel)(self.packet, 1) } {
                // Taken back, or nothing was tied after all.
                STATUS_SUCCESS | STATUS_CANCELLED => self.tied = false,
                // Being queued as we asked: it arrives, and this wait takes
                // the port's own timeout.
                STATUS_PENDING => return Ok(Armed::Unsettled),
                status => return Err(("cancel", status)),
            }
        }
        // Relative, in the timer's hundreds of nanoseconds, and rounded up:
        // rounded down, the pass wakes just before the deadline it armed and
        // pays a second wake to act on it.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "clamped to the wait bounds by the caller"
        )]
        let due = -((timeout_ms * 10_000.0).ceil() as i64);
        // SAFETY: our own timer, a due time that outlives the call, and no
        // completion routine.
        let set =
            unsafe { SetWaitableTimer(self.timer, &raw const due, 0, None, core::ptr::null(), 0) };
        if set == 0 {
            // SAFETY: reads the calling thread's last error.
            let code = unsafe { GetLastError() };
            return Err(("set", i32::try_from(code).unwrap_or(i32::MAX)));
        }
        // Read before the set, so the timer is never due sooner than this.
        self.due = now + Duration::from_secs_f64(timeout_ms / 1000.0);
        if self.tied {
            // The tie follows the timer to its new time.
            return Ok(Armed::Tied);
        }
        let mut fired: u8 = 0;
        // SAFETY: our own packet and timer, the loop's port, and one byte to
        // write; the key is a number, never dereferenced.
        let status = unsafe {
            (self.api.associate)(
                self.packet,
                port,
                self.timer,
                core::ptr::without_provenance(KEY_DEADLINE),
                core::ptr::null(),
                STATUS_SUCCESS,
                0,
                &raw mut fired,
            )
        };
        if status != STATUS_SUCCESS {
            return Err(("tie", status));
        }
        // Fired already or not, the entry is queued once: now, or when it does.
        self.tied = true;
        Ok(if fired == 0 { Armed::Tied } else { Armed::Due })
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // SAFETY: our own packet and timer, each closed once; the cancel
        // takes back an entry the loop will not be there to see.
        unsafe {
            if self.tied {
                (self.api.cancel)(self.packet, 1);
            }
            CloseHandle(self.packet);
            CloseHandle(self.timer);
        }
    }
}

/// The loop's deadline, armed by each wait that blocks.
///
/// Dropped before the port it ties its timer to.
pub(super) struct Deadline {
    /// `None` where the system has no such timer or no way to tie one to a
    /// port: the port's own timeout then. Boxed, once at setup, so the loop's
    /// storage is no larger for it than a pointer.
    timer: Option<Box<Timer>>,
}

impl core::fmt::Debug for Deadline {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Deadline")
            .field("timed", &self.timer.is_some())
            .field("tied", &self.timer.as_ref().is_some_and(|timer| timer.tied))
            .finish()
    }
}

// SAFETY: the timer and the packet are process-wide kernel objects, not
// thread-affine, and they are used only by the thread that runs the loop.
unsafe impl Send for Deadline {}

impl Deadline {
    /// Never fails: without the timer the port's own timeout stands, said
    /// once per process.
    pub(super) fn new() -> Self {
        let timer = Timer::new().map(Box::new);
        if timer.is_none() {
            static SAID: Once = Once::new();
            SAID.call_once(|| {
                lowlat_common::log_warn!(
                    "net: no high-resolution timer for the loop, its waits end on the system tick"
                );
            });
        }
        Self { timer }
    }

    /// Arm the deadline `timeout_ms` from now on `port`, and say how long the
    /// port's own wait may then run, in its milliseconds.
    ///
    /// Never shorter than the loop's shortest wait. With the timer armed the
    /// port's timeout is a backstop past the deadline; without it, it is the
    /// deadline, rounded up to the port's unit.
    pub(super) fn arm(&mut self, port: HANDLE, timeout_ms: f64) -> u32 {
        let timeout_ms = timeout_ms.max(MIN_WAIT_MS);
        // Rounded up, never truncated, for the reason the timer's own unit is.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to the wait bounds by the caller"
        )]
        let whole = timeout_ms.ceil() as u32;
        let Some(timer) = self.timer.as_mut() else {
            return whole;
        };
        match timer.arm(port, timeout_ms) {
            Ok(Armed::Tied) => whole.saturating_add(BACKSTOP_MS),
            Ok(Armed::Due) => 0,
            Ok(Armed::Unsettled) => whole,
            Err((call, code)) => {
                // Nothing a session should end over: the port's own timeout
                // takes over for the rest of this loop's life.
                lowlat_common::log_warn!(
                    "net: the loop's timer failed, call={} code={:#x}; its waits end on the system tick",
                    call,
                    code
                );
                self.timer = None;
                whole
            }
        }
    }

    /// The deadline's entry came off the port: the packet is free to be tied
    /// again.
    pub(super) fn fired(&mut self) {
        if let Some(timer) = self.timer.as_mut() {
            timer.tied = false;
        }
    }

    /// Whether the deadline is a timer here, rather than the port's timeout.
    #[cfg(test)]
    pub(super) fn timed(&self) -> bool {
        self.timer.is_some()
    }
}
