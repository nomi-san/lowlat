//! A monitor's frame page, from the reading side
//! ([docs/11-drivers.md](../../../docs/11-drivers.md) section 1.6): asked of
//! the driver, taken out of its process and mapped read-only.
//!
//! **Only a process allowed to take handles from the driver's can use the
//! answer** -- the system account (section 1.2) -- so taking the page is
//! where any other reader is told no. The driver's process is held, as the
//! process that answered by its identifier and start time, for every handle
//! taken after it: each buffer and the fence, as the page names them.

use core::sync::atomic::AtomicU64;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use lowlat_vdd_proto::frames::{self, Pointer, SECTION_WORDS, SIZE, Snapshot};
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED, FILETIME, GetLastError, HANDLE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Memory::{
    FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_LIMITED_INFORMATION, WaitForMultipleObjects,
};

use crate::Error;
use crate::device::Device;

/// What a wait for the next picture ended on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// The driver published a picture.
    Published,
    /// Nothing within the time.
    TimedOut,
    /// The driver's process ended: the page will not change again.
    Ended,
}

/// A view of a section, by its address, unmapped when dropped.
#[derive(Debug)]
struct View(*mut core::ffi::c_void);

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: a view this mapped, unmapped once; nothing reads it after.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.0 }) };
    }
}

impl View {
    fn words(&self) -> &[AtomicU64] {
        // SAFETY: a view of the whole section, page-aligned, mapped for this
        // view's life and read through atomics alone.
        unsafe { core::slice::from_raw_parts(self.0 as *const AtomicU64, SECTION_WORDS) }
    }
}

/// A monitor's frame page, mapped read-only.
#[derive(Debug)]
pub struct FramePage {
    /// The driver's process, which every handle is taken from.
    process: OwnedHandle,
    view: View,
    _section: OwnedHandle,
    event: OwnedHandle,
}

// SAFETY: the handles are kernel objects any thread may use; the view is
// read through atomics alone.
unsafe impl Send for FramePage {}

/// The system's last error, as the controller's error.
fn last_error() -> Error {
    // SAFETY: no precondition.
    match unsafe { GetLastError() } {
        ERROR_ACCESS_DENIED => Error::Denied,
        code => Error::Io(code),
    }
}

/// When `process` started, in file-time units.
fn started(process: HANDLE) -> Option<u64> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: a live process handle with the right to ask; live outputs.
    let ok = unsafe {
        GetProcessTimes(
            process,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    (ok != 0).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Take `value` out of `process` into this one: with `access`, or with the
/// access it has there when `access` is none.
fn take(process: HANDLE, value: u64, access: u32) -> Result<OwnedHandle, Error> {
    if value == 0 {
        return Err(Error::Argument);
    }
    let mut taken: HANDLE = core::ptr::null_mut();
    let options = if access == 0 {
        DUPLICATE_SAME_ACCESS
    } else {
        0
    };
    // SAFETY: a live process handle with the right to take handles from it;
    // the value is checked by the system; a live output.
    let ok = unsafe {
        DuplicateHandle(
            process,
            value as HANDLE,
            GetCurrentProcess(),
            &raw mut taken,
            access,
            0,
            options,
        )
    };
    if ok == 0 || taken.is_null() {
        return Err(last_error());
    }
    // SAFETY: a handle this process now owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(taken) })
}

impl FramePage {
    /// Ask the driver for slot `slot`'s page and take it.
    pub fn open(slot: u8) -> Result<Self, Error> {
        let answer = Device::open()?.frames(slot)?;
        if answer.version != frames::VERSION || usize::try_from(answer.size).ok() != Some(SIZE) {
            return Err(Error::Version(answer.version));
        }
        // SAFETY: no handle passed in; the identifier is checked by the
        // system.
        let raw = unsafe {
            OpenProcess(
                PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                answer.process,
            )
        };
        if raw.is_null() {
            return Err(last_error());
        }
        // SAFETY: a handle this process now owns.
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        // **The process that answered, not one that came to carry its
        // identifier since**: handles taken from another would be anything.
        if started(process.as_raw_handle()) != Some(answer.started) {
            return Err(Error::Absent);
        }
        let section = take(process.as_raw_handle(), answer.section, FILE_MAP_READ)?;
        // SAFETY: a section handle with the right to map it for reading.
        let mapped = unsafe { MapViewOfFile(section.as_raw_handle(), FILE_MAP_READ, 0, 0, SIZE) };
        if mapped.Value.is_null() {
            return Err(last_error());
        }
        let view = View(mapped.Value);
        let snapshot = frames::read(view.words()).ok_or(Error::Version(0))?;
        let event = take(process.as_raw_handle(), snapshot.event, SYNCHRONIZE)?;
        Ok(Self {
            process,
            view,
            _section: section,
            event,
        })
    }

    /// The page as it stands, or none when the driver is mid-write beyond the
    /// reader's patience or wrote what cannot be.
    pub fn read(&self) -> Option<Snapshot> {
        frames::read(self.view.words())
    }

    /// The pointer as the driver last said it, or none before it has said
    /// anything (docs/11-drivers.md section 1.7).
    pub fn pointer(&self) -> Option<Pointer> {
        frames::read_pointer(self.view.words())
    }

    /// The pixels of the shape numbered `shape` into `into`: how many bytes,
    /// or none when the page holds another shape's by now.
    pub fn shape(&self, shape: u32, into: &mut [u8]) -> Option<usize> {
        frames::read_shape(self.view.words(), shape, into)
    }

    /// A handle the page names -- a buffer or the fence -- taken into this
    /// process with the access it has in the driver's.
    pub fn pull(&self, value: u64) -> Result<OwnedHandle, Error> {
        take(self.process.as_raw_handle(), value, 0)
    }

    /// The driver's process, by its identifier: where its handles are taken
    /// from.
    pub fn process_id(&self) -> u32 {
        // SAFETY: a live process handle.
        unsafe { windows_sys::Win32::System::Threading::GetProcessId(self.process.as_raw_handle()) }
    }

    /// Wait up to `timeout_ms` for the next picture.
    pub fn wait(&self, timeout_ms: u32) -> Waited {
        let handles = [self.event.as_raw_handle(), self.process.as_raw_handle()];
        // SAFETY: two live handles, both waitable.
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, timeout_ms) } {
            WAIT_OBJECT_0 => Waited::Published,
            WAIT_TIMEOUT => Waited::TimedOut,
            _ => Waited::Ended,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A reader not allowed to take handles from the driver's process gets
    /// nothing it can use**: asked by an ordinary user the driver answers --
    /// any caller the device admits may ask -- and taking the page out of
    /// its process is refused. Run as an unelevated user, the driver
    /// installed.
    #[test]
    #[ignore = "requires the display driver and an unelevated user"]
    fn a_reader_that_cannot_take_handles_gets_nothing() {
        let answer = Device::open()
            .expect("the driver")
            .frames(0)
            .expect("an answer");
        assert_eq!(answer.version, frames::VERSION);
        assert_ne!(answer.section, 0, "the answer names no section");
        assert_eq!(FramePage::open(0).err(), Some(Error::Denied));
    }
}
