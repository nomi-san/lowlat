//! The host's own input driver, from the host's side (docs/11-drivers.md
//! section 2): a handle per device, a device of one kind per handle, input
//! reports written whole, and what the system writes to a device read back by
//! a request kept outstanding. **The handle's end removes the device**, after
//! the driver submits its neutral reports, so a host that ends -- a killed one
//! included -- leaves nothing held.
//!
//! The handle is opened for overlapped requests: one that is not serializes
//! them, and the read kept outstanding would hold every report behind it.
//!
//! **A handle follows its driver.** An open one refuses the removal an update
//! or a removal of the driver needs, which then waits for a reboot; so it is
//! closed when the system asks for the device back, and when a request finds
//! the driver's process gone. Its device goes with it, and every request after
//! answers [`Error::Absent`].

use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Mutex, MutexGuard, PoisonError};

use lowlat_hid_proto::control::{
    Create, Features, IOCTL_CREATE, IOCTL_VERSION, IOCTL_WRITTEN, VERSION, Version, Written,
};
use lowlat_hid_proto::{CONTROL_INTERFACE, Guid, Kind, container_id};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DRP_BASE_CONTAINERID, CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
    CM_Get_DevNode_Registry_PropertyW, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW,
    CM_MapCrToWin32Err, CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEQUERYREMOVE,
    CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE, CM_NOTIFY_ACTION_DEVICEREMOVEPENDING,
    CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_0, CM_NOTIFY_FILTER_0_1,
    CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE, CM_Register_Notification, CM_Unregister_Notification,
    CR_SUCCESS, HCMNOTIFICATION,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_DRIVER_PROCESS_TERMINATED, ERROR_GEN_FAILURE,
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING,
    ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::UI::Input::{
    GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICENAME,
    RIM_TYPEHID, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows_sys::core::GUID;

/// Why a device was not made, or a request not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No input driver installed, or none started; or the driver asked for
    /// its device back, or its process ended, and the handle went with it.
    Absent,
    /// The driver's interface admits the system account and administrators
    /// alone.
    Denied,
    /// A driver speaking another version of the requests; the two ship
    /// together.
    Version(u32),
    /// The driver or the system refused, with the system's error.
    Refused(u32),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Absent => f.write_str("no input driver installed or started"),
            Self::Denied => {
                f.write_str("the input driver admits the system account and administrators alone")
            }
            Self::Version(version) => write!(f, "the input driver speaks version {version}"),
            Self::Refused(code) => write!(f, "the input driver refused, error={code}"),
        }
    }
}

impl std::error::Error for Error {}

fn last_error() -> u32 {
    // SAFETY: no precondition.
    unsafe { GetLastError() }
}

/// Whether a request's error says the driver behind the handle has gone
/// rather than answering: its process ended, which leaves the handle reaching
/// nothing, and the system starts the driver again behind a new one.
fn gone(code: u32) -> bool {
    code == ERROR_DRIVER_PROCESS_TERMINATED
}

/// The interface's path, the first the system lists, terminated.
fn interface() -> Option<Vec<u16>> {
    let guid = GUID {
        data1: CONTROL_INTERFACE.data1,
        data2: CONTROL_INTERFACE.data2,
        data3: CONTROL_INTERFACE.data3,
        data4: CONTROL_INTERFACE.data4,
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
    let end = list.iter().position(|&unit| unit == 0)?;
    list.truncate(end + 1);
    (end > 0).then_some(list)
}

/// The interface, opened for overlapped requests.
fn open() -> Result<OwnedHandle, Error> {
    let path = interface().ok_or(Error::Absent)?;
    // SAFETY: a terminated path; no security attributes, no template.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            core::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(match last_error() {
            ERROR_ACCESS_DENIED => Error::Denied,
            code => Error::Refused(code),
        });
    }
    // SAFETY: a handle this now owns, closed once.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// An event an overlapped request signals, reset by the system as each
/// request starts.
fn event() -> Result<OwnedHandle, Error> {
    // SAFETY: no attributes, manual reset, not signalled, no name.
    let raw = unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) };
    if raw.is_null() {
        return Err(Error::Refused(last_error()));
    }
    // SAFETY: a handle this now owns, closed once.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

fn overlapped(event: &OwnedHandle) -> OVERLAPPED {
    OVERLAPPED {
        hEvent: event.as_raw_handle(),
        ..Default::default()
    }
}

/// The end of an overlapped request: its bytes, or the system's error.
/// Without `wait`, a request still running is `ERROR_IO_INCOMPLETE`.
fn finish(handle: HANDLE, overlapped: &mut OVERLAPPED, wait: bool) -> Result<u32, u32> {
    let mut bytes = 0u32;
    // SAFETY: a request issued on `handle` with this structure, which lives
    // until the request ends.
    let ok = unsafe { GetOverlappedResult(handle, overlapped, &raw mut bytes, i32::from(wait)) };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(bytes)
}

/// One control request on `handle`, waited for: the reply's length, or the
/// system's error.
fn control(
    handle: HANDLE,
    event: &OwnedHandle,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> Result<usize, u32> {
    let mut overlapped = overlapped(event);
    // SAFETY: live buffers of their stated lengths and a structure that lives
    // until the request ends, waited for below.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            code,
            input.as_ptr().cast(),
            u32::try_from(input.len()).unwrap_or(0),
            output.as_mut_ptr().cast(),
            u32::try_from(output.len()).unwrap_or(0),
            core::ptr::null_mut(),
            &raw mut overlapped,
        )
    };
    if ok == 0 {
        let code = last_error();
        if code != ERROR_IO_PENDING {
            return Err(code);
        }
    }
    finish(handle, &mut overlapped, true).map(|bytes| usize::try_from(bytes).unwrap_or(0))
}

/// Whether the driver behind `handle` speaks this version of the requests.
fn speaks(handle: HANDLE, event: &OwnedHandle) -> Result<(), Error> {
    let mut answer = [0u8; Version::LEN];
    let len = control(handle, event, IOCTL_VERSION, &[], &mut answer).map_err(Error::Refused)?;
    let version = answer
        .get(..len)
        .and_then(|bytes| Version::decode(bytes).ok())
        .ok_or(Error::Version(0))?;
    if version.version != VERSION {
        return Err(Error::Version(version.version));
    }
    Ok(())
}

/// Whether this process can make devices: the driver installed and started,
/// this process admitted -- the system account, or an administrator elevated
/// -- and the driver speaking these requests. Nothing is made.
pub fn probe() -> Result<(), Error> {
    let handle = open()?;
    let event = event()?;
    speaks(handle.as_raw_handle(), &event)
}

/// The handle, while the driver has not asked for its device back: shared
/// with the system's notifications about that device, which close it from a
/// thread of the system's.
#[derive(Debug)]
struct Held(Mutex<Option<OwnedHandle>>);

impl Held {
    fn lock(&self) -> MutexGuard<'_, Option<OwnedHandle>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
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

/// **Closed before this returns**, so the removal is not refused for it. A
/// request in flight on the handle finishes first: it holds the lock.
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
            held.lock().take();
        }
    }
    ERROR_SUCCESS
}

/// How long a device's end waits for its read to end once cancelled. Past it
/// the read's buffer is let go of rather than freed, so a late answer writes
/// nothing that is someone else's.
const READ_ENDS_MS: u32 = 5_000;

/// The read of what the system wrote, kept outstanding between polls: its
/// buffer and structure stay put while the system may write them.
struct Read {
    event: OwnedHandle,
    overlapped: OVERLAPPED,
    buffer: [u8; Written::LEN],
    pending: bool,
}

impl Read {
    /// Wait for the read to end, cancelled or closed beneath it: true once
    /// the system writes it no more.
    fn ended(&self) -> bool {
        // SAFETY: the read's own event, which the system signals as it ends.
        unsafe { WaitForSingleObject(self.event.as_raw_handle(), READ_ENDS_MS) == WAIT_OBJECT_0 }
    }
}

/// One device of the host's own input driver, removed when this goes.
pub struct Device {
    /// Boxed, so the notification's context stays where it was registered.
    held: Box<Held>,
    notification: HCMNOTIFICATION,
    kind: Kind,
    event: OwnedHandle,
    /// Freed at the end only once the system writes it no more.
    read: ManuallyDrop<Box<Read>>,
}

// SAFETY: handles any thread may use, the device's behind its lock; the
// registration is the system's value, released once; the read's buffer is
// this device's own and is read only by whoever holds the device.
unsafe impl Send for Device {}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").field("kind", &self.kind).finish()
    }
}

impl Device {
    /// Make a device of `kind` in `slot`, answering `features` beside the
    /// kind's own -- a pad's peer's calibration and firmware, the pairing the
    /// host made up for it.
    pub fn create(kind: Kind, slot: u8, features: Features) -> Result<Self, Error> {
        let handle = open()?;
        let target = handle.as_raw_handle();
        let read_event = event()?;
        let mut device = Self {
            held: Box::new(Held(Mutex::new(Some(handle)))),
            notification: core::ptr::null_mut(),
            kind,
            event: event()?,
            read: ManuallyDrop::new(Box::new(Read {
                overlapped: overlapped(&read_event),
                event: read_event,
                buffer: [0; Written::LEN],
                pending: false,
            })),
        };
        let filter = CM_NOTIFY_FILTER {
            cbSize: u32::try_from(size_of::<CM_NOTIFY_FILTER>()).unwrap_or(0),
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
            u: CM_NOTIFY_FILTER_0 {
                DeviceHandle: CM_NOTIFY_FILTER_0_1 { hTarget: target },
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
            return Err(Error::Refused(unsafe {
                CM_MapCrToWin32Err(cr, ERROR_GEN_FAILURE)
            }));
        }
        let mut answer = [0u8; Version::LEN];
        let len = device.control(IOCTL_VERSION, &[], &mut answer)?;
        let version = answer
            .get(..len)
            .and_then(|bytes| Version::decode(bytes).ok())
            .ok_or(Error::Version(0))?;
        if version.version != VERSION {
            return Err(Error::Version(version.version));
        }
        let mut request = [0u8; Create::LEN];
        Create {
            kind,
            slot,
            features,
        }
        .encode(&mut request)
        .map_err(|_| Error::Refused(ERROR_INVALID_PARAMETER))?;
        device.control(IOCTL_CREATE, &request, &mut [])?;
        Ok(device)
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// One control request, waited for.
    ///
    /// **Under the handle's lock**, so the system's word that the driver is
    /// going waits for it rather than closing the handle beneath it.
    fn control(&mut self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, Error> {
        let mut held = self.held.lock();
        let handle = held.as_ref().ok_or(Error::Absent)?.as_raw_handle();
        match control(handle, &self.event, code, input, output) {
            Ok(len) => Ok(len),
            Err(code) if gone(code) => {
                held.take();
                Err(Error::Absent)
            }
            Err(code) => Err(Error::Refused(code)),
        }
    }

    /// One input report, identifier byte first, as the kind declares it; a
    /// report it does not declare whole is refused.
    pub fn write(&mut self, report: &[u8]) -> Result<(), Error> {
        let mut held = self.held.lock();
        let handle = held.as_ref().ok_or(Error::Absent)?.as_raw_handle();
        let mut overlapped = overlapped(&self.event);
        // SAFETY: a live buffer of its stated length and a structure that
        // lives until the request ends, waited for below.
        let ok = unsafe {
            WriteFile(
                handle,
                report.as_ptr(),
                u32::try_from(report.len()).unwrap_or(0),
                core::ptr::null_mut(),
                &raw mut overlapped,
            )
        };
        let mut ended = Ok(0);
        if ok == 0 {
            let code = last_error();
            if code != ERROR_IO_PENDING {
                ended = Err(code);
            }
        }
        match ended.and_then(|_| finish(handle, &mut overlapped, true)) {
            Ok(_) => Ok(()),
            Err(code) if gone(code) => {
                held.take();
                Err(Error::Absent)
            }
            Err(code) => Err(Error::Refused(code)),
        }
    }

    /// What the system wrote to the device since the last poll, without
    /// waiting: the read stays outstanding between polls, and none is the
    /// answer while nothing came.
    pub fn written(&mut self) -> Result<Option<Written>, Error> {
        let held = self.held.lock();
        let read = &mut **self.read;
        let Some(handle) = held.as_ref().map(AsRawHandle::as_raw_handle) else {
            // Closed beneath a read, which ended with the handle.
            if read.pending && read.ended() {
                read.pending = false;
            }
            return Err(Error::Absent);
        };
        if !read.pending {
            read.overlapped = overlapped(&read.event);
            // SAFETY: the read's own buffer and structure, which stay put in
            // their box until the request ends (`Drop` waits for it).
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_WRITTEN,
                    core::ptr::null(),
                    0,
                    read.buffer.as_mut_ptr().cast(),
                    u32::try_from(read.buffer.len()).unwrap_or(0),
                    core::ptr::null_mut(),
                    &raw mut read.overlapped,
                )
            };
            if ok == 0 {
                let code = last_error();
                if code != ERROR_IO_PENDING {
                    return Err(Error::Refused(code));
                }
            }
            read.pending = true;
        }
        match finish(handle, &mut read.overlapped, false) {
            Err(ERROR_IO_INCOMPLETE) => Ok(None),
            Err(code) => {
                read.pending = false;
                Err(Error::Refused(code))
            }
            Ok(bytes) => {
                read.pending = false;
                let len = usize::try_from(bytes).unwrap_or(0);
                read.buffer
                    .get(..len)
                    .and_then(|bytes| Written::decode(bytes).ok())
                    .map(Some)
                    .ok_or(Error::Refused(ERROR_INVALID_PARAMETER))
            }
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if !self.notification.is_null() {
            // SAFETY: this device's registration, released once and never
            // from its own notification; the release waits for one in flight,
            // so the context outlives every notification.
            unsafe { CM_Unregister_Notification(self.notification) };
        }
        if self.read.pending {
            let overlapped: *const OVERLAPPED = &self.read.overlapped;
            if let Some(handle) = self.held.lock().as_ref() {
                // SAFETY: the read issued on this handle.
                unsafe { CancelIoEx(handle.as_raw_handle(), overlapped) };
            }
            if !self.read.ended() {
                // Let go of rather than freed: the system may still write it.
                return;
            }
        }
        // SAFETY: dropped once, here, with nothing writing it any more.
        unsafe { ManuallyDrop::drop(&mut self.read) };
        // The handle closes after this: the driver submits the device's
        // neutral reports and removes it.
    }
}

/// Raw input's devices, listed at once: the machine's own and every device of
/// this driver's.
const LISTED_MAX: usize = 256;

/// Whether the system's raw input lists `kind`'s device in `slot`: what is
/// written to a device reaches the system from then on, and every report
/// written before is lost (docs/11-drivers.md section 2.4).
pub fn listed(kind: Kind, slot: u8) -> bool {
    let wanted = match kind {
        Kind::Keyboard => RIM_TYPEKEYBOARD,
        Kind::Mouse => RIM_TYPEMOUSE,
        Kind::DualShock4 | Kind::DualSense => RIM_TYPEHID,
    };
    let container = text(container_id(kind, slot));
    let size = u32::try_from(size_of::<RAWINPUTDEVICELIST>()).unwrap_or(0);
    let mut list = [RAWINPUTDEVICELIST::default(); LISTED_MAX];
    let mut count = u32::try_from(LISTED_MAX).unwrap_or(0);
    // SAFETY: a list of `count` entries, as the call is told.
    let mut got = unsafe { GetRawInputDeviceList(list.as_mut_ptr(), &raw mut count, size) };
    let mut more = Vec::new();
    if got == u32::MAX && last_error() == ERROR_INSUFFICIENT_BUFFER {
        // More than the list holds: `count` is how many there are now.
        more = vec![RAWINPUTDEVICELIST::default(); usize::try_from(count).unwrap_or(0)];
        // SAFETY: a list of `count` entries, as the call is told.
        got = unsafe { GetRawInputDeviceList(more.as_mut_ptr(), &raw mut count, size) };
    }
    let Ok(got) = usize::try_from(got) else {
        return false;
    };
    let listed = if more.is_empty() {
        list.get(..got)
    } else {
        more.get(..got)
    };
    listed.unwrap_or_default().iter().any(|device| {
        device.dwType == wanted
            && container_of(device.hDevice).is_some_and(|c| same(&c, &container))
    })
}

/// A GUID as the system writes one: braced, upper case,
/// `{data1-data2-data3-data4[0..2]-data4[2..8]}`.
fn text(id: Guid) -> [u16; 38] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    // The thirty-two digits in the order they are written.
    let value = (u128::from(id.data1) << 96)
        | (u128::from(id.data2) << 80)
        | (u128::from(id.data3) << 64)
        | u128::from(u64::from_be_bytes(id.data4));
    let mut digits = (0..32u32).rev().map(|digit| {
        let nibble = usize::try_from((value >> (digit * 4)) & 0xf).unwrap_or(0);
        HEX.get(nibble).copied().unwrap_or(b'0')
    });
    let mut out = [0u16; 38];
    for (at, slot) in out.iter_mut().enumerate() {
        *slot = u16::from(match at {
            0 => b'{',
            9 | 14 | 19 | 24 => b'-',
            37 => b'}',
            _ => digits.next().unwrap_or(b'0'),
        });
    }
    out
}

/// Two GUIDs' texts the same, whichever case each was written in.
fn same(a: &[u16], b: &[u16]) -> bool {
    let fold = |unit: &u16| char::from_u32(u32::from(*unit)).map(|c| c.to_ascii_uppercase());
    a.len() == b.len() && a.iter().map(fold).eq(b.iter().map(fold))
}

/// The container of the device raw input lists as `device`, as the system
/// writes it.
fn container_of(device: HANDLE) -> Option<[u16; 38]> {
    let mut name = [0u16; 512];
    let mut len = u32::try_from(name.len()).ok()?;
    // SAFETY: a buffer of `len` characters, as the call is told.
    let written = unsafe {
        GetRawInputDeviceInfoW(
            device,
            RIDI_DEVICENAME,
            name.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    let name = name.get(..usize::try_from(written).ok()?)?;
    let name = name.split(|unit| *unit == 0).next()?;
    // `\\?\HID#...&COL01#2&...&0000#{class}`: the instance between the
    // prefix and the interface's class, a `#` for each `\`.
    let path = name.get(4..)?;
    let end = path.iter().rposition(|unit| *unit == u16::from(b'#'))?;
    let mut instance = [0u16; 512];
    if end >= instance.len() {
        return None;
    }
    for (slot, unit) in instance.iter_mut().zip(path.get(..end)?) {
        *slot = if *unit == u16::from(b'#') {
            u16::from(b'\\')
        } else {
            *unit
        };
    }
    let mut node = 0u32;
    // SAFETY: a terminated identifier -- `instance` is longer than any path
    // copied into it -- and a live output.
    let cr =
        unsafe { CM_Locate_DevNodeW(&raw mut node, instance.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) };
    if cr != CR_SUCCESS {
        return None;
    }
    let mut container = [0u16; 40];
    let mut size = u32::try_from(size_of_val(&container)).ok()?;
    // SAFETY: a buffer of `size` bytes; no type asked for.
    let cr = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            node,
            CM_DRP_BASE_CONTAINERID,
            core::ptr::null_mut(),
            container.as_mut_ptr().cast(),
            &raw mut size,
            0,
        )
    };
    if cr != CR_SUCCESS {
        return None;
    }
    let mut out = [0u16; 38];
    out.copy_from_slice(container.get(..38)?);
    Some(out)
}

#[cfg(test)]
mod tests {
    //! Hardware checks, run from an elevated shell on the console with the
    //! input driver installed (scripts/hid-install.ps1):
    //!
    //!   cargo test -p lowlat-inject --lib hid -- --ignored --test-threads=1

    use super::*;
    use lowlat_hid_proto::{keyboard, mouse, pad};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_DRP_BASE_CONTAINERID, CM_DRP_HARDWAREID, CM_Get_DevNode_Registry_PropertyW,
        CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_BUSY};
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, CreateWellKnownSid, ImpersonateLoggedOnUser, RevertToSelf,
        SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_IMPERSONATE, TOKEN_QUERY, WinBuiltinAdministratorsSid,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICENAME,
        RIM_TYPEHID, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// The device node of a present device instance.
    fn node(instance: &str) -> Option<u32> {
        let id = wide(instance);
        let mut node = 0u32;
        // SAFETY: a terminated identifier and a live output.
        let cr =
            unsafe { CM_Locate_DevNodeW(&raw mut node, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) };
        (cr == CR_SUCCESS).then_some(node)
    }

    /// A node's text property, each string of a list, upper-cased.
    fn strings(node: u32, property: u32) -> Vec<String> {
        let mut buffer = [0u16; 1024];
        let mut len = u32::try_from(size_of_val(&buffer)).unwrap();
        // SAFETY: a buffer of `len` bytes; no type asked for.
        let cr = unsafe {
            CM_Get_DevNode_Registry_PropertyW(
                node,
                property,
                core::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &raw mut len,
                0,
            )
        };
        if cr != CR_SUCCESS {
            return Vec::new();
        }
        let units = buffer.get(..len as usize / 2).unwrap_or(&buffer);
        units
            .split(|unit| *unit == 0)
            .filter(|id| !id.is_empty())
            .map(|id| String::from_utf16_lossy(id).to_uppercase())
            .collect()
    }

    /// The container of `kind`'s device in `slot`, as the system writes it,
    /// upper-cased.
    fn container(kind: Kind, slot: u8) -> String {
        let id = lowlat_hid_proto::container_id(kind, slot);
        let d = id.data4;
        format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            id.data1, id.data2, id.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }

    /// The devices raw input lists with `kind`'s identity: each one's type and
    /// container. Raw input names a device of the virtual HID framework by the
    /// framework's own identifier (`HID_DEVICE_SYSTEM_VHF`) and an instance
    /// the framework chose: the kind's vendor and product are among its
    /// hardware identifiers alone, and its container names the kind and the
    /// slot.
    fn listed(kind: Kind) -> Vec<(u32, String)> {
        let identity = kind.identity();
        let wanted = format!(
            "HID\\VID_{:04X}&PID_{:04X}",
            identity.vendor, identity.product
        );
        let mut count = 0u32;
        let size = u32::try_from(size_of::<RAWINPUTDEVICELIST>()).unwrap();
        // SAFETY: a count query.
        unsafe { GetRawInputDeviceList(core::ptr::null_mut(), &raw mut count, size) };
        let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
        // SAFETY: a buffer of the count asked for.
        let got = unsafe { GetRawInputDeviceList(list.as_mut_ptr(), &raw mut count, size) };
        list.truncate(usize::try_from(got).unwrap_or(0));
        let mut found = Vec::new();
        for device in list {
            let mut name = [0u16; 512];
            let mut len = 512u32;
            // SAFETY: a buffer of `len` characters.
            let written = unsafe {
                GetRawInputDeviceInfoW(
                    device.hDevice,
                    RIDI_DEVICENAME,
                    name.as_mut_ptr().cast(),
                    &raw mut len,
                )
            };
            if written == u32::MAX {
                continue;
            }
            // `\\?\HID#...&COL01#2&...&0000#{class}`: the instance between the
            // prefix and the interface's class, a `#` for each `\`.
            let name = String::from_utf16_lossy(&name[..written as usize]).to_uppercase();
            let Some((path, _class)) = name.trim_start_matches(r"\\?\").rsplit_once('#') else {
                continue;
            };
            let Some(node) = node(&path.replace('#', "\\")) else {
                continue;
            };
            let ours = strings(node, CM_DRP_HARDWAREID).iter().any(|id| {
                id.strip_prefix(&wanted)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('&'))
            });
            if ours {
                let container = strings(node, CM_DRP_BASE_CONTAINERID).into_iter().next();
                found.push((device.dwType, container.unwrap_or_default()));
            }
        }
        found
    }

    /// The types listed for `kind`'s device in `slot`: those in the kind's
    /// and slot's container.
    fn ours(kind: Kind, slot: u8) -> Vec<u32> {
        let container = container(kind, slot);
        listed(kind)
            .into_iter()
            .filter(|(_, listed)| *listed == container)
            .map(|(kind, _)| kind)
            .collect()
    }

    fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> Option<Duration> {
        let started = Instant::now();
        while started.elapsed() < limit {
            if done() {
                return Some(started.elapsed());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// **Each kind arrives as the system's own device in its kind's and slot's
    /// container, and goes with its handle**: a keyboard and a mouse the
    /// system's own drivers claim, a pad the game controller class does,
    /// listed by raw input with the kind's identity and in the container that
    /// names the kind and the slot; gone again once the handle closes.
    #[test]
    #[ignore = "needs the input driver installed and an elevated shell"]
    fn each_kind_arrives_as_the_systems_own_device_and_goes_with_its_handle() {
        for (kind, slot, expected) in [
            (Kind::Keyboard, 15, RIM_TYPEKEYBOARD),
            (Kind::Mouse, 15, RIM_TYPEMOUSE),
            (Kind::DualShock4, 63, RIM_TYPEHID),
            (Kind::DualSense, 62, RIM_TYPEHID),
        ] {
            assert!(
                ours(kind, slot).is_empty(),
                "{kind:?} listed before it was made"
            );
            let device = Device::create(kind, slot, Features::new()).unwrap();
            let arrived = wait_until(Duration::from_secs(10), || {
                ours(kind, slot).contains(&expected)
            });
            eprintln!("{kind:?}: listed after {arrived:?}, as {:?}", listed(kind));
            assert!(
                arrived.is_some(),
                "{kind:?} never listed as type {expected} in container {}: {:?}",
                container(kind, slot),
                listed(kind)
            );
            drop(device);
            let gone = wait_until(Duration::from_secs(10), || ours(kind, slot).is_empty());
            eprintln!("{kind:?}: gone after {gone:?}");
            assert!(
                gone.is_some(),
                "{kind:?} still listed after its handle closed"
            );
        }
    }

    /// **A token that is not the system account's and holds administrators
    /// for deny alone is refused**: the interface admits the system account
    /// and administrators, an administrator only elevated.
    #[test]
    #[ignore = "needs the input driver installed and an elevated shell"]
    fn a_token_without_administrators_is_refused() {
        let mut token: HANDLE = core::ptr::null_mut();
        // SAFETY: the process's own token, with the access a restricted copy
        // and an impersonation need.
        let opened = unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ASSIGN_PRIMARY,
                &raw mut token,
            )
        };
        assert_ne!(opened, 0);
        let mut admins = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut size = SECURITY_MAX_SID_SIZE;
        // SAFETY: a buffer of the documented maximum size and its length.
        let made = unsafe {
            CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                core::ptr::null_mut(),
                admins.as_mut_ptr().cast(),
                &raw mut size,
            )
        };
        assert_ne!(made, 0);
        let disable = SID_AND_ATTRIBUTES {
            Sid: admins.as_mut_ptr().cast(),
            Attributes: 0,
        };
        let mut restricted: HANDLE = core::ptr::null_mut();
        // SAFETY: the token above and one group to hold for deny alone.
        let ok = unsafe {
            CreateRestrictedToken(
                token,
                0,
                1,
                &raw const disable,
                0,
                core::ptr::null(),
                0,
                core::ptr::null(),
                &raw mut restricted,
            )
        };
        assert_ne!(ok, 0);
        // SAFETY: the restricted token, on this thread until reverted.
        assert_ne!(unsafe { ImpersonateLoggedOnUser(restricted) }, 0);
        let refused = Device::create(Kind::Keyboard, 13, Features::new());
        // SAFETY: back to the process's own token.
        unsafe { RevertToSelf() };
        // SAFETY: handles this test opened, closed once.
        unsafe {
            CloseHandle(restricted);
            CloseHandle(token);
        }
        assert_eq!(refused.unwrap_err(), Error::Denied);
        assert!(
            Device::create(Kind::Keyboard, 13, Features::new()).is_ok(),
            "the elevated token itself is admitted"
        );
    }

    /// **A report is taken whole and only as its kind declares it**: the
    /// power key never, another kind's report never, a report cut short never.
    #[test]
    #[ignore = "needs the input driver installed and an elevated shell"]
    fn a_report_is_taken_whole_and_only_as_its_kind_declares_it() {
        let mut keys = Device::create(Kind::Keyboard, 12, Features::new()).unwrap();
        keys.write(&keyboard::NEUTRAL_KEYS).unwrap();
        let mut power = keyboard::NEUTRAL_KEYS;
        power[2 + usize::from(keyboard::POWER / 8)] |= 1 << (keyboard::POWER % 8);
        assert_eq!(
            keys.write(&power),
            Err(Error::Refused(ERROR_INVALID_PARAMETER))
        );
        assert!(keys.write(&mouse::NEUTRAL).is_err(), "a mouse's report");
        assert!(
            keys.write(&keyboard::NEUTRAL_KEYS[..29]).is_err(),
            "cut short"
        );
        let mut pad = Device::create(Kind::DualSense, 61, Features::new()).unwrap();
        pad.write(pad::DS5_NEUTRAL).unwrap();
        assert_eq!(pad.written(), Ok(None), "nothing written to the pad yet");
    }

    /// A child process, killed when dropped: a check failing while the child
    /// holds its device leaves neither the device nor its key behind.
    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The child of the next test: a keyboard holding F24 until it is killed.
    #[test]
    #[ignore = "run by a_killed_host_leaves_no_key_held"]
    fn a_child_holding_a_key() {
        if std::env::var_os("LOWLAT_HID_CHILD").is_none() {
            return;
        }
        let mut keys = Device::create(Kind::Keyboard, 11, Features::new()).unwrap();
        let mut held = keyboard::Keys::new();
        held.set(0x73, true);
        loop {
            let _ = keys.write(held.report());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// **A killed host leaves no key held**: a process holding F24 on the
    /// driver's keyboard is killed, and the key reads up once its device goes.
    #[test]
    #[ignore = "needs the input driver installed and an elevated shell on the console"]
    fn a_killed_host_leaves_no_key_held() {
        const VK_F24: i32 = 0x87;
        let down = || {
            // SAFETY: no precondition.
            (unsafe { GetAsyncKeyState(VK_F24) } as u16) & 0x8000 != 0
        };
        assert!(!down(), "F24 down before the test");
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "hid::tests::a_child_holding_a_key"])
                .env("LOWLAT_HID_CHILD", "1")
                .spawn()
                .unwrap(),
        );
        let held = wait_until(Duration::from_secs(10), down);
        eprintln!("F24 down after {held:?}");
        assert!(held.is_some(), "the child's key never went down");
        assert!(
            ours(Kind::Keyboard, 11).contains(&RIM_TYPEKEYBOARD),
            "the child's keyboard listed while it holds the key"
        );
        child.0.kill().unwrap();
        let _ = child.0.wait();
        let released = wait_until(Duration::from_secs(10), || !down());
        eprintln!("F24 up after {released:?} from the kill");
        assert!(
            released.is_some(),
            "F24 still down after its host was killed"
        );
        let gone = wait_until(Duration::from_secs(10), || {
            ours(Kind::Keyboard, 11).is_empty()
        });
        assert!(gone.is_some(), "the killed host's keyboard still listed");
    }

    /// **A kind and slot is one device at a time**: a second handle asking
    /// for the kind and slot another's device holds is refused as busy, and
    /// given it once that device goes.
    #[test]
    #[ignore = "needs the input driver installed and an elevated shell"]
    fn a_kind_and_slot_is_one_device_at_a_time() {
        let first = Device::create(Kind::Keyboard, 10, Features::new()).unwrap();
        assert_eq!(
            Device::create(Kind::Keyboard, 10, Features::new()).unwrap_err(),
            Error::Refused(ERROR_BUSY)
        );
        let other = Device::create(Kind::Mouse, 10, Features::new());
        assert!(other.is_ok(), "another kind in the slot: {other:?}");
        drop(first);
        let again = Device::create(Kind::Keyboard, 10, Features::new());
        assert!(
            again.is_ok(),
            "the slot again once its device went: {again:?}"
        );
    }

    /// **A handle lets go when the system asks for the driver's device back**:
    /// a keyboard held and written to while the driver's device is restarted
    /// -- the run sheet restarts it once this says it holds one -- answers
    /// absent once the system asks, and the restart is not refused for it.
    #[test]
    #[ignore = "run by the run sheet, which restarts the input driver's device beside it"]
    fn a_held_keyboard_lets_go_when_the_driver_is_asked_back() {
        let mut keys = Device::create(Kind::Keyboard, 9, Features::new()).unwrap();
        let started = Instant::now();
        eprintln!("holding a keyboard in slot 9");
        loop {
            match keys.write(&keyboard::NEUTRAL_KEYS) {
                Ok(()) => {}
                Err(error) => {
                    eprintln!("let go after {:?}: {error:?}", started.elapsed());
                    assert_eq!(error, Error::Absent);
                    assert_eq!(keys.write(&keyboard::NEUTRAL_KEYS), Err(Error::Absent));
                    return;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "never asked to let go"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
