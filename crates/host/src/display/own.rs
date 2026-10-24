//! The host's own display on Windows: one monitor of the host's display
//! driver, plugged by the host and bound to it, so it goes when the host does
//! however the host ends, and held at the mode the host wants
//! (docs/11-drivers.md section 1, docs/06-api.md section 14).
//!
//! **Its calls wait for the system**, up to the controller's bound, so they
//! run on the stream's own thread and never on a caller's.

use std::time::Duration;

use lowlat_vdd_ctl::{BOUND, Controller, Error, Lifetime, MAX_MONITORS, Mode};

use super::sys::publish_own;

/// The size the display is first plugged at when nothing asked for one.
pub(crate) const FIRST_SIZE: (u32, u32) = (1920, 1080);

/// The refresh it is first plugged at when none is asked and the system
/// restores none.
const FIRST_HZ: u32 = 60;

/// The host's own display.
#[derive(Debug, Default)]
pub(crate) struct Own {
    controller: Option<Controller>,
    /// The slot the monitor is in, while one is plugged.
    slot: Option<u8>,
    /// The size the host wants it at, once it wants one.
    wanted: Option<(u32, u32)>,
    /// The size and refresh last asked and put, or found in place.
    asked: Option<(u32, u32, Option<u32>)>,
    /// A mode of another's the display is left at, said once.
    left: Option<Mode>,
    /// Why the last attempt failed, said once rather than every pass.
    refused: Option<Error>,
    /// A size and refresh the system refused to put, asked again by
    /// [`Own::retry`] until it takes or another is asked.
    pending: Option<(u32, u32, Option<u32>)>,
}

impl Own {
    /// Whether a monitor is plugged.
    pub(crate) fn plugged(&self) -> bool {
        self.slot.is_some()
    }

    /// The size the host wants, once it wants one.
    pub(crate) fn wanted(&self) -> Option<(u32, u32)> {
        self.wanted
    }

    /// Have the display at `width` x `height`, and at `refresh` where one is
    /// asked: plugged where there is none, else put there as it is asked, and
    /// again whenever the system puts back the mode its store holds for it --
    /// which another monitor's arrival and a program leaving exclusive
    /// fullscreen both do. Answers whether it is as wanted now.
    ///
    /// **What nobody asked for is the desk's.** With no refresh asked, the
    /// display keeps the refresh it has -- the one the system restores for it
    /// as it is plugged, and one set in its display settings after -- and a
    /// turn is never held: a mode is put keeping whatever turn it has. **Nor
    /// is a mode another put** ([`put_over`]): a program's in exclusive
    /// fullscreen stands, and the stream follows it.
    pub(crate) fn hold(&mut self, width: u32, height: u32, refresh: Option<u32>) -> bool {
        self.hold_within(width, height, refresh, BOUND)
    }

    /// Whether a mode the system refused waits to be asked again.
    pub(crate) fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// **Ask again for the mode the system refused**, waiting for it no
    /// longer than `bound`. The hold is otherwise asked as a stream is built,
    /// and a refusal that passes -- a new session's logon screen refused the
    /// owner's size for seconds after a logoff, measured -- left the display
    /// at another mode for the whole stream. Answers whether it is as wanted
    /// now; false with nothing pending.
    pub(crate) fn retry(&mut self, bound: Duration) -> bool {
        let Some((width, height, refresh)) = self.pending else {
            return false;
        };
        self.hold_within(width, height, refresh, bound)
    }

    fn hold_within(
        &mut self,
        width: u32,
        height: u32,
        refresh: Option<u32>,
        bound: Duration,
    ) -> bool {
        let first = Mode::new(width, height, refresh.unwrap_or(FIRST_HZ));
        if !first.is_valid() {
            self.refuse("a mode no monitor of this display may be", Error::Mode);
            return false;
        }
        self.wanted = Some((width, height));
        let ask = Some((width, height, refresh));
        let (Some(slot), Some(controller)) = (self.slot, self.controller.as_mut()) else {
            return self.plug_asked(ask, first, refresh.is_none());
        };
        let monitor = match controller.monitor(slot) {
            Ok(monitor) => monitor,
            // Gone underneath the host, as a restart of the driver takes it:
            // plugged again, which is what keeping it for the host's life is.
            Err(Error::NoMonitor) => {
                lowlat_common::log_warn!("display: the host's own display went, plugging it again");
                self.slot = None;
                publish_own(None);
                return self.plug_asked(ask, first, refresh.is_none());
            }
            // **The driver went, and the handle with it** -- updated,
            // restarted, its process ended (docs/11-drivers.md section 1.8):
            // a new handle, and the display plugged on it once the driver is
            // back.
            Err(Error::Absent) => {
                lowlat_common::log_warn!(
                    "display: the host's own display driver went, plugging the display again"
                );
                self.lost();
                return self.plug_asked(ask, first, refresh.is_none());
            }
            Err(error) => {
                self.refuse("the host's own display could not be read", error);
                return false;
            }
        };
        if monitor.active && !monitor.name.is_empty() {
            publish_own(Some((monitor.name.as_str(), slot)));
        }
        let mode = Mode::new(width, height, refresh.unwrap_or(monitor.mode.refresh_hz));
        if monitor.active && monitor.mode == mode {
            self.asked = ask;
            self.left = None;
            self.pending = None;
            return true;
        }
        let saved = lowlat_vdd_ctl::saved_mode(&monitor);
        if !put_over(self.asked != ask, monitor.active, monitor.mode, saved) {
            if self.left != Some(monitor.mode) {
                lowlat_common::log_info!(
                    "display: the host's own display left at {}, another's mode, asked={mode} \
                     stored={}",
                    monitor.mode,
                    saved.map_or_else(|| "none".to_string(), |saved| saved.to_string())
                );
                self.left = Some(monitor.mode);
            }
            return false;
        }
        match controller.set_mode_within(slot, mode, bound) {
            Ok(now) => {
                lowlat_common::log_info!(
                    "display: the host's own display put at {mode}, was {} {}",
                    monitor.mode,
                    if monitor.active { "shown" } else { "not shown" }
                );
                if !now.name.is_empty() {
                    publish_own(Some((now.name.as_str(), slot)));
                }
                self.asked = ask;
                self.left = None;
                self.refused = None;
                self.pending = None;
                true
            }
            Err(error) => {
                self.refuse("the host's own display could not be put at its mode", error);
                self.pending = ask;
                false
            }
        }
    }

    /// Plug the monitor at `mode`, bound to this host, in the highest free
    /// slot: an application's first free one is never the host's, and the
    /// host's display keeps one identity the system remembers it by. With
    /// `any_refresh`, at the refresh the system restores for that identity,
    /// `mode`'s where it restores none.
    fn plug(&mut self, mode: Mode, any_refresh: bool) -> bool {
        if self.controller.is_none() {
            match Controller::open() {
                Ok(controller) => self.controller = Some(controller),
                Err(error) => {
                    self.refuse("the host's own display driver could not be opened", error);
                    return false;
                }
            }
        }
        let Some(controller) = self.controller.as_mut() else {
            return false;
        };
        let taken: Vec<u8> = controller
            .list()
            .map(|monitors| monitors.iter().map(|monitor| monitor.slot).collect())
            .unwrap_or_default();
        let free = (0..MAX_MONITORS)
            .rev()
            .filter_map(|slot| u8::try_from(slot).ok())
            .filter(|slot| !taken.contains(slot));
        for slot in free {
            let plugged = if any_refresh {
                controller.plug_at_size(mode, Some(slot))
            } else {
                controller.plug(mode, Lifetime::Bound, false, Some(slot))
            };
            match plugged {
                Ok(monitor) => {
                    lowlat_common::log_info!(
                        "display: the host's own display plugged, slot={slot} {mode} {}",
                        monitor.name
                    );
                    self.slot = Some(slot);
                    if !monitor.name.is_empty() {
                        publish_own(Some((monitor.name.as_str(), slot)));
                    }
                    self.refused = None;
                    return true;
                }
                // Taken between the list and the plug, by another program.
                Err(Error::SlotTaken) => {}
                // **Plugged, and the system has not finished.** The slot is
                // the host's; the next look finds it shown and puts its mode.
                Err(Error::NotReady(_)) => {
                    lowlat_common::log_warn!(
                        "display: the host's own display plugged in slot {slot}, the system has \
                         not shown it yet"
                    );
                    self.slot = Some(slot);
                    return false;
                }
                Err(Error::Absent) => {
                    self.lost();
                    self.refuse("the host's own display driver went", Error::Absent);
                    return false;
                }
                Err(error) => {
                    self.refuse("the host's own display could not be plugged", error);
                    return false;
                }
            }
        }
        self.refuse("the host's own display has no free slot", Error::Full);
        false
    }

    /// Plug the monitor, and once it is plugged take `ask` as put.
    fn plug_asked(
        &mut self,
        ask: Option<(u32, u32, Option<u32>)>,
        mode: Mode,
        any_refresh: bool,
    ) -> bool {
        let plugged = self.plug(mode, any_refresh);
        if plugged {
            self.asked = ask;
            self.left = None;
            self.pending = None;
        }
        plugged
    }

    /// The driver's handle followed its driver away, and the monitor bound to
    /// it with it: nothing left to name, and a new handle at the next look.
    fn lost(&mut self) {
        self.controller = None;
        if self.slot.take().is_some() {
            publish_own(None);
        }
    }

    /// Say why, once until something succeeds or the reason changes.
    fn refuse(&mut self, what: &str, error: Error) {
        if self.refused != Some(error) {
            self.refused = Some(error);
            lowlat_common::log_warn!("display: {what}, {error:?}");
        }
    }
}

impl Drop for Own {
    /// **The controller's handle unplugs the monitor as it closes**, the
    /// driver's own rule for a bound one; what is left is to stop naming it.
    fn drop(&mut self) {
        if self.slot.is_some() {
            publish_own(None);
        }
    }
}

/// **Whether the host puts its mode over the display's mode `now`**: as it is
/// asked anew -- an owner's size, a refresh -- when the display is not shown,
/// and over the system's restore of the mode its store holds for the display
/// (`saved`), which another monitor's arrival and a program leaving exclusive
/// fullscreen both put back. Never over any other mode -- a program's in
/// exclusive fullscreen, one set from outside and not saved -- which the
/// stream follows instead. A program asking for exactly the store's mode is
/// the one it cannot tell from a restore.
fn put_over(anew: bool, shown: bool, now: Mode, saved: Option<Mode>) -> bool {
    anew || !shown || saved == Some(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The host's mode is put as it is asked and over the system's
    /// restore, never over another's**: a new ask is put whatever the display
    /// is at, and a display not shown is shown; the store's mode is put back
    /// over; a program's own mode is left, its refresh alone included, and so
    /// is any mode while the store's is unknown.
    #[test]
    fn the_hosts_mode_is_put_as_asked_and_over_a_restore_alone() {
        let stored = Mode::new(1920, 1080, 60);
        let program = Mode::new(1280, 720, 60);
        assert!(
            put_over(true, true, program, Some(stored)),
            "a new ask left"
        );
        assert!(
            put_over(false, false, program, Some(stored)),
            "a display not shown left"
        );
        assert!(
            put_over(false, true, stored, Some(stored)),
            "the system's restore left"
        );
        assert!(
            !put_over(false, true, program, Some(stored)),
            "a program's mode put over"
        );
        assert!(
            !put_over(false, true, Mode::new(1920, 1080, 144), Some(stored)),
            "a program's refresh put over"
        );
        assert!(
            !put_over(false, true, program, None),
            "a mode put over with the store's unknown"
        );
    }

    /// **The host's own display follows its driver**: held, then the driver
    /// ended under it -- its process, or with `LOWLAT_RESTART` naming the
    /// device's instance, the device restarted by the system, which the
    /// held handle must not refuse -- and held again until it is plugged
    /// again, on a new handle, once the driver is back. Run as the system
    /// account, which takes the driver's page, the driver installed.
    #[test]
    #[ignore = "ends or restarts the display driver; run as the system account"]
    fn the_hosts_own_display_follows_its_driver() {
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };

        let process = |slot| {
            lowlat_vdd_ctl::FramePage::open(slot)
                .expect("the driver's page")
                .process_id()
        };
        let mut own = Own::default();
        assert!(
            own.hold(1920, 1080, Some(60)),
            "the display was not plugged"
        );
        let before = process(own.slot.expect("a slot"));
        let began = Instant::now();
        match std::env::var("LOWLAT_RESTART").ok() {
            Some(instance) => {
                let out = std::process::Command::new("pnputil")
                    .args(["/restart-device", &instance])
                    .output()
                    .expect("pnputil");
                let said = String::from_utf8_lossy(&out.stdout).to_string();
                println!("pnputil after {:?}: {}", began.elapsed(), said.trim());
                assert!(
                    out.status.success() && !said.contains("pending"),
                    "the restart was refused"
                );
            }
            None => {
                // SAFETY: an identifier the system checks; the handle closed
                // once.
                unsafe {
                    let handle = OpenProcess(PROCESS_TERMINATE, 0, before);
                    assert!(
                        !handle.is_null(),
                        "the driver's process could not be opened"
                    );
                    TerminateProcess(handle, 1);
                    CloseHandle(handle);
                }
            }
        }
        while !(own.hold(1920, 1080, Some(60)) && own.plugged()) {
            assert!(
                began.elapsed() < Duration::from_secs(10),
                "the display did not come back"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        let back = began.elapsed();
        let after = process(own.slot.expect("a slot"));
        println!(
            "the display back at slot {:?} after {back:?}, the driver's process {before} then {after}",
            own.slot
        );
        assert_ne!(before, after, "the driver did not end");
    }

    /// The monitor at `slot`, as the system shows it now, until `ok` holds of
    /// it or five seconds pass.
    fn shown_until(
        own: &Own,
        slot: u8,
        what: &str,
        ok: impl Fn(&lowlat_vdd_ctl::Monitor) -> bool,
    ) -> lowlat_vdd_ctl::Monitor {
        use std::time::{Duration, Instant};

        let began = Instant::now();
        loop {
            let monitor = own
                .controller
                .as_ref()
                .expect("a handle")
                .monitor(slot)
                .expect("the monitor");
            if ok(&monitor) {
                return monitor;
            }
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "{what} never came"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// **What nobody asked for is the desk's**: held with no refresh asked,
    /// the host's own display keeps one set at the desk, and one asked is
    /// put; turned at the desk it is left turned, its mode read unturned.
    /// Run as a user, the driver installed; nothing is saved.
    #[test]
    #[ignore = "plugs and turns a monitor of the display driver"]
    fn the_hosts_own_display_leaves_the_desk_its_refresh_and_its_turn() {
        use super::super::sys::set_at_desk;
        use std::time::Duration;

        let mut own = Own::default();
        assert!(own.hold(1920, 1080, None), "the display was not plugged");
        let slot = own.slot.expect("a slot");
        let name = shown_until(&own, slot, "the display", |m| m.active).name;
        assert!(
            set_at_desk(&name, (1920, 1080, 120), false, false),
            "the desk's 120 refused"
        );
        shown_until(&own, slot, "the desk's 120", |m| m.mode.refresh_hz == 120);
        assert!(own.hold(1920, 1080, None), "the held display refused");
        std::thread::sleep(Duration::from_millis(500));
        let kept = shown_until(&own, slot, "the display", |m| m.active).mode;
        assert!(own.hold(1920, 1080, Some(60)), "the asked 60 refused");
        shown_until(&own, slot, "the asked 60", |m| m.mode.refresh_hz == 60);
        assert!(
            set_at_desk(&name, (1920, 1080, 60), true, false),
            "the desk's turn refused"
        );
        shown_until(&own, slot, "the desk's turn", |m| m.turned);
        assert!(own.hold(1920, 1080, Some(60)), "the turned display refused");
        std::thread::sleep(Duration::from_millis(500));
        let turned = shown_until(&own, slot, "the display", |m| m.active);
        assert!(
            set_at_desk(&name, (1920, 1080, 60), false, false),
            "the turn back refused"
        );
        println!("the desk's 120 held as {kept}, turned as {}", turned.mode);
        assert_eq!(kept.refresh_hz, 120, "the desk's refresh put back");
        assert!(turned.turned, "the desk's turn undone");
        assert_eq!(
            (turned.mode.width, turned.mode.height),
            (1920, 1080),
            "the mode read turned"
        );
    }

    /// **A turn from outside is left, and a mode put after keeps it**: the
    /// host's own display turned a quarter as `lowlatd` turns it for a guest
    /// -- by its name, through the system's display configuration, as its
    /// display settings do -- then put at another size while turned, its
    /// sides swapped where the desktop's own interface takes them, still
    /// turned, then turned upright the same way. Run as a user, the driver
    /// installed; nothing is saved.
    #[test]
    #[ignore = "plugs and turns a monitor of the display driver"]
    fn a_turn_from_outside_is_left_and_a_mode_keeps_it() {
        use lowlat_vdd_ctl::{Turn, put_turn};

        let mut own = Own::default();
        assert!(
            own.hold(1920, 1080, Some(60)),
            "the display was not plugged"
        );
        let slot = own.slot.expect("a slot");
        let name = shown_until(&own, slot, "the display", |m| m.active).name;
        put_turn(&name, Turn::Quarter).expect("the turn refused");
        shown_until(&own, slot, "the turn", |m| m.turned);
        assert!(
            own.hold(2560, 1440, Some(60)),
            "a mode refused while turned"
        );
        let resized = shown_until(&own, slot, "the new mode", |m| {
            (m.mode.width, m.mode.height) == (2560, 1440)
        });
        put_turn(&resized.name, Turn::Upright).expect("the turn back refused");
        let upright = shown_until(&own, slot, "upright again", |m| !m.turned);
        println!(
            "turned and put at {} turned {}; upright at {}",
            resized.mode, resized.turned, upright.mode
        );
        assert!(resized.turned, "the mode undid the turn");
        assert_eq!(
            upright.mode,
            Mode::new(2560, 1440, 60),
            "upright at another mode"
        );
    }

    /// **A refresh saved at the desk comes back with the display**: put and
    /// saved as the display settings save it, then the display unplugged with
    /// its handle and plugged again with no refresh asked, it arrives at that
    /// refresh. Saves the arrangement of this machine's monitors with the
    /// display among them, as the display settings do, and saves it at 60
    /// after. Run as a user, the driver installed.
    #[test]
    #[ignore = "plugs a monitor of the display driver and saves its arrangement"]
    fn a_refresh_saved_at_the_desk_comes_back_with_the_hosts_own_display() {
        use super::super::sys::set_at_desk;
        use std::time::{Duration, Instant};

        let mut own = Own::default();
        assert!(own.hold(1920, 1080, None), "the display was not plugged");
        let slot = own.slot.expect("a slot");
        let name = shown_until(&own, slot, "the display", |m| m.active).name;
        assert!(
            set_at_desk(&name, (1920, 1080, 120), false, true),
            "the desk's 120 refused"
        );
        shown_until(&own, slot, "the desk's 120", |m| m.mode.refresh_hz == 120);
        drop(own);
        std::thread::sleep(Duration::from_secs(2));
        let mut again = Own::default();
        assert!(
            again.hold(1920, 1080, None),
            "the display was not plugged again"
        );
        assert_eq!(again.slot, Some(slot), "another slot, another identity");
        // **The system's restore can come seconds after the arrival.**
        let began = Instant::now();
        let mut back = shown_until(&again, slot, "the display", |m| m.active);
        while back.mode.refresh_hz != 120 && began.elapsed() < Duration::from_secs(6) {
            std::thread::sleep(Duration::from_millis(200));
            back = shown_until(&again, slot, "the display", |m| m.active);
        }
        let saved_at_60 = set_at_desk(&back.name, (1920, 1080, 60), false, true);
        println!(
            "plugged again at {} after {:?}; saved at 60 after {saved_at_60}",
            back.mode,
            began.elapsed()
        );
        assert_eq!(
            back.mode.refresh_hz, 120,
            "the desk's saved refresh lost to the plug"
        );
    }

    /// **A program's own mode stands, and the system's restore is put back**:
    /// the host's own display held at a size the system's store does not hold
    /// for it, then `LOWLAT_FSE` -- a program taking it fullscreen at a size
    /// of its own, and taking that back whenever the display is not at it,
    /// `{display}` replaced by its name -- run while the display is held again
    /// whenever it is not at the host's size, as the rebuild a new size causes
    /// holds it. Nothing is put while the program holds the display; once it
    /// leaves, the system puts the store's mode back, and the host its own.
    /// Run as a user, the driver installed; nothing is saved.
    #[test]
    #[ignore = "plugs a monitor of the display driver and takes it fullscreen"]
    fn a_programs_own_mode_stands_and_a_restore_is_put_back() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let mut own = Own::default();
        assert!(own.hold(1920, 1080, None), "the display was not plugged");
        let slot = own.slot.expect("a slot");
        let shown = shown_until(&own, slot, "the display", |m| m.active);
        let saved = lowlat_vdd_ctl::saved_mode(&shown).expect("the store's mode for it");
        let (width, height) = if (saved.width, saved.height) == (2560, 1440) {
            (1920, 1080)
        } else {
            (2560, 1440)
        };
        assert!(own.hold(width, height, None), "the host's size refused");
        shown_until(&own, slot, "the host's size", |m| {
            (m.mode.width, m.mode.height) == (width, height)
        });
        println!(
            "slot {slot} {}: the store's {saved}, held at {width}x{height}",
            shown.name
        );

        let line = std::env::var("LOWLAT_FSE")
            .expect("LOWLAT_FSE")
            .replace("{display}", &shown.name);
        let mut words = line.split_whitespace();
        let mut program = Command::new(words.next().expect("a program"))
            .args(words)
            .spawn()
            .expect("the program");
        let began = Instant::now();
        let (mut theirs, mut over, mut ended, mut back) = (None, 0u32, None::<Instant>, None);
        while back.is_none() && ended.is_none_or(|at| at.elapsed() < Duration::from_secs(5)) {
            let at = began.elapsed().as_secs_f64() * 1000.0;
            let monitor = own
                .controller
                .as_ref()
                .expect("a handle")
                .monitor(slot)
                .expect("the monitor");
            if (monitor.mode.width, monitor.mode.height) == (width, height) {
                if ended.is_some() {
                    back = Some(at);
                }
            } else if monitor.active {
                // **The store's mode is the system's restore** -- the program
                // leaving puts it back before it ends -- and anything else
                // is the program's.
                let restored = monitor.mode == saved;
                if !restored && theirs.is_none() {
                    println!("{at:9.1} the program's {}", monitor.mode);
                    theirs = Some(monitor.mode);
                }
                if own.hold(width, height, None) {
                    println!("{at:9.1} the host's size put over {}", monitor.mode);
                    over += u32::from(!restored);
                }
            }
            if ended.is_none() && program.try_wait().expect("the program").is_some() {
                println!("{at:9.1} the program ended");
                ended = Some(Instant::now());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = program.kill();
        let _ = program.wait();
        println!("put over the program {over} time(s), back at the host's size at {back:?} ms");
        assert!(theirs.is_some(), "the program put no mode of its own");
        assert_eq!(over, 0, "the host's size put over the program's mode");
        assert!(
            back.is_some(),
            "the host's size not put back after the program"
        );
    }
}
