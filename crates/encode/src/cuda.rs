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

use core::ffi::{CStr, c_char, c_int, c_uint};

use lowlat_common::dynlib::Library;

use crate::ffi::cuda::{
    CU_EVENT_DISABLE_TIMING, CUDA_ERROR_NOT_READY, CUDA_SUCCESS, CUcontext, CUdevice, CUdeviceptr,
    CUevent, CUresult, CUstream,
};

/// Versioned first, as with the encoder runtime.
const SONAMES: [&CStr; 2] = [c"libcuda.so.1", c"libcuda.so"];

type Init = unsafe extern "C" fn(c_uint) -> CUresult;
type DeviceGetCount = unsafe extern "C" fn(*mut c_int) -> CUresult;
type DeviceGet = unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult;
type DeviceGetName = unsafe extern "C" fn(*mut c_char, c_int, CUdevice) -> CUresult;
type DeviceGetPciBusId = unsafe extern "C" fn(*mut c_char, c_int, CUdevice) -> CUresult;
type MemAllocPitch =
    unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, c_uint) -> CUresult;
type MemFree = unsafe extern "C" fn(CUdeviceptr) -> CUresult;
type MemsetD8 = unsafe extern "C" fn(CUdeviceptr, u8, usize) -> CUresult;
type PrimaryCtxRetain = unsafe extern "C" fn(*mut CUcontext, CUdevice) -> CUresult;
type PrimaryCtxRelease = unsafe extern "C" fn(CUdevice) -> CUresult;
type CtxPushCurrent = unsafe extern "C" fn(CUcontext) -> CUresult;
type StreamCreate = unsafe extern "C" fn(*mut CUstream, c_uint) -> CUresult;
type StreamDestroy = unsafe extern "C" fn(CUstream) -> CUresult;
type EventCreate = unsafe extern "C" fn(*mut CUevent, c_uint) -> CUresult;
type EventDestroy = unsafe extern "C" fn(CUevent) -> CUresult;
type EventRecord = unsafe extern "C" fn(CUevent, CUstream) -> CUresult;
type EventQuery = unsafe extern "C" fn(CUevent) -> CUresult;

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
    /// Never popped: the thread that drives a session drives it for the
    /// session's life, so there is nothing to restore.
    pub fn make_current(&self) -> Result<()> {
        // SAFETY: the handle is valid for the life of `self`.
        check(unsafe { (self.push_current)(self.raw) })
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
#[derive(Debug)]
pub struct Cuda {
    device_get_count: DeviceGetCount,
    device_get: DeviceGet,
    device_get_name: DeviceGetName,
    device_get_pci_bus_id: DeviceGetPciBusId,
    primary_ctx_retain: PrimaryCtxRetain,
    primary_ctx_release: PrimaryCtxRelease,
    ctx_push_current: CtxPushCurrent,
    mem_alloc_pitch: MemAllocPitch,
    mem_free: MemFree,
    memset_d8: MemsetD8,
    stream_create: StreamCreate,
    stream_destroy: StreamDestroy,
    event_create: EventCreate,
    event_destroy: EventDestroy,
    event_record: EventRecord,
    event_query: EventQuery,
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl Cuda {
    /// Open the runtime and initialise it.
    pub fn load() -> Result<Self> {
        let library = Library::open_first(&SONAMES).ok_or(Error::Unavailable)?;

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
                mem_alloc_pitch: library
                    .symbol(c"cuMemAllocPitch_v2")
                    .ok_or(Error::MissingSymbol)?,
                mem_free: library
                    .symbol(c"cuMemFree_v2")
                    .ok_or(Error::MissingSymbol)?,
                memset_d8: library
                    .symbol(c"cuMemsetD8_v2")
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
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        // SAFETY: allocated once, freed once; the type is neither `Copy` nor
        // `Clone`.
        unsafe { (self.free)(self.ptr) };
    }
}

impl Cuda {
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
        })
    }

    /// Fill `count` bytes from the start of a buffer with one value.
    pub fn fill(&self, buffer: &DeviceBuffer, value: u8, count: usize) -> Result<()> {
        // SAFETY: the caller's count is bounded by the allocation it came
        // from; the pointer is live for the life of the buffer.
        check(unsafe { (self.memset_d8)(buffer.ptr, value, count) })
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

/// A marker recorded into a stream, which can be tested without blocking.
#[derive(Debug)]
pub struct Event {
    raw: CUevent,
    record: EventRecord,
    query: EventQuery,
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
        let mut raw: CUevent = core::ptr::null_mut();
        // SAFETY: the out pointer is to a live local. Timing is disabled
        // because only the fact of completion is wanted, and keeping it costs
        // a synchronisation the query would otherwise not need.
        check(unsafe { (self.event_create)(&raw mut raw, CU_EVENT_DISABLE_TIMING) })?;
        Ok(Event {
            raw,
            record: self.event_record,
            query: self.event_query,
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

    /// The address of whichever card is driving a connected output, read from
    /// the kernel rather than assumed. Returns `None` on a machine with no
    /// display, which is where most of continuous integration runs.
    fn display_address() -> Option<PciAddress> {
        let entries = std::fs::read_dir("/sys/class/drm").ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
            if status.trim() != "connected" {
                continue;
            }
            // card1-DP-4 -> card1 -> its device link, whose name is the address.
            let name = entry.file_name();
            let card = name.to_str()?.split('-').next()?.to_string();
            let link = std::fs::canonicalize(format!("/sys/class/drm/{card}/device")).ok()?;
            return PciAddress::parse(link.file_name()?.to_str()?);
        }
        None
    }

    /// Needs the vendor driver, so it is off by default. Run with
    /// `cargo test -p lowlat-encode -- --ignored`.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_selected_device_is_the_one_driving_the_display() {
        let cuda = Cuda::load().expect("compute runtime did not load");
        let count = cuda.device_count().expect("device count");
        assert!(count > 0, "runtime loaded but reports no devices");

        for ordinal in 0..count {
            let device = cuda.device(ordinal).expect("device");
            let mut name = [0u8; 96];
            let len = cuda.device_name(&device, &mut name).expect("name");
            println!(
                "device {ordinal}: {} at {}",
                String::from_utf8_lossy(&name[..len]),
                device.address()
            );
        }

        let Some(display) = display_address() else {
            println!("no connected output; selection by address not exercised");
            return;
        };
        println!("display is at {display}");

        let device = cuda
            .device_at(display)
            .expect("no compute device at the display's address");
        assert_eq!(device.address(), display);

        let context = cuda.retain_primary(&device).expect("primary context");
        assert!(!context.raw().is_null());

        // The refusal is the half worth proving: an address that exists on the
        // machine but belongs to another device must not silently succeed.
        let absent = PciAddress::parse("ffff:ff:ff.f").expect("parse");
        assert_eq!(
            cuda.device_at(absent).unwrap_err(),
            Error::NoSuchDevice(absent),
            "selection fell back to another device"
        );
    }
}
