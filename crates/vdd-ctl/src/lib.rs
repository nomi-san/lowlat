//! The display driver's controller: the user-mode side of its control
//! interface ([docs/11-drivers.md](../../../docs/11-drivers.md) section 1).
//!
//! A [`Controller`] is one handle on the driver. A monitor it plugs bound is
//! its own: only it unplugs or re-modes that one, and its close, or its
//! process's end, unplugs it. A kept monitor belongs to no handle and lives
//! until removed, plugged again when the driver starts if it is restored.
//!
//! **Every call waits for the system to finish what it asked, within
//! [`BOUND`]**: a plug until the monitor is active at its mode, a mode until
//! it is current, an unplug until the monitor has gone. Past the bound a
//! call answers [`Error::NotReady`] and [`Controller::list`] says where the
//! monitor stands.
//!
//! **A kept monitor's arrangement is saved in the system's own store**, as any
//! monitor's is, so it comes back after a reboot at its mode and place --
//! unless only virtual displays would be active, an arrangement that holds a
//! later boot waiting for their driver. **A bound monitor's never is.**

#![cfg(windows)]

mod device;
mod display;
mod file;
mod frames;

use std::time::{Duration, Instant};

pub use lowlat_vdd_proto::MAX_MONITORS;
pub use lowlat_vdd_proto::control::Lifetime;
use lowlat_vdd_proto::control::{Entry, Plug};
pub use lowlat_vdd_proto::edid::Edid;
pub use lowlat_vdd_proto::frames::{Picture, Pointer as HardwareCursor, ShapeKind, Snapshot};
pub use lowlat_vdd_proto::modes::Mode;
pub use lowlat_vdd_proto::settings::{Gpu, Pointer, Settings};

use device::Device;
pub use frames::{FramePage, Waited};

/// How long a call waits for the system.
pub const BOUND: Duration = Duration::from_secs(3);

/// How long a plug leaves the system to make the monitor active by itself --
/// at once for a set of monitors it has not seen or one it saved -- before
/// the controller does.
const THE_SYSTEMS_OWN: Duration = Duration::from_millis(400);

/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(20);

/// How long a mode just put is given to show before the system is taken to
/// have put another back, and it is put again.
const RESETTLE: Duration = Duration::from_millis(300);

/// Why a call did not do what it asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The driver is not installed, or its device is not started.
    Absent,
    /// The driver speaks another protocol: it and this controller ship
    /// together.
    Version(u32),
    /// The device refused this account.
    Denied,
    /// The driver's adapter is still starting.
    Starting,
    /// Every slot is taken.
    Full,
    /// The slot asked for is taken.
    SlotTaken,
    /// The monitor is bound to another handle.
    NotOwner,
    /// No monitor at that slot.
    NoMonitor,
    /// A mode no monitor of this display may be.
    Mode,
    /// Arguments that contradict each other.
    Argument,
    /// The monitor is being plugged again; ask again shortly.
    Busy,
    /// The system's display configuration refused, with its code.
    Display(i32),
    /// The monitor is plugged but the system did not finish within the bound.
    NotReady,
    /// The settings file could not be read or written.
    File,
    /// Any other answer from the system, its error code.
    Io(u32),
}

/// A monitor of the driver, as the driver and the system see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub slot: u8,
    /// Plugged through this handle, bound to it.
    pub owned: bool,
    pub kept: bool,
    /// Kept, and plugged again when the driver starts.
    pub restore: bool,
    /// Shown on the desktop.
    pub active: bool,
    /// The mode now when shown, else the one the driver holds for it.
    pub mode: Mode,
    /// Its top left on the desktop, when shown.
    pub x: i32,
    pub y: i32,
    /// The system's name for the display when shown, `\\.\DISPLAYn`.
    pub name: String,
    pub os_target_id: u32,
    pub os_adapter_luid: u64,
    /// The GPU the system renders it on, from its last swapchain.
    pub render_luid: u64,
    /// Pictures the system handed over.
    pub frames: u64,
}

/// A GPU a person may name in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    pub gpu: Gpu,
    pub name: String,
}

/// Wait until `ready` answers, or the deadline.
fn wait<T>(deadline: Instant, mut ready: impl FnMut() -> Option<T>) -> Option<T> {
    loop {
        if let Some(value) = ready() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// One handle on the driver.
#[derive(Debug)]
pub struct Controller {
    device: Device,
}

impl Controller {
    /// Open the driver.
    pub fn open() -> Result<Self, Error> {
        Ok(Self {
            device: Device::open()?,
        })
    }

    /// Plug a monitor at `mode`: bound to this handle, or kept and, with
    /// `restore`, plugged again whenever the driver starts. `slot` asks for
    /// a slot, whose identity the system remembers the monitor's place by.
    pub fn plug(
        &mut self,
        mode: Mode,
        lifetime: Lifetime,
        restore: bool,
        slot: Option<u8>,
    ) -> Result<Monitor, Error> {
        if !mode.is_valid() {
            return Err(Error::Mode);
        }
        if restore && lifetime == Lifetime::Bound {
            return Err(Error::Argument);
        }
        if slot.is_some_and(|slot| usize::from(slot) >= MAX_MONITORS) {
            return Err(Error::Argument);
        }
        let deadline = Instant::now() + BOUND;
        let plugged = self.device.plug(Plug {
            mode,
            lifetime,
            restore,
            slot,
        })?;
        let slot = u8::try_from(plugged.slot).map_err(|_| Error::Version(0))?;
        if restore {
            let kept = file::change(deadline, |settings| {
                if let Some(entry) = settings.restore.get_mut(usize::from(slot)) {
                    *entry = Some(mode);
                }
            });
            if let Err(error) = kept {
                // Not plugged at all rather than plugged without coming back.
                let _ = self.device.unplug(slot);
                return Err(error);
            }
        }
        let kept = lifetime == Lifetime::Kept;
        self.show(
            plugged.os_adapter_luid,
            plugged.os_target_id,
            Some(mode),
            kept,
            deadline,
        )?;
        self.monitor(slot)
    }

    /// Wait for the system to show the monitor, making it active when the
    /// system does not, then put it at `mode` -- which a saved arrangement
    /// may have overruled.
    fn show(
        &self,
        adapter: u64,
        target: u32,
        mode: Option<Mode>,
        kept: bool,
        deadline: Instant,
    ) -> Result<(), Error> {
        let own = (Instant::now() + THE_SYSTEMS_OWN).min(deadline);
        let mut shown = wait(own, || display::shown(adapter, target));
        if shown.is_none() {
            let save = kept && display::others_active(adapter);
            display::activate(adapter, target, save)
                .map_err(|code| Error::Display(i32::try_from(code).unwrap_or(i32::MAX)))?;
            shown = wait(deadline, || display::shown(adapter, target));
        }
        let shown = shown.ok_or(Error::NotReady)?;
        let Some(mode) = mode else {
            return Ok(());
        };
        if shown.mode == mode {
            return Ok(());
        }
        let save = kept && display::others_active(adapter);
        self.put_mode(&shown.name, mode, save, deadline)?;
        // **Put again while the system takes it back.** A monitor plugged
        // again is restored to its set's saved arrangement as its arrival
        // completes, which can come after the mode was put -- a second and
        // more later while a program presents on it, measured -- and an
        // unsaved mode does not survive that. So the mode is put again
        // whenever the system has the monitor at another, until the bound.
        loop {
            let settle = (Instant::now() + RESETTLE).min(deadline);
            if wait(settle, || {
                display::shown(adapter, target).filter(|shown| shown.mode == mode)
            })
            .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::NotReady);
            }
            if let Some(now) = display::shown(adapter, target) {
                self.put_mode(&now.name, mode, save, deadline)?;
            }
        }
    }

    /// Set a mode, again within the bound while the system refuses it: after
    /// a replug the system can take a while to list the new mode.
    fn put_mode(&self, name: &str, mode: Mode, save: bool, deadline: Instant) -> Result<(), Error> {
        loop {
            match display::set_mode(name, mode, save) {
                Ok(()) => return Ok(()),
                Err(_) if Instant::now() + Duration::from_millis(100) < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(code) => return Err(Error::Display(code)),
            }
        }
    }

    /// Unplug a monitor this handle plugged, or a kept one.
    pub fn unplug(&mut self, slot: u8) -> Result<(), Error> {
        let deadline = Instant::now() + BOUND;
        let entry = self.entry(slot)?;
        self.device.unplug(slot)?;
        if entry.flags & Entry::RESTORE != 0 {
            file::change(deadline, |settings| {
                if let Some(entry) = settings.restore.get_mut(usize::from(slot)) {
                    *entry = None;
                }
            })?;
        }
        wait(deadline, || {
            display::shown(entry.os_adapter_luid, entry.os_target_id)
                .is_none()
                .then_some(())
        })
        .ok_or(Error::NotReady)
    }

    /// Put a monitor this handle plugged, or a kept one, at `mode`: through
    /// the system's display configuration when the monitor lists it, by a
    /// replug with the mode added first when it does not.
    pub fn set_mode(&mut self, slot: u8, mode: Mode) -> Result<Monitor, Error> {
        if !mode.is_valid() {
            return Err(Error::Mode);
        }
        let deadline = Instant::now() + BOUND;
        let entry = self.entry(slot)?;
        let kept = entry.flags & Entry::KEPT != 0;
        if !kept && entry.flags & Entry::OWNED == 0 {
            return Err(Error::NotOwner);
        }
        let (adapter, target) = (entry.os_adapter_luid, entry.os_target_id);
        // Shown first, so the system can be asked what it offers.
        self.show(adapter, target, None, kept, deadline)?;
        let name = display::shown(adapter, target).ok_or(Error::NotReady)?.name;
        if display::offers(&name, mode) {
            self.show(adapter, target, Some(mode), kept, deadline)?;
        } else {
            let plugged = self.device.replug(slot, mode)?;
            self.show(
                plugged.os_adapter_luid,
                plugged.os_target_id,
                Some(mode),
                kept,
                deadline,
            )?;
        }
        if entry.flags & Entry::RESTORE != 0 {
            file::change(deadline, |settings| {
                if let Some(entry) = settings.restore.get_mut(usize::from(slot)) {
                    *entry = Some(mode);
                }
            })?;
        }
        self.monitor(slot)
    }

    fn entry(&self, slot: u8) -> Result<Entry, Error> {
        let list = self.device.list()?;
        list.entries
            .get(usize::from(slot))
            .copied()
            .filter(|entry| entry.flags & Entry::IN_USE != 0)
            .ok_or(Error::NoMonitor)
    }

    fn describe(entry: &Entry) -> Option<Monitor> {
        if entry.flags & Entry::IN_USE == 0 {
            return None;
        }
        let shown = display::shown(entry.os_adapter_luid, entry.os_target_id);
        let held = Mode::new(entry.width, entry.height, entry.refresh_hz);
        Some(Monitor {
            slot: u8::try_from(entry.slot).ok()?,
            owned: entry.flags & Entry::OWNED != 0,
            kept: entry.flags & Entry::KEPT != 0,
            restore: entry.flags & Entry::RESTORE != 0,
            active: shown.is_some(),
            mode: shown.as_ref().map_or(held, |shown| shown.mode),
            x: shown.as_ref().map_or(0, |shown| shown.x),
            y: shown.as_ref().map_or(0, |shown| shown.y),
            name: shown.map(|shown| shown.name).unwrap_or_default(),
            os_target_id: entry.os_target_id,
            os_adapter_luid: entry.os_adapter_luid,
            render_luid: entry.render_luid,
            frames: entry.frames,
        })
    }

    /// The monitor at `slot`.
    pub fn monitor(&self, slot: u8) -> Result<Monitor, Error> {
        Self::describe(&self.entry(slot)?).ok_or(Error::NoMonitor)
    }

    /// Every monitor the driver has, whoever plugged it.
    pub fn list(&self) -> Result<Vec<Monitor>, Error> {
        Ok(self
            .device
            .list()?
            .entries
            .iter()
            .filter_map(Self::describe)
            .collect())
    }

    /// Change the file's settings -- the GPU, the pointer, the custom modes,
    /// the EDID -- and have the driver read them: the GPU at once, the rest
    /// for the monitors plugged after.
    pub fn change_settings(
        &mut self,
        change: impl FnOnce(&mut Settings),
    ) -> Result<Settings, Error> {
        let settings = file::change(Instant::now() + BOUND, change)?;
        self.device.reload()?;
        Ok(settings)
    }
}

/// The file's settings, read without the driver.
pub fn settings() -> Result<Settings, Error> {
    file::read()
}

/// The GPUs a person may name, in the system's high-performance order.
pub fn gpus() -> Vec<GpuInfo> {
    let Ok(adapters) = lowlat_drivers::d3d11::D3d11::load().and_then(|d3d11| d3d11.adapters())
    else {
        return Vec::new();
    };
    adapters
        .iter()
        .filter(|adapter| adapter.decodes_here())
        .filter_map(|adapter| {
            Some(GpuInfo {
                gpu: Gpu {
                    vendor: u16::try_from(adapter.vendor).ok()?,
                    device: u16::try_from(adapter.device).ok()?,
                },
                name: adapter.description.clone(),
            })
        })
        .collect()
}
