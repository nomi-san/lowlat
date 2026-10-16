//! The driver's control interface: found, opened, and asked.
//!
//! **A handle follows its driver** ([docs/11-drivers.md](../../../docs/11-drivers.md)
//! section 1.8). An open handle refuses the removal an update or a restart
//! of the driver needs, and the device then waits for a reboot; so the handle
//! is closed when the system asks for the device back, and when a call finds
//! the driver gone. The monitors bound to it go with it, as they would with
//! the driver, and every call after answers that the driver is absent: a new
//! open is a new handle, so what was bound to the old one is never taken for
//! the new one's.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Mutex, PoisonError};

use lowlat_vdd_proto::CONTROL_INTERFACE;
use lowlat_vdd_proto::control::{
    FrameChannel, Frames, IOCTL_FRAMES, IOCTL_LIST, IOCTL_PLUG, IOCTL_RELOAD, IOCTL_REPLUG,
    IOCTL_UNPLUG, IOCTL_VERSION, List, Plug, Plugged, Replug, Unplug, VERSION, Version,
};
use lowlat_vdd_proto::modes::Mode;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_MapCrToWin32Err, CM_NOTIFY_ACTION,
    CM_NOTIFY_ACTION_DEVICEQUERYREMOVE, CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE,
    CM_NOTIFY_ACTION_DEVICEREMOVEPENDING, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
    CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_1, CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
    CM_Register_Notification, CM_Unregister_Notification, CR_SUCCESS, HCMNOTIFICATION,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BAD_COMMAND, ERROR_BUSY, ERROR_DRIVER_PROCESS_TERMINATED,
    ERROR_GEN_FAILURE, ERROR_INVALID_PARAMETER, ERROR_NO_SYSTEM_RESOURCES, ERROR_SUCCESS,
    GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::core::GUID;

use crate::Error;

/// The handle, while the driver has not asked for it back: shared with the
/// system's notifications about the device, which close it from a thread of
/// the system's.
#[derive(Debug)]
struct Held(Mutex<Option<OwnedHandle>>);

impl Held {
    /// Close the handle, once; a lock a panic poisoned still holds it.
    fn close(&self) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
    }
}

/// An open handle on the driver: what it plugs bound is its own.
#[derive(Debug)]
pub(crate) struct Device {
    /// Boxed, so the notification's context stays where it was registered.
    held: Box<Held>,
    notification: HCMNOTIFICATION,
}

// SAFETY: a file handle any thread may use, behind a lock; the registration
// is the system's value, released once. The controller's calls on one handle
// take turns above this.
unsafe impl Send for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        if !self.notification.is_null() {
            // SAFETY: this device's registration, released once and never
            // from its own notification; the release waits for one in flight,
            // so the context outlives every notification. The handle, if still
            // open, closes with `held` after.
            unsafe { CM_Unregister_Notification(self.notification) };
        }
    }
}

/// Whether the system's word about the device ends the handle: asked for the
/// device back, or telling it went.
fn closes(action: CM_NOTIFY_ACTION) -> bool {
    matches!(
        action,
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE
            | CM_NOTIFY_ACTION_DEVICEREMOVEPENDING
            | CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE
    )
}

/// **Closed before this returns**, so the removal is not refused for it.
unsafe extern "system" fn notified(
    _notification: HCMNOTIFICATION,
    context: *const core::ffi::c_void,
    action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    if closes(action) {
        // SAFETY: the context registered with this notification, alive until
        // the registration is released.
        if let Some(held) = unsafe { context.cast::<Held>().as_ref() } {
            held.close();
        }
    }
    ERROR_SUCCESS
}

/// Whether the system's answer to a request says the driver behind the handle
/// has gone rather than answering: its process ended, which leaves the handle
/// reaching nothing -- the system starts the driver again behind a new one
/// (measured: answered within 2 ms of the end, and no notification first).
fn gone(code: u32) -> bool {
    code == ERROR_DRIVER_PROCESS_TERMINATED
}

/// The interface's path, the first the system lists.
fn interface() -> Option<Vec<u16>> {
    let id = CONTROL_INTERFACE;
    let guid = GUID {
        data1: id.data1,
        data2: id.data2,
        data3: id.data3,
        data4: id.data4,
    };
    let mut len = 0u32;
    // SAFETY: live outputs; no device instance named.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut len,
            &raw const guid,
            core::ptr::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS || len <= 1 {
        return None;
    }
    let mut list = vec![0u16; usize::try_from(len).ok()?];
    // SAFETY: a buffer of the length asked for.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            &raw const guid,
            core::ptr::null(),
            list.as_mut_ptr(),
            len,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        return None;
    }
    let end = list.iter().position(|&c| c == 0)?;
    list.truncate(end + 1);
    (end > 0).then_some(list)
}

impl Device {
    /// Open the driver and check it speaks this controller's protocol.
    pub(crate) fn open() -> Result<Self, Error> {
        let path = interface().ok_or(Error::Absent)?;
        // SAFETY: a terminated path; no security attributes, no template.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                core::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: no precondition.
            return Err(match unsafe { GetLastError() } {
                ERROR_ACCESS_DENIED => Error::Denied,
                code => Error::Io(code),
            });
        }
        // SAFETY: a handle this now owns, closed once.
        let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut device = Self {
            held: Box::new(Held(Mutex::new(Some(owned)))),
            notification: core::ptr::null_mut(),
        };
        let filter = CM_NOTIFY_FILTER {
            cbSize: u32::try_from(size_of::<CM_NOTIFY_FILTER>()).unwrap_or(0),
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
            u: CM_NOTIFY_FILTER_0 {
                DeviceHandle: CM_NOTIFY_FILTER_0_1 { hTarget: handle },
            },
            ..Default::default()
        };
        // SAFETY: a live filter; the context is the boxed state, which
        // outlives the registration (`Drop`).
        let cr = unsafe {
            CM_Register_Notification(
                &raw const filter,
                (&raw const *device.held).cast(),
                Some(notified),
                &raw mut device.notification,
            )
        };
        // **A handle that cannot follow its driver is not kept**: it would
        // refuse the driver's next update.
        if cr != CR_SUCCESS {
            // SAFETY: no precondition.
            return Err(Error::Io(unsafe {
                CM_MapCrToWin32Err(cr, ERROR_GEN_FAILURE)
            }));
        }
        let mut out = [0u8; Version::LEN];
        let len = device.ask(IOCTL_VERSION, &[], &mut out)?;
        let version = Version::decode(out.get(..len).unwrap_or(&[]))
            .map_err(|_| Error::Version(0))?
            .version;
        if version != VERSION {
            return Err(Error::Version(version));
        }
        Ok(device)
    }

    /// One request, answered with the reply's length or the system's error;
    /// [`Error::Absent`] once the handle has followed its driver away.
    ///
    /// **The request runs under the handle's lock**, so the system's word
    /// that the driver is going waits for it rather than closing the handle
    /// beneath it.
    fn ask(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, Error> {
        let mut held = self.held.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(handle) = held.as_ref() else {
            return Err(Error::Absent);
        };
        let mut got = 0u32;
        // SAFETY: live buffers of the lengths given; a synchronous handle.
        let ok = unsafe {
            DeviceIoControl(
                handle.as_raw_handle(),
                code,
                input.as_ptr().cast(),
                u32::try_from(input.len()).unwrap_or(0),
                output.as_mut_ptr().cast(),
                u32::try_from(output.len()).unwrap_or(0),
                &raw mut got,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: no precondition.
            let code = unsafe { GetLastError() };
            if gone(code) {
                held.take();
                return Err(Error::Absent);
            }
            return Err(Error::Io(code));
        }
        Ok(usize::try_from(got).unwrap_or(0))
    }

    /// The driver's answers, read for what they mean for a monitor.
    fn monitor_error(error: Error, slot_asked: bool) -> Error {
        match error {
            Error::Io(ERROR_ACCESS_DENIED) => Error::NotOwner,
            Error::Io(ERROR_NO_SYSTEM_RESOURCES) => Error::Full,
            Error::Io(ERROR_BUSY) if slot_asked => Error::SlotTaken,
            Error::Io(ERROR_BUSY) => Error::Busy,
            Error::Io(ERROR_INVALID_PARAMETER) => Error::NoMonitor,
            Error::Io(ERROR_BAD_COMMAND) => Error::Starting,
            other => other,
        }
    }

    pub(crate) fn plug(&self, plug: Plug) -> Result<Plugged, Error> {
        let mut input = [0u8; Plug::LEN];
        plug.encode(&mut input).map_err(|_| Error::Argument)?;
        let mut out = [0u8; Plugged::LEN];
        let len = self
            .ask(IOCTL_PLUG, &input, &mut out)
            .map_err(|e| Self::monitor_error(e, plug.slot.is_some()))?;
        Plugged::decode(out.get(..len).unwrap_or(&[])).map_err(|_| Error::Version(VERSION))
    }

    pub(crate) fn unplug(&self, slot: u8) -> Result<(), Error> {
        let mut input = [0u8; Unplug::LEN];
        Unplug { slot }
            .encode(&mut input)
            .map_err(|_| Error::Argument)?;
        self.ask(IOCTL_UNPLUG, &input, &mut [])
            .map_err(|e| Self::monitor_error(e, false))?;
        Ok(())
    }

    pub(crate) fn replug(&self, slot: u8, mode: Mode) -> Result<Plugged, Error> {
        let mut input = [0u8; Replug::LEN];
        Replug { slot, mode }
            .encode(&mut input)
            .map_err(|_| Error::Argument)?;
        let mut out = [0u8; Plugged::LEN];
        let len = self
            .ask(IOCTL_REPLUG, &input, &mut out)
            .map_err(|e| Self::monitor_error(e, false))?;
        Plugged::decode(out.get(..len).unwrap_or(&[])).map_err(|_| Error::Version(VERSION))
    }

    pub(crate) fn list(&self) -> Result<List, Error> {
        let mut out = vec![0u8; List::LEN];
        let len = self.ask(IOCTL_LIST, &[], &mut out)?;
        List::decode(out.get(..len).unwrap_or(&[])).map_err(|_| Error::Version(VERSION))
    }

    pub(crate) fn reload(&self) -> Result<(), Error> {
        self.ask(IOCTL_RELOAD, &[], &mut [])?;
        Ok(())
    }

    pub(crate) fn frames(&self, slot: u8) -> Result<FrameChannel, Error> {
        let mut input = [0u8; Frames::LEN];
        Frames { slot }
            .encode(&mut input)
            .map_err(|_| Error::Argument)?;
        let mut out = [0u8; FrameChannel::LEN];
        let len = self
            .ask(IOCTL_FRAMES, &input, &mut out)
            .map_err(|e| Self::monitor_error(e, false))?;
        FrameChannel::decode(out.get(..len).unwrap_or(&[])).map_err(|_| Error::Version(VERSION))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_NOTIFY_ACTION_DEVICECUSTOMEVENT, CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED,
    };
    use windows_sys::Win32::System::Threading::CreateEventW;

    /// A device around a handle of the test's own, an event, standing in for
    /// the driver's: its requests fail, which is not what is asked of it.
    fn standing_in() -> Device {
        // SAFETY: no attributes, no name; a handle this owns.
        let event = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        assert!(!event.is_null(), "no event");
        // SAFETY: a handle this owns, closed once.
        let owned = unsafe { OwnedHandle::from_raw_handle(event) };
        Device {
            held: Box::new(Held(Mutex::new(Some(owned)))),
            notification: core::ptr::null_mut(),
        }
    }

    fn told(device: &Device, action: CM_NOTIFY_ACTION) -> u32 {
        // SAFETY: the context a registration would carry, alive for the call.
        unsafe {
            notified(
                core::ptr::null_mut(),
                (&raw const *device.held).cast(),
                action,
                core::ptr::null(),
                0,
            )
        }
    }

    /// **Asked for the device back, the handle closes before the answer**,
    /// and every request after answers that the driver is absent.
    #[test]
    fn the_handle_closes_when_its_driver_is_asked_for() {
        for action in [
            CM_NOTIFY_ACTION_DEVICEQUERYREMOVE,
            CM_NOTIFY_ACTION_DEVICEREMOVEPENDING,
            CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE,
        ] {
            let device = standing_in();
            assert_ne!(
                device.ask(IOCTL_LIST, &[], &mut [0u8; 8]),
                Err(Error::Absent),
                "a live handle answered absent"
            );
            assert_eq!(
                told(&device, action),
                ERROR_SUCCESS,
                "the removal was refused"
            );
            assert!(
                device.held.0.lock().is_ok_and(|held| held.is_none()),
                "still open"
            );
            assert_eq!(
                device.ask(IOCTL_LIST, &[], &mut [0u8; 8]),
                Err(Error::Absent)
            );
        }
    }

    /// The system's other words leave the handle as it was: a removal another
    /// refused, and a device's own events.
    #[test]
    fn the_handle_stays_for_any_other_word() {
        for action in [
            CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED,
            CM_NOTIFY_ACTION_DEVICECUSTOMEVENT,
        ] {
            let device = standing_in();
            assert_eq!(told(&device, action), ERROR_SUCCESS);
            assert!(
                device.held.0.lock().is_ok_and(|held| held.is_some()),
                "closed"
            );
        }
    }

    /// The answer that says the driver went is told from its own answers --
    /// a refusal, a busy slot, an adapter still starting.
    #[test]
    fn a_driver_gone_is_told_from_its_answers() {
        assert!(
            gone(ERROR_DRIVER_PROCESS_TERMINATED),
            "an ended driver not taken for gone"
        );
        for code in [
            ERROR_ACCESS_DENIED,
            ERROR_BUSY,
            ERROR_BAD_COMMAND,
            ERROR_INVALID_PARAMETER,
            ERROR_NO_SYSTEM_RESOURCES,
        ] {
            assert!(!gone(code), "{code} taken for gone");
        }
    }

    /// **A handle follows its driver away**: a bound monitor plugged, then the
    /// driver ended -- its process, or with `LOWLAT_RESTART` naming the
    /// device's instance, its device restarted by the system -- while the
    /// handle is open. The handle then answers absent, the restart goes
    /// through rather than being refused for it, and a new handle opens on the
    /// driver come back and plugs again. Run elevated, the driver installed.
    #[test]
    #[ignore = "ends or restarts the display driver; run elevated"]
    fn a_handle_follows_its_driver_away() {
        use crate::{Controller, Lifetime};
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };

        let mode = Mode::new(1920, 1080, 60);
        let mut controller = Controller::open().expect("the display driver");
        let monitor = controller
            .plug(mode, Lifetime::Bound, false, None)
            .expect("a monitor");
        let process = |slot| {
            Device::open()
                .and_then(|device| device.frames(slot))
                .map(|answer| answer.process)
        };
        let before = process(monitor.slot).expect("the driver's process");
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
        let answered = loop {
            match controller.list() {
                Err(error) => break error,
                Ok(_) if began.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(_) => panic!("the handle still answers 5 s after the driver ended"),
            }
        };
        println!(
            "the old handle answered {answered:?} after {:?}",
            began.elapsed()
        );
        assert_eq!(answered, Error::Absent, "the old handle answered otherwise");
        let mut again = loop {
            match Controller::open() {
                Ok(controller) => break controller,
                Err(_) if began.elapsed() < Duration::from_secs(10) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => panic!("the driver did not come back: {error:?}"),
            }
        };
        let plugged = loop {
            match again.plug(mode, Lifetime::Bound, false, Some(monitor.slot)) {
                Ok(plugged) => break plugged,
                Err(_) if began.elapsed() < Duration::from_secs(10) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => panic!("no monitor on the driver come back: {error:?}"),
            }
        };
        let after = process(plugged.slot).expect("the driver's process");
        println!(
            "plugged again at slot {} after {:?}, the driver's process {before} then {after}",
            plugged.slot,
            began.elapsed()
        );
        assert_ne!(before, after, "the driver did not end");
        assert_eq!(
            controller.list(),
            Err(Error::Absent),
            "the old handle came back"
        );
    }
}
