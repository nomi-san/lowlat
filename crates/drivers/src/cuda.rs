//! The compute runtime the encoder opens its session against.
//!
//! On this platform the encoder takes a compute device rather than a graphics
//! one, so a context has to exist before a session can. Loaded at runtime for
//! the same reason as the encoder itself (docs/07-platforms.md section 8).
//!
//! **Device selection is by address, and a miss is an error.** A machine with
//! more than one GPU has more than one compute device, and the frame source
//! lives on exactly one of them: the one driving the display. Encoding on the
//! other means moving every frame across the bus, which is a readback by
//! another name, and docs/05-host.md section 4 requires that to be chosen
//! rather than discovered. So the caller names the device it needs and this
//! module refuses rather than substituting.
//!
//! The address is discovered at construction and never stored. Enumeration
//! order is not stable across driver reloads, and neither is the display's
//! attachment.

use core::ffi::{CStr, c_char, c_int, c_uint, c_ulonglong, c_void};

use lowlat_common::dynlib::Library;

use crate::ffi::cuda::{
    CU_EVENT_BLOCKING_SYNC, CU_EVENT_DISABLE_TIMING, CUDA_ERROR_NOT_READY, CUDA_SUCCESS, CUcontext,
    CUdevice, CUdeviceptr, CUevent, CUresult, CUstream,
};

/// Memory that crosses to another device or process as a descriptor, which
/// is the platform's own kind of handle.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{Exportable, External, Plane};

/// Versioned first, as with the encoder runtime.
#[cfg(unix)]
const SONAMES: &[&CStr] = &[c"libcuda.so.1", c"libcuda.so"];
/// Installed with the display driver, in the system directory.
#[cfg(windows)]
const SONAMES: &[&CStr] = &[c"nvcuda.dll"];

type Init = unsafe extern "C" fn(c_uint) -> CUresult;
type DeviceGetCount = unsafe extern "C" fn(*mut c_int) -> CUresult;
type DeviceGet = unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult;
type DeviceGetName = unsafe extern "C" fn(*mut c_char, c_int, CUdevice) -> CUresult;
type DeviceGetPciBusId = unsafe extern "C" fn(*mut c_char, c_int, CUdevice) -> CUresult;
type MemAllocPitch =
    unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, c_uint) -> CUresult;
type MemFree = unsafe extern "C" fn(CUdeviceptr) -> CUresult;
type MemsetD8 = unsafe extern "C" fn(CUdeviceptr, u8, usize) -> CUresult;
type Memcpy2D = unsafe extern "C" fn(*const crate::ffi::cuda::CUDA_MEMCPY2D) -> CUresult;
type PrimaryCtxRetain = unsafe extern "C" fn(*mut CUcontext, CUdevice) -> CUresult;
type PrimaryCtxRelease = unsafe extern "C" fn(CUdevice) -> CUresult;
type CtxPushCurrent = unsafe extern "C" fn(CUcontext) -> CUresult;
type CtxPopCurrent = unsafe extern "C" fn(*mut CUcontext) -> CUresult;
type StreamCreate = unsafe extern "C" fn(*mut CUstream, c_uint) -> CUresult;
type StreamDestroy = unsafe extern "C" fn(CUstream) -> CUresult;
type EventCreate = unsafe extern "C" fn(*mut CUevent, c_uint) -> CUresult;
type EventDestroy = unsafe extern "C" fn(CUevent) -> CUresult;
type EventRecord = unsafe extern "C" fn(CUevent, CUstream) -> CUresult;
type EventQuery = unsafe extern "C" fn(CUevent) -> CUresult;
type EventSynchronize = unsafe extern "C" fn(CUevent) -> CUresult;
type ImportExternalMemory = unsafe extern "C" fn(
    *mut crate::ffi::cuda::CUexternalMemory,
    *const crate::ffi::cuda::CUDA_EXTERNAL_MEMORY_HANDLE_DESC,
) -> CUresult;
type ExternalMemoryGetMappedBuffer = unsafe extern "C" fn(
    *mut CUdeviceptr,
    crate::ffi::cuda::CUexternalMemory,
    *const crate::ffi::cuda::CUDA_EXTERNAL_MEMORY_BUFFER_DESC,
) -> CUresult;
type DestroyExternalMemory = unsafe extern "C" fn(crate::ffi::cuda::CUexternalMemory) -> CUresult;
type Memcpy2DAsync =
    unsafe extern "C" fn(*const crate::ffi::cuda::CUDA_MEMCPY2D, CUstream) -> CUresult;
type MemGetAllocationGranularity =
    unsafe extern "C" fn(*mut usize, *const MemAllocationProp, c_uint) -> CUresult;
type MemCreate = unsafe extern "C" fn(
    *mut MemGenericAllocationHandle,
    usize,
    *const MemAllocationProp,
    c_ulonglong,
) -> CUresult;
type MemRelease = unsafe extern "C" fn(MemGenericAllocationHandle) -> CUresult;
type MemAddressReserve =
    unsafe extern "C" fn(*mut CUdeviceptr, usize, usize, CUdeviceptr, c_ulonglong) -> CUresult;
type MemAddressFree = unsafe extern "C" fn(CUdeviceptr, usize) -> CUresult;
type MemMap = unsafe extern "C" fn(
    CUdeviceptr,
    usize,
    usize,
    MemGenericAllocationHandle,
    c_ulonglong,
) -> CUresult;
type MemUnmap = unsafe extern "C" fn(CUdeviceptr, usize) -> CUresult;
type MemSetAccess =
    unsafe extern "C" fn(CUdeviceptr, usize, *const MemAccessDesc, usize) -> CUresult;
type MemExportToShareableHandle =
    unsafe extern "C" fn(*mut c_void, MemGenericAllocationHandle, c_uint, c_ulonglong) -> CUresult;

/// The virtual-memory interface's descriptors, declared here because the
/// vendored header predates that interface. The layouts are the ones its
/// own header documents, and the sizes are asserted below so a slip is a
/// build failure rather than a fault in the driver.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
type MemGenericAllocationHandle = c_ulonglong;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[repr(C)]
#[derive(Clone, Copy)]
struct MemLocation {
    kind: c_uint,
    id: c_int,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[repr(C)]
#[derive(Clone, Copy)]
struct MemAllocationProp {
    kind: c_uint,
    requested_handle_types: c_uint,
    location: MemLocation,
    win32_handle_meta_data: *mut c_void,
    /// The compression type, the direct-access flag, the usage and four
    /// reserved bytes: none of them asked for, all of them zero.
    alloc_flags: [u8; 8],
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[repr(C)]
#[derive(Clone, Copy)]
struct MemAccessDesc {
    location: MemLocation,
    flags: c_uint,
}

const _: () = assert!(core::mem::size_of::<MemAllocationProp>() == 32);
const _: () = assert!(core::mem::align_of::<MemAllocationProp>() == 8);
const _: () = assert!(core::mem::size_of::<MemAccessDesc>() == 12);

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const MEM_ALLOCATION_TYPE_PINNED: c_uint = 1;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const MEM_LOCATION_TYPE_DEVICE: c_uint = 1;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const MEM_ACCESS_FLAGS_PROT_READWRITE: c_uint = 3;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const MEM_ALLOC_GRANULARITY_MINIMUM: c_uint = 0;

/// Why the runtime could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No such library, which is the ordinary case without the hardware.
    Unavailable,
    /// Loaded, but missing an entry point it must export.
    MissingSymbol,
    /// A call failed. The code is carried rather than a message, because a
    /// message would mean either allocating or holding a driver pointer.
    Status(CUresult),
    /// No device at the requested address. **Never substituted**: encoding on
    /// a device the frames do not live on is a silent per-frame copy.
    NoSuchDevice(PciAddress),
    /// The runtime is present but reports no devices at all.
    NoDevices,
    /// A copy was asked for more rows than the source holds. Ours to get
    /// right, so it is refused rather than clamped: a clamp would upload a
    /// partial picture and the fault would show as torn output rather than
    /// as an error.
    SourceTooSmall,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("compute runtime not present"),
            Self::MissingSymbol => f.write_str("compute runtime is missing an entry point"),
            Self::Status(status) => write!(f, "compute runtime returned status {status}"),
            Self::NoSuchDevice(address) => {
                write!(f, "no compute device at {address}")
            }
            Self::NoDevices => f.write_str("compute runtime reports no devices"),
            Self::SourceTooSmall => f.write_str("source holds fewer rows than the copy needs"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

fn check(status: CUresult) -> Result<()> {
    if status == CUDA_SUCCESS {
        Ok(())
    } else {
        Err(Error::Status(status))
    }
}

/// A device's bus address, as both the compute runtime and the display stack
/// render it: `0000:01:00.0`.
///
/// Fixed storage and no allocation, so it can be compared on any path. Held as
/// written by the runtime and compared case-insensitively, because the two
/// sources that produce it are not required to agree on case.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PciAddress {
    text: [u8; Self::CAPACITY],
    len: usize,
}

impl PciAddress {
    const CAPACITY: usize = 20;

    /// Parse from text, lowercasing as it goes. Returns `None` if it is longer
    /// than any real address.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.trim().as_bytes();
        if bytes.is_empty() || bytes.len() > Self::CAPACITY {
            return None;
        }
        let mut stored = [0u8; Self::CAPACITY];
        for (slot, byte) in stored.iter_mut().zip(bytes) {
            *slot = byte.to_ascii_lowercase();
        }
        Some(Self {
            text: stored,
            len: bytes.len(),
        })
    }

    pub fn as_str(&self) -> &str {
        // The bytes came from a `&str` or from an ASCII buffer the runtime
        // wrote, so this cannot fail; an empty string is a better answer here
        // than a panic on a diagnostic path.
        core::str::from_utf8(&self.text[..self.len]).unwrap_or("")
    }
}

impl core::fmt::Display for PciAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::fmt::Debug for PciAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PciAddress({})", self.as_str())
    }
}

/// One compute device.
#[derive(Debug, Clone, Copy)]
pub struct Device {
    handle: CUdevice,
    address: PciAddress,
}

impl Device {
    pub fn address(&self) -> PciAddress {
        self.address
    }
}

/// A retained primary context on one device.
///
/// The primary context is shared with everything else in the process that
/// touches the same device, which is what the encoder and any future import
/// path both want; creating a private one instead would put our buffers in a
/// context nothing else can reach.
#[derive(Debug)]
pub struct Context {
    raw: CUcontext,
    device: CUdevice,
    release: PrimaryCtxRelease,
    push_current: CtxPushCurrent,
    pop_current: CtxPopCurrent,
}

// SAFETY: a context is usable from any thread, and this type only hands out
// the raw handle. Sending one is what lets the encoder be built where the
// pipeline is assembled and run on the encode thread.
unsafe impl Send for Context {}

impl Context {
    /// The raw handle, for the encoder session.
    pub fn raw(&self) -> CUcontext {
        self.raw
    }

    /// Make this context current on the calling thread.
    ///
    /// **Retaining a context does not make it current**, and every allocation
    /// and every encoder call is made against whatever is current on the
    /// calling thread. Without this they fail with an invalid-context status,
    /// which names neither the context nor the thread and is the least
    /// informative way this can go wrong.
    ///
    /// Pushed rather than assigned, because the interface offers no assign.
    /// Not popped by the thread that drives a session, which drives it for
    /// the session's life; a probe on a borrowed thread pops with
    /// [`Self::release_current`].
    pub fn make_current(&self) -> Result<()> {
        // SAFETY: the handle is valid for the life of `self`.
        check(unsafe { (self.push_current)(self.raw) })
    }

    /// Undo [`Self::make_current`] on the calling thread, for a probe that
    /// borrows an application's thread and leaves it as it found it.
    pub fn release_current(&self) -> Result<()> {
        let mut popped: CUcontext = core::ptr::null_mut();
        // SAFETY: the out pointer is to a live local; the context popped is
        // whatever this thread has on top, which is the one pushed.
        check(unsafe { (self.pop_current)(&raw mut popped) })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: balanced against the retain that produced it, once, because
        // this type is neither `Copy` nor `Clone`.
        unsafe { (self.release)(self.device) };
    }
}

/// The loaded compute runtime.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug)]
pub struct Cuda {
    device_get_count: DeviceGetCount,
    device_get: DeviceGet,
    device_get_name: DeviceGetName,
    device_get_pci_bus_id: DeviceGetPciBusId,
    primary_ctx_retain: PrimaryCtxRetain,
    primary_ctx_release: PrimaryCtxRelease,
    ctx_push_current: CtxPushCurrent,
    ctx_pop_current: CtxPopCurrent,
    mem_alloc_pitch: MemAllocPitch,
    mem_free: MemFree,
    memset_d8: MemsetD8,
    memcpy_2d: Memcpy2D,
    stream_create: StreamCreate,
    stream_destroy: StreamDestroy,
    event_create: EventCreate,
    event_destroy: EventDestroy,
    event_record: EventRecord,
    event_query: EventQuery,
    event_synchronize: EventSynchronize,
    import_external_memory: ImportExternalMemory,
    external_memory_get_mapped_buffer: ExternalMemoryGetMappedBuffer,
    destroy_external_memory: DestroyExternalMemory,
    memcpy_2d_async: Memcpy2DAsync,
    mem_get_allocation_granularity: MemGetAllocationGranularity,
    mem_create: MemCreate,
    mem_release: MemRelease,
    mem_address_reserve: MemAddressReserve,
    mem_address_free: MemAddressFree,
    mem_map: MemMap,
    mem_unmap: MemUnmap,
    mem_set_access: MemSetAccess,
    mem_export_to_shareable_handle: MemExportToShareableHandle,
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl Cuda {
    /// Open the runtime and initialise it.
    pub fn load() -> Result<Self> {
        let library = Library::open_first(SONAMES).ok_or(Error::Unavailable)?;

        // SAFETY: every signature is transcribed from the vendored header, and
        // the symbol names are the ones that header's own loader uses, which
        // matters because several of these carry a version suffix in the
        // library that the documented name does not show.
        let loaded = unsafe {
            let init: Init = library.symbol(c"cuInit").ok_or(Error::MissingSymbol)?;
            Self {
                device_get_count: library
                    .symbol(c"cuDeviceGetCount")
                    .ok_or(Error::MissingSymbol)?,
                device_get: library.symbol(c"cuDeviceGet").ok_or(Error::MissingSymbol)?,
                device_get_name: library
                    .symbol(c"cuDeviceGetName")
                    .ok_or(Error::MissingSymbol)?,
                device_get_pci_bus_id: library
                    .symbol(c"cuDeviceGetPCIBusId")
                    .ok_or(Error::MissingSymbol)?,
                primary_ctx_retain: library
                    .symbol(c"cuDevicePrimaryCtxRetain")
                    .ok_or(Error::MissingSymbol)?,
                primary_ctx_release: library
                    .symbol(c"cuDevicePrimaryCtxRelease")
                    .ok_or(Error::MissingSymbol)?,
                ctx_push_current: library
                    .symbol(c"cuCtxPushCurrent_v2")
                    .ok_or(Error::MissingSymbol)?,
                ctx_pop_current: library
                    .symbol(c"cuCtxPopCurrent_v2")
                    .ok_or(Error::MissingSymbol)?,
                mem_alloc_pitch: library
                    .symbol(c"cuMemAllocPitch_v2")
                    .ok_or(Error::MissingSymbol)?,
                mem_free: library
                    .symbol(c"cuMemFree_v2")
                    .ok_or(Error::MissingSymbol)?,
                memset_d8: library
                    .symbol(c"cuMemsetD8_v2")
                    .ok_or(Error::MissingSymbol)?,
                memcpy_2d: library
                    .symbol(c"cuMemcpy2D_v2")
                    .ok_or(Error::MissingSymbol)?,
                stream_create: library
                    .symbol(c"cuStreamCreate")
                    .ok_or(Error::MissingSymbol)?,
                stream_destroy: library
                    .symbol(c"cuStreamDestroy_v2")
                    .ok_or(Error::MissingSymbol)?,
                event_create: library
                    .symbol(c"cuEventCreate")
                    .ok_or(Error::MissingSymbol)?,
                event_destroy: library
                    .symbol(c"cuEventDestroy_v2")
                    .ok_or(Error::MissingSymbol)?,
                event_record: library
                    .symbol(c"cuEventRecord")
                    .ok_or(Error::MissingSymbol)?,
                event_query: library
                    .symbol(c"cuEventQuery")
                    .ok_or(Error::MissingSymbol)?,
                event_synchronize: library
                    .symbol(c"cuEventSynchronize")
                    .ok_or(Error::MissingSymbol)?,
                import_external_memory: library
                    .symbol(c"cuImportExternalMemory")
                    .ok_or(Error::MissingSymbol)?,
                external_memory_get_mapped_buffer: library
                    .symbol(c"cuExternalMemoryGetMappedBuffer")
                    .ok_or(Error::MissingSymbol)?,
                destroy_external_memory: library
                    .symbol(c"cuDestroyExternalMemory")
                    .ok_or(Error::MissingSymbol)?,
                memcpy_2d_async: library
                    .symbol(c"cuMemcpy2DAsync_v2")
                    .ok_or(Error::MissingSymbol)?,
                mem_get_allocation_granularity: library
                    .symbol(c"cuMemGetAllocationGranularity")
                    .ok_or(Error::MissingSymbol)?,
                mem_create: library.symbol(c"cuMemCreate").ok_or(Error::MissingSymbol)?,
                mem_release: library
                    .symbol(c"cuMemRelease")
                    .ok_or(Error::MissingSymbol)?,
                mem_address_reserve: library
                    .symbol(c"cuMemAddressReserve")
                    .ok_or(Error::MissingSymbol)?,
                mem_address_free: library
                    .symbol(c"cuMemAddressFree")
                    .ok_or(Error::MissingSymbol)?,
                mem_map: library.symbol(c"cuMemMap").ok_or(Error::MissingSymbol)?,
                mem_unmap: library.symbol(c"cuMemUnmap").ok_or(Error::MissingSymbol)?,
                mem_set_access: library
                    .symbol(c"cuMemSetAccess")
                    .ok_or(Error::MissingSymbol)?,
                mem_export_to_shareable_handle: library
                    .symbol(c"cuMemExportToShareableHandle")
                    .ok_or(Error::MissingSymbol)?,
                _library: library,
            }
            .initialised(init)?
        };
        Ok(loaded)
    }

    /// # Safety
    ///
    /// `init` must be this library's initialiser.
    unsafe fn initialised(self, init: Init) -> Result<Self> {
        // SAFETY: the caller guarantees the pointer; the flags argument is
        // documented as reserved and must be zero.
        check(unsafe { init(0) })?;
        Ok(self)
    }

    /// How many devices the runtime can see.
    pub fn device_count(&self) -> Result<u32> {
        let mut count: c_int = 0;
        // SAFETY: the pointer is to a live local for the duration of the call.
        check(unsafe { (self.device_get_count)(&raw mut count) })?;
        Ok(count.max(0).unsigned_abs())
    }

    /// The device at an enumeration position.
    ///
    /// **The position is not an identity.** It moves across driver reloads,
    /// which is why the address travels with the device and selection is by
    /// address rather than by index.
    pub fn device(&self, ordinal: u32) -> Result<Device> {
        let ordinal = c_int::try_from(ordinal).map_err(|_| Error::NoDevices)?;
        let mut handle: CUdevice = 0;
        // SAFETY: the pointer is to a live local for the duration of the call.
        check(unsafe { (self.device_get)(&raw mut handle, ordinal) })?;

        let mut buffer = [0u8; PciAddress::CAPACITY];
        // SAFETY: the buffer is writable for the length passed, and the
        // runtime writes a NUL-terminated string within it.
        check(unsafe {
            (self.device_get_pci_bus_id)(
                buffer.as_mut_ptr().cast::<c_char>(),
                c_int::try_from(buffer.len()).unwrap_or(c_int::MAX),
                handle,
            )
        })?;
        let text = CStr::from_bytes_until_nul(&buffer)
            .ok()
            .and_then(|text| text.to_str().ok())
            .ok_or(Error::NoDevices)?;
        let address = PciAddress::parse(text).ok_or(Error::NoDevices)?;

        Ok(Device { handle, address })
    }

    /// A device's model name, for a log line at startup.
    pub fn device_name(&self, device: &Device, out: &mut [u8; 96]) -> Result<usize> {
        // SAFETY: the buffer is writable for the length passed.
        check(unsafe {
            (self.device_get_name)(
                out.as_mut_ptr().cast::<c_char>(),
                c_int::try_from(out.len()).unwrap_or(c_int::MAX),
                device.handle,
            )
        })?;
        Ok(out.iter().position(|byte| *byte == 0).unwrap_or(out.len()))
    }

    /// The device at `address`, or an error.
    ///
    /// **There is no fallback.** Substituting another device would put the
    /// encoder somewhere the frames are not, which costs a copy across the bus
    /// on every frame and would be discovered as a latency figure rather than
    /// as a failure.
    pub fn device_at(&self, address: PciAddress) -> Result<Device> {
        for ordinal in 0..self.device_count()? {
            let device = self.device(ordinal)?;
            if device.address == address {
                return Ok(device);
            }
        }
        Err(Error::NoSuchDevice(address))
    }

    /// The first device, for a pipeline with no frame source to be near.
    ///
    /// Only correct while the source is synthetic. Anything reading a real
    /// display must use [`Self::device_at`], because on a machine with two
    /// GPUs the first device is not reliably the one driving the screen.
    pub fn any_device(&self) -> Result<Device> {
        if self.device_count()? == 0 {
            return Err(Error::NoDevices);
        }
        self.device(0)
    }

    /// Retain the device's primary context.
    pub fn retain_primary(&self, device: &Device) -> Result<Context> {
        let mut raw: CUcontext = core::ptr::null_mut();
        // SAFETY: the pointer is to a live local for the duration of the call.
        check(unsafe { (self.primary_ctx_retain)(&raw mut raw, device.handle) })?;
        Ok(Context {
            raw,
            device: device.handle,
            release: self.primary_ctx_release,
            push_current: self.ctx_push_current,
            pop_current: self.ctx_pop_current,
        })
    }
}

/// A pitched device allocation.
///
/// Pitched rather than packed because the driver picks an alignment the
/// hardware is happy to read, and the encoder takes the pitch as a parameter
/// rather than assuming one.
#[derive(Debug)]
pub struct DeviceBuffer {
    ptr: CUdeviceptr,
    pitch: usize,
    free: MemFree,
    /// Carried with the buffer for the same reason `free` is: an owner of one
    /// can then write into it without also holding the runtime.
    copy: Memcpy2D,
}

// SAFETY: a device allocation belongs to its context, not to a thread.
unsafe impl Send for DeviceBuffer {}

impl DeviceBuffer {
    pub fn ptr(&self) -> CUdeviceptr {
        self.ptr
    }

    pub fn pitch(&self) -> usize {
        self.pitch
    }

    /// Copy rows of host memory into this allocation, starting at `dst_row`.
    ///
    /// **Both sides keep their own stride.** A device allocation is padded to
    /// the driver's alignment and a host frame is not, so one flat copy would
    /// be right only by coincidence; every row after the first would land
    /// progressively further from where it belongs, which shows as a picture
    /// sheared rather than as an error.
    ///
    /// Synchronous with respect to the caller's memory: the copy is staged
    /// out of pageable host memory, so `source` may be overwritten as soon as
    /// this returns.
    pub fn write_rows(
        &self,
        dst_row: usize,
        source: &[u8],
        src_pitch: usize,
        row_bytes: usize,
        rows: usize,
    ) -> Result<()> {
        if rows == 0 || row_bytes == 0 {
            return Ok(());
        }
        // The final row need not be padded out, so the requirement is every
        // row but the last at full pitch, plus the bytes actually read from
        // the last one.
        if src_pitch < row_bytes || source.len() < (rows - 1) * src_pitch + row_bytes {
            return Err(Error::SourceTooSmall);
        }

        // SAFETY: plain data, whose only pointers are the two set below and
        // both are live for the call.
        let mut copy = unsafe { core::mem::zeroed::<crate::ffi::cuda::CUDA_MEMCPY2D>() };
        copy.srcMemoryType = crate::ffi::cuda::CU_MEMORYTYPE_HOST;
        copy.srcHost = source.as_ptr().cast();
        copy.srcPitch = src_pitch;
        copy.dstMemoryType = crate::ffi::cuda::CU_MEMORYTYPE_DEVICE;
        copy.dstDevice = self.ptr;
        copy.dstPitch = self.pitch;
        copy.dstY = dst_row;
        copy.WidthInBytes = row_bytes;
        copy.Height = rows;

        // SAFETY: the descriptor is live for the call, and both sides are
        // bounded above.
        check(unsafe { (self.copy)(&raw const copy) })
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        // SAFETY: allocated once, freed once; the type is neither `Copy` nor
        // `Clone`.
        unsafe { (self.free)(self.ptr) };
    }
}

impl Cuda {
    /// Copy `rows` of `row_bytes` from a pitched device address into host
    /// memory at `dst_pitch`: a decoded picture's read-back. Synchronous:
    /// the bytes are in `dst` when this returns.
    ///
    /// # Safety
    ///
    /// `src` addresses at least `rows` rows of `src_pitch` bytes in the
    /// current context.
    pub unsafe fn read_rows(
        &self,
        src: CUdeviceptr,
        src_pitch: usize,
        dst: &mut [u8],
        dst_pitch: usize,
        row_bytes: usize,
        rows: usize,
    ) -> Result<()> {
        if rows == 0 || row_bytes == 0 {
            return Ok(());
        }
        if dst_pitch < row_bytes || dst.len() < (rows - 1) * dst_pitch + row_bytes {
            return Err(Error::SourceTooSmall);
        }
        // SAFETY: plain data, whose only pointers are the two set below and
        // both are live for the call.
        let mut copy = unsafe { core::mem::zeroed::<crate::ffi::cuda::CUDA_MEMCPY2D>() };
        copy.srcMemoryType = crate::ffi::cuda::CU_MEMORYTYPE_DEVICE;
        copy.srcDevice = src;
        copy.srcPitch = src_pitch;
        copy.dstMemoryType = crate::ffi::cuda::CU_MEMORYTYPE_HOST;
        copy.dstHost = dst.as_mut_ptr().cast();
        copy.dstPitch = dst_pitch;
        copy.WidthInBytes = row_bytes;
        copy.Height = rows;
        // SAFETY: the descriptor is live for the call; the destination is
        // bounded above and the source by the caller's contract.
        check(unsafe { (self.memcpy_2d)(&raw const copy) })
    }

    /// Allocate `rows` of at least `width` bytes.
    pub fn alloc_pitch(&self, width: usize, rows: usize) -> Result<DeviceBuffer> {
        let mut ptr: CUdeviceptr = 0;
        let mut pitch: usize = 0;
        // SAFETY: both out pointers are to live locals. The element size is
        // the widest the encoder is documented to read through.
        check(unsafe { (self.mem_alloc_pitch)(&raw mut ptr, &raw mut pitch, width, rows, 16) })?;
        Ok(DeviceBuffer {
            ptr,
            pitch,
            free: self.mem_free,
            copy: self.memcpy_2d,
        })
    }

    /// Fill `count` bytes from the start of a buffer with one value.
    pub fn fill(&self, buffer: &DeviceBuffer, value: u8, count: usize) -> Result<()> {
        // SAFETY: the caller's count is bounded by the allocation it came
        // from; the pointer is live for the life of the buffer.
        check(unsafe { (self.memset_d8)(buffer.ptr, value, count) })
    }

    /// Copy rows of host memory to a pitched device address: the mirror of
    /// [`Self::read_rows`]. Synchronous with respect to `source`.
    ///
    /// # Safety
    ///
    /// `dst` addresses at least `rows` rows of `dst_pitch` bytes in the
    /// current context.
    pub unsafe fn write_rows(
        &self,
        dst: CUdeviceptr,
        dst_pitch: usize,
        source: &[u8],
        src_pitch: usize,
        row_bytes: usize,
        rows: usize,
    ) -> Result<()> {
        if rows == 0 || row_bytes == 0 {
            return Ok(());
        }
        if src_pitch < row_bytes || source.len() < (rows - 1) * src_pitch + row_bytes {
            return Err(Error::SourceTooSmall);
        }
        // SAFETY: plain data, whose only pointers are the two set below and
        // both are live for the call.
        let mut copy = unsafe { core::mem::zeroed::<crate::ffi::cuda::CUDA_MEMCPY2D>() };
        copy.srcMemoryType = crate::ffi::cuda::CU_MEMORYTYPE_HOST;
        copy.srcHost = source.as_ptr().cast();
        copy.srcPitch = src_pitch;
        copy.dstMemoryType = crate::ffi::cuda::CU_MEMORYTYPE_DEVICE;
        copy.dstDevice = dst;
        copy.dstPitch = dst_pitch;
        copy.WidthInBytes = row_bytes;
        copy.Height = rows;
        // SAFETY: the descriptor is live for the call; the source is
        // bounded above and the destination by the caller's contract.
        check(unsafe { (self.memcpy_2d)(&raw const copy) })
    }

    /// Queue a copy of `rows` of `row_bytes` between two pitched device
    /// addresses on `stream`. **Returns before the copy is done**: the
    /// caller waits for an event recorded behind it before either side is
    /// touched again.
    ///
    /// # Safety
    ///
    /// Both addresses cover `rows` rows at their pitch in the current
    /// context, and stay valid until the stream has passed the copy.
    // Two pitched sides and a stream: the copy descriptor's own fields, one
    // past the lint's count.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn copy_rows_async(
        &self,
        src: CUdeviceptr,
        src_pitch: usize,
        dst: CUdeviceptr,
        dst_pitch: usize,
        row_bytes: usize,
        rows: usize,
        stream: &Stream,
    ) -> Result<()> {
        if rows == 0 || row_bytes == 0 {
            return Ok(());
        }
        if src_pitch < row_bytes || dst_pitch < row_bytes {
            return Err(Error::SourceTooSmall);
        }
        // SAFETY: plain data, whose only pointers are the two set below.
        let mut copy = unsafe { core::mem::zeroed::<crate::ffi::cuda::CUDA_MEMCPY2D>() };
        copy.srcMemoryType = crate::ffi::cuda::CU_MEMORYTYPE_DEVICE;
        copy.srcDevice = src;
        copy.srcPitch = src_pitch;
        copy.dstMemoryType = crate::ffi::cuda::CU_MEMORYTYPE_DEVICE;
        copy.dstDevice = dst;
        copy.dstPitch = dst_pitch;
        copy.WidthInBytes = row_bytes;
        copy.Height = rows;
        // SAFETY: the descriptor is live for the call, which reads it whole
        // before returning; the addresses are the caller's contract.
        check(unsafe { (self.memcpy_2d_async)(&raw const copy, stream.raw) })
    }
}

/// A command stream the encoder is told to use.
///
/// Giving the encoder our own stream is what makes completion observable: work
/// submitted to a stream can have an event recorded behind it, and that event
/// can be asked whether it has passed without waiting for it.
#[derive(Debug)]
pub struct Stream {
    raw: CUstream,
    destroy: StreamDestroy,
}

// SAFETY: a stream belongs to its context, not to a thread.
unsafe impl Send for Stream {}

impl Stream {
    pub fn raw(&self) -> CUstream {
        self.raw
    }

    /// The address of the handle, not the handle.
    ///
    /// The encoder's stream setter takes a **pointer to** a stream, which is
    /// easy to misread: passing the handle instead makes the driver
    /// dereference a stream as though it were memory, and it faults inside the
    /// driver with nothing pointing back at the call. The address must also
    /// stay put for as long as the encoder holds it, so callers keep the
    /// stream somewhere that does not move.
    pub fn handle_ptr(&self) -> *mut core::ffi::c_void {
        (&raw const self.raw).cast::<core::ffi::c_void>().cast_mut()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: created once, destroyed once.
        unsafe { (self.destroy)(self.raw) };
    }
}

/// A marker recorded into a stream, which can be tested without blocking or
/// waited on.
#[derive(Debug)]
pub struct Event {
    raw: CUevent,
    record: EventRecord,
    query: EventQuery,
    synchronize: EventSynchronize,
    destroy: EventDestroy,
}

// SAFETY: an event belongs to its context, not to a thread.
unsafe impl Send for Event {}

impl Event {
    /// Place this event behind everything already submitted to `stream`.
    pub fn record(&self, stream: &Stream) -> Result<()> {
        // SAFETY: both handles are valid for the life of their owners.
        check(unsafe { (self.record)(self.raw, stream.raw) })
    }

    /// Has the recorded point been reached?
    ///
    /// **Does not wait.** A not-ready answer is the ordinary case and not an
    /// error, which is the whole reason this exists: it is the only completion
    /// signal on this platform that can be asked rather than waited on.
    pub fn ready(&self) -> Result<bool> {
        // SAFETY: the handle is valid for the life of `self`.
        let status = unsafe { (self.query)(self.raw) };
        match status {
            CUDA_SUCCESS => Ok(true),
            CUDA_ERROR_NOT_READY => Ok(false),
            other => Err(Error::Status(other)),
        }
    }

    /// Block until the recorded point is reached. The thread sleeps on an
    /// event from [`Cuda::create_waitable_event`]; on any other the wait
    /// follows the context's own policy, which by default spins throughout.
    pub fn wait(&self) -> Result<()> {
        // SAFETY: the handle is valid for the life of `self`.
        check(unsafe { (self.synchronize)(self.raw) })
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: created once, destroyed once.
        unsafe { (self.destroy)(self.raw) };
    }
}

impl Cuda {
    /// A stream to hand to the encoder.
    pub fn create_stream(&self) -> Result<Stream> {
        let mut raw: CUstream = core::ptr::null_mut();
        // SAFETY: the out pointer is to a live local. Flag zero is the
        // default, which orders against the legacy stream; the encoder is the
        // only producer here so nothing weaker is needed.
        check(unsafe { (self.stream_create)(&raw mut raw, 0) })?;
        Ok(Stream {
            raw,
            destroy: self.stream_destroy,
        })
    }

    /// An event for completion only.
    pub fn create_event(&self) -> Result<Event> {
        // Timing is disabled because only the fact of completion is wanted,
        // and keeping it costs a synchronisation the query would otherwise
        // not need.
        self.event(CU_EVENT_DISABLE_TIMING)
    }

    /// An event for completion only, whose wait sleeps until the device
    /// reaches it. The context's own waits spin: the thread spends the whole
    /// wait on a core, and on a machine whose cores are all busy it is
    /// preempted mid-spin and sees the completion a scheduling slice late.
    /// The context is shared with the application, so the sleep is asked of
    /// the one event rather than of the context.
    pub fn create_waitable_event(&self) -> Result<Event> {
        self.event(CU_EVENT_BLOCKING_SYNC | CU_EVENT_DISABLE_TIMING)
    }

    fn event(&self, flags: c_uint) -> Result<Event> {
        let mut raw: CUevent = core::ptr::null_mut();
        // SAFETY: the out pointer is to a live local.
        check(unsafe { (self.event_create)(&raw mut raw, flags) })?;
        Ok(Event {
            raw,
            record: self.event_record,
            query: self.event_query,
            synchronize: self.event_synchronize,
            destroy: self.event_destroy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_round_trips_and_normalises_case() {
        let lower = PciAddress::parse("0000:01:00.0").expect("parse");
        let upper = PciAddress::parse("0000:01:00.0".to_uppercase().as_str()).expect("parse");
        assert_eq!(lower, upper, "case decided equality");
        assert_eq!(lower.as_str(), "0000:01:00.0");
    }

    #[test]
    fn an_address_rejects_what_cannot_be_one() {
        assert!(PciAddress::parse("").is_none());
        assert!(PciAddress::parse(&"0".repeat(PciAddress::CAPACITY + 1)).is_none());
        // Two different devices must not compare equal.
        assert_ne!(
            PciAddress::parse("0000:01:00.0"),
            PciAddress::parse("0000:10:00.0")
        );
    }
}
