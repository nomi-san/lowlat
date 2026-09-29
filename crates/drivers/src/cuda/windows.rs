//! The graphics interface's side of the runtime: a compute device found by
//! the graphics adapter it is, so that a decoder's device and the textures
//! its pictures are handed out in share one GPU. This half of the runtime is
//! written per platform, as the descriptor half is.

use core::ffi::{c_char, c_uint};

use lowlat_common::dynlib::Library;

use super::{Cuda, Device, Error, Result, check};
use crate::ffi::cuda::{CUdevice, CUresult};

type DeviceGetLuid = unsafe extern "C" fn(*mut c_char, *mut c_uint, CUdevice) -> CUresult;

/// The entry points only this platform has, each found or not: a driver
/// without one has no decoder that hands pictures to the graphics interface.
#[derive(Debug)]
pub(crate) struct Interop {
    device_get_luid: Option<DeviceGetLuid>,
}

impl Interop {
    /// # Safety
    ///
    /// `library` is the compute runtime, whose exports have the signatures
    /// transcribed here from the vendored header.
    pub(crate) unsafe fn load(library: &Library) -> Self {
        // SAFETY: the caller's contract.
        unsafe {
            Self {
                device_get_luid: library.symbol(c"cuDeviceGetLuid"),
            }
        }
    }
}

impl Cuda {
    /// The graphics adapter `device` is, as the system identifies it for the
    /// boot: its locally unique identifier as one value, low part first.
    pub fn luid(&self, device: &Device) -> Result<u64> {
        let get = self.interop.device_get_luid.ok_or(Error::MissingSymbol)?;
        let mut bytes = [0u8; 8];
        let mut mask: c_uint = 0;
        // SAFETY: the buffer holds the eight bytes the call writes; the mask
        // is a live local.
        check(unsafe { get(bytes.as_mut_ptr().cast(), &raw mut mask, device.handle) })?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// The compute device that is the graphics adapter `luid`, or an error:
    /// never another, since a decoder on another GPU than its textures
    /// hands out nothing the textures' device can open.
    pub fn device_for_luid(&self, luid: u64) -> Result<Device> {
        for ordinal in 0..self.device_count()? {
            let device = self.device(ordinal)?;
            if self.luid(&device)? == luid {
                return Ok(device);
            }
        }
        Err(Error::NoSuchAdapter(luid))
    }
}
