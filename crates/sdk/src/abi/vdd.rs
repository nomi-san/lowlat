//! The third half of the public C ABI: the virtual displays of the host's own
//! display driver, on Windows ([06-api.md](../../../docs/06-api.md) section
//! 3c, [11-drivers.md](../../../docs/11-drivers.md) section 1).
//!
//! Naming follows the header rather than Rust convention, which is permitted
//! here and nowhere else.

use core::ffi::c_char;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use lowlat_vdd_ctl::{
    Controller, Error, Gpu, GpuChoice, Lifetime, Mode, Monitor, Pointer, Settings,
};

use super::guard;
use super::lowlat_status::{self, *};

/// A monitor bound to the handle that plugged it: unplugged when the handle
/// is closed or its process ends.
pub const LOWLAT_VDD_BOUND: u32 = 0;
/// A monitor kept until removed, owned by no handle.
pub const LOWLAT_VDD_KEPT: u32 = 1;
/// No slot asked for: the first free one.
pub const LOWLAT_VDD_SLOT_ANY: u32 = u32::MAX;
/// How many monitors the driver carries, each a slot from zero.
pub const LOWLAT_VDD_SLOTS: u32 = 16;
/// Bytes in a display's name, its terminator included.
pub const LOWLAT_VDD_NAME_MAX: usize = 32;
/// The custom modes the settings hold.
pub const LOWLAT_VDD_MODES_MAX: usize = 16;
/// Bytes in an EDID the settings hold: one block or two.
pub const LOWLAT_VDD_EDID_MAX: usize = 256;

/// One controller, as the application holds it. The monitors it plugs bound
/// are its own.
///
/// Opaque: the application holds a pointer it cannot look inside.
#[derive(Debug)]
pub struct lowlat_vdd {
    /// Set when a call was contained, and never cleared.
    poisoned: AtomicBool,
    /// Calls on one handle take turns.
    held: Mutex<Controller>,
}

/// What [`lowlat_vdd_plug`] is asked for.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_vdd_plug_info {
    /// `sizeof(lowlat_vdd_plug_info)`, set by the caller.
    pub size: u32,
    /// [`LOWLAT_VDD_BOUND`] or [`LOWLAT_VDD_KEPT`].
    pub lifetime: u32,
    /// A kept monitor the driver plugs again whenever it starts.
    pub restore: bool,
    /// The slot, whose identity the system remembers the monitor's place
    /// and mode by; [`LOWLAT_VDD_SLOT_ANY`] for the first free one.
    pub slot: u32,
    /// The mode it arrives at, first and preferred in its list.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// One monitor, as the driver and the system see it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_vdd_monitor {
    /// `sizeof(lowlat_vdd_monitor)`, set by the caller.
    pub size: u32,
    pub slot: u32,
    /// Plugged through this handle and bound to it.
    pub owned: bool,
    pub kept: bool,
    /// Kept, and plugged again whenever the driver starts.
    pub restore: bool,
    /// Shown on the desktop.
    pub active: bool,
    /// The mode now when shown, else the one the driver holds for it.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Its top left on the desktop, when shown.
    pub x: i32,
    pub y: i32,
    /// The system's name for the display when shown, `\\.\DISPLAYn`; empty
    /// when not. UTF-8, terminated.
    pub name: [c_char; LOWLAT_VDD_NAME_MAX],
    /// The GPU the system renders it on, its LUID packed high part above low;
    /// zero before its first picture.
    pub render_adapter: u64,
    /// Pictures the system has handed the driver.
    pub frames: u64,
}

/// A size and a whole refresh rate.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct lowlat_vdd_mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// The settings every monitor shares, kept in a file the driver reads when it
/// starts.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_vdd_settings {
    /// `sizeof(lowlat_vdd_settings)`, set by the caller.
    pub size: u32,
    /// The GPU the monitors render on, by its maker's and its own number;
    /// both zero for the system's high-performance GPU. Zero too when the
    /// file names a maker or the power-saving GPU, which these cannot say;
    /// written back as read, that choice stands.
    pub gpu_vendor: u32,
    pub gpu_device: u32,
    /// The pointer drawn into the pictures rather than reported beside them.
    pub pointer_drawn: bool,
    /// Custom modes every monitor plugged after lists.
    pub mode_count: u32,
    pub modes: [lowlat_vdd_mode; LOWLAT_VDD_MODES_MAX],
    /// A real monitor's EDID for every slot, one block or two; zero bytes for
    /// the driver's own. A different EDID makes each slot a new monitor to
    /// the system.
    pub edid_len: u32,
    pub edid: [u8; LOWLAT_VDD_EDID_MAX],
}

fn refused(error: Error) -> lowlat_status {
    match error {
        Error::Absent => LOWLAT_ERR_VDD_ABSENT,
        Error::Version(_) => LOWLAT_ERR_VDD_VERSION,
        Error::Denied => LOWLAT_ERR_VDD_DENIED,
        Error::Starting => LOWLAT_ERR_VDD_STARTING,
        Error::Full => LOWLAT_ERR_VDD_FULL,
        Error::SlotTaken => LOWLAT_ERR_VDD_SLOT_TAKEN,
        Error::NotOwner => LOWLAT_ERR_VDD_NOT_OWNER,
        Error::NoMonitor => LOWLAT_ERR_VDD_NO_MONITOR,
        Error::Mode | Error::Argument => LOWLAT_ERR_INVALID_ARGUMENT,
        Error::Busy => LOWLAT_ERR_VDD_BUSY,
        Error::Display(_) => LOWLAT_ERR_VDD_DISPLAY_CONFIG,
        Error::NotReady(_) => LOWLAT_ERR_VDD_NOT_READY,
        Error::File => LOWLAT_ERR_VDD_FILE,
        Error::Io(_) => LOWLAT_ERR_VDD_IO,
    }
}

/// Copy `text` into a terminated array, cut to fit.
fn put(into: &mut [c_char], text: &str) {
    let room = into.len().saturating_sub(1);
    let bytes = text.as_bytes().get(..room.min(text.len())).unwrap_or(&[]);
    for (to, from) in into.iter_mut().zip(bytes) {
        *to = c_char::from_ne_bytes([*from]);
    }
    if let Some(end) = into.get_mut(bytes.len()) {
        *end = 0;
    }
}

/// Fill the caller's monitor, which must be at least this minor's size.
///
/// # Safety
///
/// `out` is null or points at a `lowlat_vdd_monitor` the caller owns.
unsafe fn fill(out: *mut lowlat_vdd_monitor, monitor: &Monitor) -> lowlat_status {
    let Some(out) = (unsafe { out.as_mut() }) else {
        return LOWLAT_OK;
    };
    if (out.size as usize) < size_of::<lowlat_vdd_monitor>() {
        return LOWLAT_ERR_INVALID_ARGUMENT;
    }
    let size = out.size;
    *out = lowlat_vdd_monitor {
        size,
        slot: u32::from(monitor.slot),
        owned: monitor.owned,
        kept: monitor.kept,
        restore: monitor.restore,
        active: monitor.active,
        width: monitor.mode.width,
        height: monitor.mode.height,
        refresh_hz: monitor.mode.refresh_hz,
        x: monitor.x,
        y: monitor.y,
        name: [0; LOWLAT_VDD_NAME_MAX],
        render_adapter: monitor.render_luid,
        frames: monitor.frames,
    };
    put(&mut out.name, &monitor.name);
    LOWLAT_OK
}

/// A refusal's status. **On `LOWLAT_ERR_VDD_NOT_READY` the monitor is
/// plugged** and the system has not finished, so `out`, when given and large
/// enough, holds it as it stands (minor 30): its slot first of all, which a
/// plug that asked for none has no other way to learn.
///
/// # Safety
///
/// `out` is null or points at a `lowlat_vdd_monitor` the caller owns.
unsafe fn refused_into(
    controller: &Controller,
    error: Error,
    out: *mut lowlat_vdd_monitor,
) -> lowlat_status {
    if let Error::NotReady(slot) = error
        && let Ok(monitor) = controller.monitor(slot)
    {
        // A short `out` is left as it is: the status is the news.
        let _ = unsafe { fill(out, &monitor) };
    }
    refused(error)
}

fn slot_of(slot: u32) -> Option<u8> {
    u8::try_from(slot)
        .ok()
        .filter(|slot| u32::from(*slot) < LOWLAT_VDD_SLOTS)
}

/// The pair a caller is told for the file's GPU: an exact GPU's own, and
/// zero for any other choice, which the pair cannot say.
fn told(gpu: Option<GpuChoice>) -> (u32, u32) {
    match gpu {
        Some(GpuChoice::Exact(gpu)) => (u32::from(gpu.vendor), u32::from(gpu.device)),
        _ => (0, 0),
    }
}

/// The file's GPU once a caller wrote `pair`, which reads as `asked`.
///
/// **The pair read, written back, keeps the choice it stood for**: a maker's
/// choice or the power-saving one reads as zero, and writing that back must
/// not turn it into the high-performance GPU behind its chooser's back.
fn written_back(
    current: Option<GpuChoice>,
    pair: (u32, u32),
    asked: Option<GpuChoice>,
) -> Option<GpuChoice> {
    if told(current) == pair {
        current
    } else {
        asked
    }
}

/// Open the display driver.
///
/// @param[out] out The new handle; the monitors it plugs bound are its own.
/// @returns [`LOWLAT_ERR_VDD_ABSENT`] when the driver is not installed or not
/// started, [`LOWLAT_ERR_VDD_VERSION`] when it is another version than this
/// library's, [`LOWLAT_ERR_VDD_DENIED`] when it refuses this account.
///
/// # Safety
///
/// `out` points at writable storage for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_open(out: *mut *mut lowlat_vdd) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        if out.is_null() {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        let controller = match Controller::open() {
            Ok(controller) => controller,
            Err(error) => return refused(error),
        };
        let handle = Box::new(lowlat_vdd {
            poisoned: AtomicBool::new(false),
            held: Mutex::new(controller),
        });
        unsafe { out.write(Box::into_raw(handle)) };
        LOWLAT_OK
    })
}

/// Close a handle, unplugging every monitor bound to it.
///
/// **Works on a poisoned handle.**
///
/// @param[in] vdd The handle, not used again. Null does nothing.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`] and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_close(vdd: *mut lowlat_vdd) {
    guard((), || {
        if vdd.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(vdd) });
    });
}

/// Run an entry point that needs the handle: null refused, a poisoned handle
/// refused, a panic poisoning it.
///
/// # Safety
///
/// `vdd` is null or came from [`lowlat_vdd_open`].
unsafe fn entered(
    vdd: *mut lowlat_vdd,
    call: impl FnOnce(&mut Controller) -> lowlat_status,
) -> lowlat_status {
    let Some(handle) = (unsafe { vdd.as_ref() }) else {
        return LOWLAT_ERR_INVALID_ARGUMENT;
    };
    if handle.poisoned.load(Ordering::Acquire) {
        return LOWLAT_ERR_POISONED;
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut held = handle.held.lock().unwrap_or_else(|held| held.into_inner());
        call(&mut held)
    })) {
        Ok(status) => status,
        Err(_) => {
            handle.poisoned.store(true, Ordering::Release);
            lowlat_common::log_error!("abi: a call panicked, contained and the handle poisoned");
            LOWLAT_ERR_INTERNAL
        }
    }
}

/// Plug a monitor and wait, within a few seconds, until the system shows it
/// at its mode.
///
/// @param[in] vdd The handle.
/// @param[in] plug What to plug.
/// @param[out] out The monitor as plugged, or from minor 30 as it stands on
/// [`LOWLAT_ERR_VDD_NOT_READY`]; may be null.
/// @returns [`LOWLAT_ERR_VDD_FULL`] when every slot is taken,
/// [`LOWLAT_ERR_VDD_SLOT_TAKEN`] when the slot asked for is,
/// [`LOWLAT_ERR_VDD_DISPLAY_CONFIG`] when the system's display configuration
/// refused the monitor, which is then unplugged again, and
/// [`LOWLAT_ERR_VDD_NOT_READY`] when the monitor is plugged and the system has
/// not finished within the bound -- `out` then names its slot, and
/// [`lowlat_vdd_get_monitor`] says where it stands later.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`]; `plug` points at a filled
/// `lowlat_vdd_plug_info`; `out` is null or points at a `lowlat_vdd_monitor` whose
/// `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_plug(
    vdd: *mut lowlat_vdd,
    plug: *const lowlat_vdd_plug_info,
    out: *mut lowlat_vdd_monitor,
) -> lowlat_status {
    unsafe {
        entered(vdd, |controller| {
            let Some(plug) = plug.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (plug.size as usize) < size_of::<lowlat_vdd_plug_info>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let lifetime = match plug.lifetime {
                LOWLAT_VDD_BOUND => Lifetime::Bound,
                LOWLAT_VDD_KEPT => Lifetime::Kept,
                _ => return LOWLAT_ERR_INVALID_ARGUMENT,
            };
            let slot = match plug.slot {
                LOWLAT_VDD_SLOT_ANY => None,
                slot => match slot_of(slot) {
                    Some(slot) => Some(slot),
                    None => return LOWLAT_ERR_INVALID_ARGUMENT,
                },
            };
            let mode = Mode::new(plug.width, plug.height, plug.refresh_hz);
            match controller.plug(mode, lifetime, plug.restore, slot) {
                Ok(monitor) => fill(out, &monitor),
                Err(error) => refused_into(controller, error, out),
            }
        })
    }
}

/// Unplug a monitor this handle plugged, or a kept one, and wait until it has
/// gone.
///
/// @returns [`LOWLAT_ERR_VDD_NOT_OWNER`] for a monitor bound to another handle.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_unplug(vdd: *mut lowlat_vdd, slot: u32) -> lowlat_status {
    unsafe {
        entered(vdd, |controller| {
            let Some(slot) = slot_of(slot) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            match controller.unplug(slot) {
                Ok(()) => LOWLAT_OK,
                Err(error) => refused(error),
            }
        })
    }
}

/// Put a monitor this handle plugged, or a kept one, at a mode and wait until
/// it is current: through the system's display configuration when the monitor
/// lists the mode, by plugging it again with the mode added when it does not.
///
/// @param[out] out The monitor at its new mode, or from minor 30 as it stands
/// on [`LOWLAT_ERR_VDD_NOT_READY`]; may be null.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`]; `out` is null or points at a
/// `lowlat_vdd_monitor` whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_set_mode(
    vdd: *mut lowlat_vdd,
    slot: u32,
    width: u32,
    height: u32,
    refresh_hz: u32,
    out: *mut lowlat_vdd_monitor,
) -> lowlat_status {
    unsafe {
        entered(vdd, |controller| {
            let Some(slot) = slot_of(slot) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            match controller.set_mode(slot, Mode::new(width, height, refresh_hz)) {
                Ok(monitor) => fill(out, &monitor),
                Err(error) => refused_into(controller, error, out),
            }
        })
    }
}

/// The slots in use, by any handle. Pass a null `slots` to learn the count.
///
/// @param[out] slots Room for `*count` slot numbers.
/// @param[in,out] count The room going in, the slots in use coming out.
/// @returns [`LOWLAT_ERR_TOO_SMALL`] with the count written when the room is
/// short.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`]; `count` points at a `uint32_t`;
/// `slots` is null or has room for `*count` of them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_list(
    vdd: *mut lowlat_vdd,
    slots: *mut u32,
    count: *mut u32,
) -> lowlat_status {
    unsafe {
        entered(vdd, |controller| {
            if count.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let monitors = match controller.list() {
                Ok(monitors) => monitors,
                Err(error) => return refused(error),
            };
            let found = u32::try_from(monitors.len()).unwrap_or(u32::MAX);
            if slots.is_null() {
                count.write(found);
                return LOWLAT_OK;
            }
            let room = count.read() as usize;
            for (at, monitor) in monitors.iter().take(room).enumerate() {
                slots.add(at).write(u32::from(monitor.slot));
            }
            if room < monitors.len() {
                count.write(found);
                return LOWLAT_ERR_TOO_SMALL;
            }
            count.write(found);
            LOWLAT_OK
        })
    }
}

/// The monitor at a slot.
///
/// @returns [`LOWLAT_ERR_VDD_NO_MONITOR`] for an empty slot.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`]; `out` points at a `lowlat_vdd_monitor`
/// whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_get_monitor(
    vdd: *mut lowlat_vdd,
    slot: u32,
    out: *mut lowlat_vdd_monitor,
) -> lowlat_status {
    unsafe {
        entered(vdd, |controller| {
            let Some(slot) = slot_of(slot) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if out.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            match controller.monitor(slot) {
                Ok(monitor) => fill(out, &monitor),
                Err(error) => refused(error),
            }
        })
    }
}

/// The settings every monitor shares, from the driver's file.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`]; `out` points at a
/// `lowlat_vdd_settings` whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_get_settings(
    vdd: *mut lowlat_vdd,
    out: *mut lowlat_vdd_settings,
) -> lowlat_status {
    unsafe {
        entered(vdd, |_| {
            let Some(out) = out.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (out.size as usize) < size_of::<lowlat_vdd_settings>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let settings = match lowlat_vdd_ctl::settings() {
                Ok(settings) => settings,
                Err(error) => return refused(error),
            };
            let size = out.size;
            *out = lowlat_vdd_settings {
                size,
                gpu_vendor: told(settings.gpu).0,
                gpu_device: told(settings.gpu).1,
                pointer_drawn: settings.pointer == Pointer::Drawn,
                mode_count: u32::try_from(settings.modes().len()).unwrap_or(0),
                modes: [lowlat_vdd_mode::default(); LOWLAT_VDD_MODES_MAX],
                edid_len: 0,
                edid: [0; LOWLAT_VDD_EDID_MAX],
            };
            for (to, from) in out.modes.iter_mut().zip(settings.modes()) {
                *to = lowlat_vdd_mode {
                    width: from.width,
                    height: from.height,
                    refresh_hz: from.refresh_hz,
                };
            }
            if let Some(edid) = settings.edid {
                let bytes = edid.as_slice();
                for (to, from) in out.edid.iter_mut().zip(bytes) {
                    *to = *from;
                }
                out.edid_len = u32::try_from(bytes.len()).unwrap_or(0);
            }
            LOWLAT_OK
        })
    }
}

/// Change the settings every monitor shares. The GPU applies at once, the
/// pointer at each monitor's next swapchain, the custom modes and the EDID to
/// the monitors plugged after.
///
/// @returns [`LOWLAT_ERR_INVALID_ARGUMENT`] for a mode no monitor may be, more
/// modes than fit, or an EDID whose blocks do not check.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`]; `settings` points at a filled
/// `lowlat_vdd_settings`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_set_settings(
    vdd: *mut lowlat_vdd,
    settings: *const lowlat_vdd_settings,
) -> lowlat_status {
    unsafe {
        entered(vdd, |controller| {
            let Some(asked) = settings.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (asked.size as usize) < size_of::<lowlat_vdd_settings>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let gpu = match (
                u16::try_from(asked.gpu_vendor),
                u16::try_from(asked.gpu_device),
            ) {
                (Ok(0), Ok(0)) => None,
                (Ok(vendor), Ok(device)) => Some(GpuChoice::Exact(Gpu { vendor, device })),
                _ => return LOWLAT_ERR_INVALID_ARGUMENT,
            };
            let pair = (asked.gpu_vendor, asked.gpu_device);
            let count = asked.mode_count as usize;
            let Some(modes) = asked.modes.get(..count) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let mut wanted = Settings::default();
            for mode in modes {
                if !wanted.add_mode(Mode::new(mode.width, mode.height, mode.refresh_hz)) {
                    return LOWLAT_ERR_INVALID_ARGUMENT;
                }
            }
            let edid = match asked.edid_len as usize {
                0 => None,
                len => match asked.edid.get(..len).and_then(lowlat_vdd_ctl::Edid::parse) {
                    Some(edid) => Some(edid),
                    None => return LOWLAT_ERR_INVALID_ARGUMENT,
                },
            };
            let pointer = if asked.pointer_drawn {
                Pointer::Drawn
            } else {
                Pointer::Hardware
            };
            let changed = controller.change_settings(|settings| {
                settings.gpu = written_back(settings.gpu, pair, gpu);
                settings.pointer = pointer;
                while let Some(mode) = settings.modes().first().copied() {
                    settings.remove_mode(mode);
                }
                for mode in wanted.modes() {
                    settings.add_mode(*mode);
                }
                settings.edid = edid;
            });
            match changed {
                Ok(_) => LOWLAT_OK,
                Err(error) => refused(error),
            }
        })
    }
}

/// Panic on purpose and prove the boundary contains it: the third half's twin
/// of `lowlat_debug_panic`. Not for applications.
///
/// @returns [`LOWLAT_ERR_INTERNAL`]; every later call on `vdd` answers
/// [`LOWLAT_ERR_POISONED`], and closing it still works.
///
/// # Safety
///
/// `vdd` came from [`lowlat_vdd_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_vdd_debug_panic(vdd: *mut lowlat_vdd) -> lowlat_status {
    unsafe {
        entered(vdd, |_| {
            panic!("deliberate panic, to prove the boundary contains one")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor_with_slot(slot: u32) -> lowlat_vdd_monitor {
        // SAFETY: plain data, for which zero is a value.
        let mut monitor: lowlat_vdd_monitor = unsafe { core::mem::zeroed() };
        monitor.size = u32::try_from(size_of::<lowlat_vdd_monitor>()).expect("fits");
        monitor.slot = slot;
        monitor
    }

    /// **`NOT_READY` names the monitor in `out`** (minor 30), and no other
    /// refusal touches it. The answer is made up here -- this machine's system
    /// shows a plugged monitor at once, so a real one cannot be had on demand
    /// -- and the monitor it names is real. Bound, so the handle's end takes
    /// it away.
    #[test]
    #[ignore = "plugs a display on this machine's driver"]
    fn not_ready_names_the_monitor_in_out() {
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1280, 720, 60), Lifetime::Bound, false, None)
            .expect("a monitor");
        let mut out = monitor_with_slot(99);
        // SAFETY: a live monitor of this minor's size.
        let status =
            unsafe { refused_into(&controller, Error::NotReady(monitor.slot), &raw mut out) };
        assert_eq!(status, LOWLAT_ERR_VDD_NOT_READY);
        assert_eq!(out.slot, u32::from(monitor.slot), "out not filled");
        assert!(out.owned, "not this handle's");
        let mut untouched = monitor_with_slot(99);
        // SAFETY: as above.
        let status = unsafe { refused_into(&controller, Error::Busy, &raw mut untouched) };
        assert_eq!(status, LOWLAT_ERR_VDD_BUSY);
        assert_eq!(untouched.slot, 99, "out filled for another refusal");
    }

    /// **The library says exact GPUs alone, and loses no other choice.** A
    /// maker's choice and the power-saving one read as zero; written back as
    /// read, each stays, and any other pair replaces it.
    #[test]
    fn a_gpu_choice_the_pair_cannot_say_survives_being_written_back() {
        let rtx = Some(GpuChoice::Exact(Gpu {
            vendor: 0x10de,
            device: 0x2d05,
        }));
        let as_asked = |vendor, device| {
            (vendor != 0 || device != 0).then_some(GpuChoice::Exact(Gpu { vendor, device }))
        };
        assert_eq!(told(rtx), (0x10de, 0x2d05));
        for other in [
            Some(GpuChoice::Maker(0x8086)),
            Some(GpuChoice::PowerSaving),
            None,
        ] {
            assert_eq!(told(other), (0, 0), "{other:?}");
            assert_eq!(written_back(other, (0, 0), None), other, "{other:?} lost");
            assert_eq!(
                written_back(other, (0x10de, 0x2d05), as_asked(0x10de, 0x2d05)),
                rtx,
                "{other:?} kept over an exact GPU asked"
            );
        }
        assert_eq!(written_back(rtx, (0x10de, 0x2d05), rtx), rtx);
        assert_eq!(
            written_back(rtx, (0, 0), None),
            None,
            "auto asked, the exact kept"
        );
    }
}
