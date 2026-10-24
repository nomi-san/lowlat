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
//!
//! A person drives it from a shell through [`cli`]: the program `lowlat-vdd`
//! and `lowlatd display` take the same commands.

#![cfg(windows)]

pub mod cli;
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
    /// The monitor at this slot is plugged but the system did not finish
    /// within the bound.
    NotReady(u8),
    /// The settings file could not be read or written.
    File,
    /// Any other answer from the system, its error code.
    Io(u32),
}

/// How a monitor is turned on the desktop, numbered as the system's display
/// configuration numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    Upright = 1,
    Quarter = 2,
    Half = 3,
    ThreeQuarters = 4,
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
    /// The mode now when shown, else the one the driver holds for it: the
    /// monitor's own, turned or not.
    pub mode: Mode,
    /// Turned on the desktop, by its display settings or a program.
    pub turned: bool,
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

/// Put any display of the desktop -- a panel, another vendor's virtual display,
/// one this controller did not plug -- at `width` x `height`, the monitor's own
/// sides, by the system's name for it, `\\.\DISPLAYn` or the bare `DISPLAYn`:
/// at the refresh it runs where that size has it, else the highest that size
/// has, keeping its turn. **A size it does not list is refused**, never
/// scaled. Never saved to the system's store.
pub fn put_size(name: &str, width: u32, height: u32) -> Result<Mode, Error> {
    let name = display::gdi_name(name);
    let (listed, now) = display::refreshes(&name, width, height);
    let mode = Mode::new(width, height, refresh_for(now, &listed).ok_or(Error::Mode)?);
    display::set_mode(&name, mode, false).map_err(Error::Display)?;
    Ok(mode)
}

/// Turn any display of the desktop by the system's name for it, with its
/// prefix or without, through the system's display configuration: its path
/// turned, its mode as it is. Never saved to the system's store.
pub fn put_turn(name: &str, turn: Turn) -> Result<(), Error> {
    let (adapter, target) = display::named(name).ok_or(Error::NoMonitor)?;
    display::set_turn(adapter, target, turn as i32, false)
        .map_err(|code| Error::Display(i32::try_from(code).unwrap_or(i32::MAX)))
}

/// **The mode the system's store holds for `monitor`** among the monitors
/// connected now: what the system puts back on it as another monitor arrives
/// or a program leaves exclusive fullscreen, where a mode a program put for
/// itself is another. None where the store shows it in no arrangement of
/// these monitors.
pub fn saved_mode(monitor: &Monitor) -> Option<Mode> {
    display::saved(monitor.os_adapter_luid, monitor.os_target_id)
}

/// The refresh a size is put at: the one the display runs at `now` where the
/// size lists it, else the highest it lists; none where it lists none.
fn refresh_for(now: u32, listed: &[u32]) -> Option<u32> {
    if listed.contains(&now) {
        Some(now)
    } else {
        listed.iter().copied().max()
    }
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
        self.plug_with(mode, lifetime, restore, slot, false)
    }

    /// Plug a monitor bound to this handle at `mode`'s size, **at the refresh
    /// the system restores for it** -- the one its display settings last saved
    /// for the set of monitors it joins -- and at `mode` whole where the
    /// system restores none, or another size.
    pub fn plug_at_size(&mut self, mode: Mode, slot: Option<u8>) -> Result<Monitor, Error> {
        self.plug_with(mode, Lifetime::Bound, false, slot, true)
    }

    fn plug_with(
        &mut self,
        mode: Mode,
        lifetime: Lifetime,
        restore: bool,
        slot: Option<u8>,
        any_refresh: bool,
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
        let shown = self.show(
            slot,
            plugged.os_adapter_luid,
            plugged.os_target_id,
            Some(mode),
            any_refresh,
            kept,
            deadline,
        );
        if let Err(refused @ Error::Display(_)) = shown {
            // **Not plugged at all rather than plugged and not shown**, as for
            // the file above: the system refused the monitor, so nothing of
            // the request stays behind -- neither the monitor nor its line.
            if restore {
                let _ = file::change(Instant::now() + BOUND, |settings| {
                    if let Some(entry) = settings.restore.get_mut(usize::from(slot)) {
                        *entry = None;
                    }
                });
            }
            let _ = self.device.unplug(slot);
            return Err(refused);
        }
        shown?;
        self.monitor(slot)
    }

    /// Wait for the system to show the monitor at `slot`, making it active
    /// when the system does not, then put it at `mode` -- which a saved
    /// arrangement may have overruled. With `any_refresh`, `mode`'s size at
    /// whatever refresh the system has it at stands.
    #[allow(
        clippy::too_many_arguments,
        reason = "one monitor's place, what it is to be put at, and the bound"
    )]
    fn show(
        &self,
        slot: u8,
        adapter: u64,
        target: u32,
        mode: Option<Mode>,
        any_refresh: bool,
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
        let shown = shown.ok_or(Error::NotReady(slot))?;
        let Some(mode) = mode else {
            return Ok(());
        };
        let at = |shown: &display::Shown| {
            if any_refresh {
                (shown.mode.width, shown.mode.height) == (mode.width, mode.height)
            } else {
                shown.mode == mode
            }
        };
        if at(&shown) {
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
            if wait(settle, || display::shown(adapter, target).filter(at)).is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::NotReady(slot));
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
        // **The file first**: a monitor the driver then refuses to unplug
        // stays plugged and no longer comes back, where the other order
        // would bring a removed one back at the driver's next start.
        if entry.flags & Entry::RESTORE != 0 {
            file::change(deadline, |settings| {
                if let Some(entry) = settings.restore.get_mut(usize::from(slot)) {
                    *entry = None;
                }
            })?;
        }
        self.device.unplug(slot)?;
        wait(deadline, || {
            display::shown(entry.os_adapter_luid, entry.os_target_id)
                .is_none()
                .then_some(())
        })
        .ok_or(Error::NotReady(slot))
    }

    /// Put a monitor this handle plugged, or a kept one, at `mode`: through
    /// the system's display configuration when the monitor lists it, by a
    /// replug with the mode added first when it does not.
    pub fn set_mode(&mut self, slot: u8, mode: Mode) -> Result<Monitor, Error> {
        self.set_mode_within(slot, mode, BOUND)
    }

    /// [`Controller::set_mode`] waiting for the system no longer than `bound`.
    /// **For a caller that must not wait**: a stream asking again between two
    /// pictures for a mode the system refused a moment ago makes one attempt
    /// and hears the answer.
    pub fn set_mode_within(
        &mut self,
        slot: u8,
        mode: Mode,
        bound: Duration,
    ) -> Result<Monitor, Error> {
        if !mode.is_valid() {
            return Err(Error::Mode);
        }
        let deadline = Instant::now() + bound;
        let entry = self.entry(slot)?;
        let kept = entry.flags & Entry::KEPT != 0;
        if !kept && entry.flags & Entry::OWNED == 0 {
            return Err(Error::NotOwner);
        }
        let (adapter, target) = (entry.os_adapter_luid, entry.os_target_id);
        // Shown first, so the system can be asked what it offers.
        self.show(slot, adapter, target, None, false, kept, deadline)?;
        let name = display::shown(adapter, target)
            .ok_or(Error::NotReady(slot))?
            .name;
        if display::offers(&name, mode) {
            self.show(slot, adapter, target, Some(mode), false, kept, deadline)?;
        } else {
            let plugged = self.device.replug(slot, mode)?;
            self.show(
                slot,
                plugged.os_adapter_luid,
                plugged.os_target_id,
                Some(mode),
                false,
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

    /// Turn a monitor this handle plugged, or a kept one, through the
    /// system's display configuration: its path turned, its mode as it is,
    /// saved as a mode is. **Put once, never held**: whatever turns it after
    /// -- its display settings, a program -- stands.
    pub fn set_turn(&mut self, slot: u8, turn: Turn) -> Result<Monitor, Error> {
        let deadline = Instant::now() + BOUND;
        let entry = self.entry(slot)?;
        let kept = entry.flags & Entry::KEPT != 0;
        if !kept && entry.flags & Entry::OWNED == 0 {
            return Err(Error::NotOwner);
        }
        let (adapter, target) = (entry.os_adapter_luid, entry.os_target_id);
        self.show(slot, adapter, target, None, false, kept, deadline)?;
        let save = kept && display::others_active(adapter);
        display::set_turn(adapter, target, turn as i32, save)
            .map_err(|code| Error::Display(i32::try_from(code).unwrap_or(i32::MAX)))?;
        let turned = turn != Turn::Upright;
        wait(deadline, || {
            display::shown(adapter, target).filter(|shown| shown.turned == turned)
        })
        .ok_or(Error::NotReady(slot))?;
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
            turned: shown.as_ref().is_some_and(|shown| shown.turned),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;

    /// **A size keeps the refresh the display runs where it can**, else takes
    /// the highest that size lists -- a person's 144 is not dropped to 60 for a
    /// guest's size -- and a size listed at none is refused.
    #[test]
    fn a_size_keeps_the_refresh_it_can_else_the_highest_it_has() {
        assert_eq!(refresh_for(144, &[60, 120, 144]), Some(144));
        assert_eq!(
            refresh_for(60, &[60, 120, 144]),
            Some(60),
            "raised past the desk's"
        );
        assert_eq!(refresh_for(144, &[60, 120]), Some(120));
        assert_eq!(refresh_for(0, &[60, 59]), Some(60), "the refresh unread");
        assert_eq!(refresh_for(60, &[]), None, "a size it does not list");
    }

    /// **A display is named with its prefix or without it**: a host lists its
    /// outputs as `DISPLAYn`, and the system's interfaces take `\\.\DISPLAYn`.
    #[test]
    fn a_display_is_named_with_its_prefix_or_without() {
        assert_eq!(display::gdi_name("DISPLAY79"), r"\\.\DISPLAY79");
        assert_eq!(display::gdi_name(r"\\.\DISPLAY79"), r"\\.\DISPLAY79");
    }

    /// Any display of the desktop put at a size and turned by the system's name
    /// for it, then put back as it was: `LOWLAT_DISPLAY` names it
    /// (`\\.\DISPLAYn`), `LOWLAT_SIZE` the size (`1280x720`), which it must list.
    /// Nothing is saved.
    #[test]
    #[ignore = "changes the mode and turn of the display LOWLAT_DISPLAY names"]
    fn any_display_is_put_at_a_size_and_turned_by_its_name() {
        let name = std::env::var("LOWLAT_DISPLAY").expect("LOWLAT_DISPLAY");
        let (width, height) = std::env::var("LOWLAT_SIZE")
            .expect("LOWLAT_SIZE")
            .split_once('x')
            .map(|(w, h)| (w.parse().expect("width"), h.parse().expect("height")))
            .expect("WxH");
        let before = display::named(&name).and_then(|(a, t)| display::shown(a, t));
        let before = before.expect("the display shown");
        let put = put_size(&name, width, height).expect("the size refused");
        put_turn(&name, Turn::Quarter).expect("the turn refused");
        std::thread::sleep(Duration::from_millis(500));
        let now = display::named(&name).and_then(|(a, t)| display::shown(a, t));
        let now = now.expect("the display shown after");
        put_turn(&name, Turn::Upright).expect("the turn back refused");
        display::set_mode(&display::gdi_name(&name), before.mode, false)
            .expect("the mode back refused");
        println!(
            "{name} was {}, put at {put}, then {} turned {}",
            before.mode, now.mode, now.turned
        );
        assert!(now.turned, "the turn did not take");
        assert_eq!(
            (now.mode.width, now.mode.height),
            (width, height),
            "the size did not take"
        );
        assert!(
            put_size(&name, 1234, 567).is_err(),
            "a size the display does not list was put"
        );
    }

    /// **An unplug writes the file first**: with the file's lock held by
    /// another, an unplug of a monitor marked to come back answers that the
    /// file could not be written and leaves the monitor plugged and still
    /// marked -- never a removed monitor that the driver's next start brings
    /// back.
    #[test]
    #[ignore = "plugs a display on this machine's driver and holds its file's lock"]
    fn an_unplug_writes_the_file_first() {
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(Mode::new(1280, 720, 60), Lifetime::Kept, true, None)
            .expect("a kept monitor, marked to come back");
        let lock = PathBuf::from(std::env::var_os("ProgramData").expect("ProgramData"))
            .join(lowlat_vdd_proto::settings::FOLDER)
            .join("displays.lock");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(&lock)
            .expect("the file's lock");
        let answered = controller.unplug(monitor.slot);
        let still = controller.monitor(monitor.slot);
        drop(held);
        let cleaned = controller.unplug(monitor.slot);
        assert_eq!(
            answered,
            Err(Error::File),
            "the unplug went on without the file"
        );
        let still = still.expect("unplugged though its line stayed in the file");
        assert!(still.kept && still.restore, "{still:?}");
        assert_eq!(cleaned, Ok(()), "the unplug once the lock was let go");
    }
}
