//! The graphics interface's side of the runtime: a compute device found by
//! the graphics adapter it is, so that a decoder's device and the textures
//! its pictures are handed out in share one GPU, and those textures written
//! from a stream. This half of the runtime is written per platform, as the
//! descriptor half is.
//!
//! **A texture is registered once and mapped for each write.** The map and
//! the unmap are queued on the stream the writes run on, and neither waits
//! on the calling thread for the work queued before it (measured: ~60 and
//! ~70 us each, behind half a millisecond of queued copies). Graphics work
//! the texture's own device issues after the unmap runs after the writes; a
//! fence that device signals then says, to anyone, when they are done. No
//! other device's order is promised by the unmap itself.

use core::ffi::{c_char, c_uint, c_void};

use lowlat_common::dynlib::Library;

use super::{Context, CtxPopCurrent, CtxPushCurrent, Cuda, Device, Error, Result, Stream, check};
use crate::ffi::cuda::{CUarray, CUcontext, CUdevice, CUgraphicsResource, CUresult, CUstream};

type DeviceGetLuid = unsafe extern "C" fn(*mut c_char, *mut c_uint, CUdevice) -> CUresult;
type RegisterResource =
    unsafe extern "C" fn(*mut CUgraphicsResource, *mut c_void, c_uint) -> CUresult;
type UnregisterResource = unsafe extern "C" fn(CUgraphicsResource) -> CUresult;
type MapResources = unsafe extern "C" fn(c_uint, *mut CUgraphicsResource, CUstream) -> CUresult;
type MappedArray =
    unsafe extern "C" fn(*mut CUarray, CUgraphicsResource, c_uint, c_uint) -> CUresult;

/// The entry points only this platform has, each found or not: a driver
/// without one has no decoder that hands pictures to the graphics interface.
#[derive(Debug)]
pub(crate) struct Interop {
    device_get_luid: Option<DeviceGetLuid>,
    register: Option<RegisterResource>,
    unregister: Option<UnregisterResource>,
    map: Option<MapResources>,
    unmap: Option<MapResources>,
    mapped_array: Option<MappedArray>,
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
                register: library.symbol(c"cuGraphicsD3D11RegisterResource"),
                unregister: library.symbol(c"cuGraphicsUnregisterResource"),
                map: library.symbol(c"cuGraphicsMapResources"),
                unmap: library.symbol(c"cuGraphicsUnmapResources"),
                mapped_array: library.symbol(c"cuGraphicsSubResourceGetMappedArray"),
            }
        }
    }
}

/// A texture of the graphics interface, registered with the runtime to be
/// written from a stream while mapped. **Unregistered when dropped, on any
/// thread**: its context is made current for the call and the thread's
/// own put back after.
#[derive(Debug)]
pub struct Registered {
    raw: CUgraphicsResource,
    context: CUcontext,
    unregister: UnregisterResource,
    push_current: CtxPushCurrent,
    pop_current: CtxPopCurrent,
}

// SAFETY: a registration belongs to its context, not to a thread.
unsafe impl Send for Registered {}
// SAFETY: as above; nothing is reached through `&self` but the handle.
unsafe impl Sync for Registered {}

impl Drop for Registered {
    fn drop(&mut self) {
        // SAFETY: the context outlives the registration, by the contract of
        // `register_texture`; registered once and unregistered once, the type
        // being neither `Copy` nor `Clone`. A context that cannot be made
        // current is one that is gone, and its registrations with it.
        unsafe {
            if check((self.push_current)(self.context)).is_ok() {
                (self.unregister)(self.raw);
                let mut popped: CUcontext = core::ptr::null_mut();
                (self.pop_current)(&raw mut popped);
            }
        }
    }
}

/// Up to three textures mapped on a stream, each as the array a copy
/// writes; unmapped by [`Cuda::unmap`].
#[derive(Debug)]
pub struct Mapped {
    resources: [CUgraphicsResource; 3],
    count: usize,
    pub arrays: [CUarray; 3],
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

    /// Whether this runtime writes the graphics interface's textures.
    pub fn has_graphics(&self) -> bool {
        let i = &self.interop;
        i.register.is_some()
            && i.unregister.is_some()
            && i.map.is_some()
            && i.unmap.is_some()
            && i.mapped_array.is_some()
    }

    /// Register a texture in `context` to be written from a stream: once,
    /// for its life. The context is made current for the call, on whichever
    /// thread, and the thread's own put back after.
    ///
    /// # Safety
    ///
    /// `texture` is a live texture resource of the graphics interface on the
    /// adapter `context`'s device is. The texture, `context` and this
    /// runtime all outlive the registration.
    pub unsafe fn register_texture(
        &self,
        context: &Context,
        texture: *mut c_void,
    ) -> Result<Registered> {
        let i = &self.interop;
        let (Some(register), Some(unregister)) = (i.register, i.unregister) else {
            return Err(Error::MissingSymbol);
        };
        context.make_current()?;
        let mut raw: CUgraphicsResource = core::ptr::null_mut();
        // SAFETY: the caller's contract for the texture; the output is live.
        let registered = check(unsafe { register(&raw mut raw, texture, 0) });
        let _ = context.release_current();
        registered?;
        Ok(Registered {
            raw,
            context: context.raw,
            unregister,
            push_current: context.push_current,
            pop_current: context.pop_current,
        })
    }

    /// Map `textures` on `stream`: each as the array a copy writes, until
    /// [`Self::unmap`]. Queued; the calling thread waits for nothing.
    pub fn map(&self, textures: &[&Registered], stream: &Stream) -> Result<Mapped> {
        let i = &self.interop;
        let (Some(map), Some(unmap), Some(array_of)) = (i.map, i.unmap, i.mapped_array) else {
            return Err(Error::MissingSymbol);
        };
        let mut mapped = Mapped {
            resources: [core::ptr::null_mut(); 3],
            count: textures.len().min(3),
            arrays: [core::ptr::null_mut(); 3],
        };
        for (slot, texture) in mapped.resources.iter_mut().zip(textures) {
            *slot = texture.raw;
        }
        let count = c_uint::try_from(mapped.count).unwrap_or(0);
        // SAFETY: the handles are live registrations; the stream belongs to
        // the current context.
        check(unsafe { map(count, mapped.resources.as_mut_ptr(), stream.raw) })?;
        for (array, resource) in mapped
            .arrays
            .iter_mut()
            .zip(mapped.resources)
            .take(mapped.count)
        {
            // SAFETY: a mapped registration; the output is live.
            let status = unsafe { array_of(array, resource, 0, 0) };
            if let Err(e) = check(status) {
                // SAFETY: mapped just above, unmapped once.
                unsafe { unmap(count, mapped.resources.as_mut_ptr(), stream.raw) };
                return Err(e);
            }
        }
        Ok(mapped)
    }

    /// Unmap what [`Self::map`] mapped, on `stream`: the writes queued before
    /// this are done before graphics work the textures' device issues after
    /// it begins. Queued; the calling thread waits for nothing.
    pub fn unmap(&self, mut mapped: Mapped, stream: &Stream) -> Result<()> {
        let unmap = self.interop.unmap.ok_or(Error::MissingSymbol)?;
        let count = c_uint::try_from(mapped.count).unwrap_or(0);
        // SAFETY: the registrations `map` mapped on this stream.
        check(unsafe { unmap(count, mapped.resources.as_mut_ptr(), stream.raw) })
    }
}
