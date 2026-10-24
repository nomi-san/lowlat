//! NVIDIA's management library, opened at run time: which of its GPUs a
//! display adapter is, the memory clocks one lists, and a floor and a ceiling
//! to hold its memory clock between.
//!
//! **The library is the display driver's**, in the system directory, and its
//! state is the process's: initialised as it is loaded here, released when the
//! [`Nvml`] goes, and a GPU's handle is good until then.
//!
//! **A held clock is the driver's, not the process's.** It stays held after
//! the process that held it has gone, until it is given back, the machine
//! restarts or the driver does. Holding and giving back need the
//! administrator; nothing else here does.
//!
//! Only what is used is declared, from the library's header (API version 13):
//! the entry points by their versioned names, which the library keeps as it
//! grows, and one structure, whose layout is asserted below.

use core::ffi::{CStr, c_char, c_int, c_uint, c_void};
use core::fmt;
use core::ptr::NonNull;

use lowlat_common::dynlib::Library;

/// A call's result, as the library numbers it.
pub type Status = c_int;

const SUCCESS: Status = 0;
const NOT_SUPPORTED: Status = 3;
const NO_PERMISSION: Status = 4;

/// The memory's clock domain, as the library numbers its domains.
const CLOCK_MEM: c_int = 2;

/// The library, installed with the display driver.
const LIBRARY: &CStr = c"nvml.dll";

/// Where a GPU sits and what it is, as the library lays it out.
#[repr(C)]
struct PciInfo {
    _bus_id_legacy: [c_char; 16],
    _domain: c_uint,
    _bus: c_uint,
    _device: c_uint,
    /// The device's number in the high half, its maker's in the low.
    device_id: c_uint,
    /// The board's: its own number in the high half, its maker's in the low.
    subsystem_id: c_uint,
    _bus_id: [c_char; 32],
}

const _: () = {
    assert!(size_of::<PciInfo>() == 68);
    assert!(core::mem::offset_of!(PciInfo, device_id) == 28);
    assert!(core::mem::offset_of!(PciInfo, subsystem_id) == 32);
};

type Init = unsafe extern "C" fn() -> Status;
type Shutdown = unsafe extern "C" fn() -> Status;
type DeviceCount = unsafe extern "C" fn(*mut c_uint) -> Status;
type DeviceByIndex = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> Status;
type DevicePci = unsafe extern "C" fn(*mut c_void, *mut PciInfo) -> Status;
type MemoryClocks = unsafe extern "C" fn(*mut c_void, *mut c_uint, *mut c_uint) -> Status;
type ClockInfo = unsafe extern "C" fn(*mut c_void, c_int, *mut c_uint) -> Status;
type HoldMemory = unsafe extern "C" fn(*mut c_void, c_uint, c_uint) -> Status;
type ReleaseMemory = unsafe extern "C" fn(*mut c_void) -> Status;

/// Why the library or a call on it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No library, which is the ordinary case without NVIDIA's driver.
    Unavailable,
    /// Loaded, but missing an entry point it must export.
    MissingSymbol,
    /// None of the library's GPUs is the one asked for.
    NotFound,
    /// More than one is: two boards of one make.
    Ambiguous,
    /// A call failed, with its status.
    Status(Status),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("NVIDIA management library not available"),
            Self::MissingSymbol => {
                f.write_str("NVIDIA management library is missing an entry point")
            }
            Self::NotFound => f.write_str("no NVIDIA GPU of that make and board"),
            Self::Ambiguous => f.write_str("more than one NVIDIA GPU of that make and board"),
            Self::Status(NO_PERMISSION) => {
                f.write_str("NVIDIA management library refused, the administrator needed")
            }
            Self::Status(NOT_SUPPORTED) => f.write_str("the GPU does not support it"),
            Self::Status(s) => write!(f, "NVIDIA management library returned status {s}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

fn check(status: Status) -> Result<()> {
    if status == SUCCESS {
        Ok(())
    } else {
        Err(Error::Status(status))
    }
}

/// One of the library's GPUs, good while the [`Nvml`] it came from lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gpu(NonNull<c_void>);

/// The entry points, every one resolved before any is called.
#[derive(Clone, Copy)]
struct Entries {
    init: Init,
    shutdown: Shutdown,
    count: DeviceCount,
    by_index: DeviceByIndex,
    pci: DevicePci,
    memory_clocks: MemoryClocks,
    clock: ClockInfo,
    hold: HoldMemory,
    release: ReleaseMemory,
}

/// A resolved entry point, or the error for a library missing it.
///
/// # Safety
///
/// As [`Library::symbol`]: `T` describes the symbol as the library defines
/// it.
unsafe fn symbol<T: Copy>(library: &Library, name: &CStr) -> Result<T> {
    // SAFETY: the caller's contract.
    unsafe { library.symbol(name) }.ok_or(Error::MissingSymbol)
}

/// The loaded library, initialised.
pub struct Nvml {
    entries: Entries,
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl fmt::Debug for Nvml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Nvml")
    }
}

impl Nvml {
    /// Open the library and initialise it.
    pub fn load() -> Result<Self> {
        let library = Library::open_system(LIBRARY).ok_or(Error::Unavailable)?;
        // SAFETY: each type is the entry point's declaration in the library's
        // header, whose one calling convention this platform's is.
        let entries = unsafe {
            Entries {
                init: symbol(&library, c"nvmlInit_v2")?,
                shutdown: symbol(&library, c"nvmlShutdown")?,
                count: symbol(&library, c"nvmlDeviceGetCount_v2")?,
                by_index: symbol(&library, c"nvmlDeviceGetHandleByIndex_v2")?,
                pci: symbol(&library, c"nvmlDeviceGetPciInfo_v3")?,
                memory_clocks: symbol(&library, c"nvmlDeviceGetSupportedMemoryClocks")?,
                clock: symbol(&library, c"nvmlDeviceGetClockInfo")?,
                hold: symbol(&library, c"nvmlDeviceSetMemoryLockedClocks")?,
                release: symbol(&library, c"nvmlDeviceResetMemoryLockedClocks")?,
            }
        };
        // SAFETY: no arguments. **Initialised only once every entry point is
        // there**, so the release in the drop always follows an
        // initialisation of this one's.
        check(unsafe { (entries.init)() })?;
        Ok(Self {
            entries,
            _library: library,
        })
    }

    /// How many GPUs the library sees.
    pub fn count(&self) -> Result<u32> {
        let mut count: c_uint = 0;
        // SAFETY: the output is a live local.
        check(unsafe { (self.entries.count)(&raw mut count) })?;
        Ok(count)
    }

    /// The GPU at `index`, below [`Nvml::count`].
    pub fn gpu(&self, index: u32) -> Result<Gpu> {
        let mut handle: *mut c_void = core::ptr::null_mut();
        // SAFETY: as above.
        check(unsafe { (self.entries.by_index)(index, &raw mut handle) })?;
        NonNull::new(handle).map(Gpu).ok_or(Error::NotFound)
    }

    /// The one GPU of the make and the board a display adapter states, by
    /// the numbers the system reports for the adapter: `vendor` and `device`
    /// the maker's and the device's, `subsystem` the board's.
    pub fn matching(&self, vendor: u32, device: u32, subsystem: u32) -> Result<Gpu> {
        let mut found = None;
        for index in 0..self.count()? {
            let gpu = self.gpu(index)?;
            let mut pci = PciInfo {
                _bus_id_legacy: [0; 16],
                _domain: 0,
                _bus: 0,
                _device: 0,
                device_id: 0,
                subsystem_id: 0,
                _bus_id: [0; 32],
            };
            // SAFETY: a GPU of this library's; the output is a live local of
            // the header's layout.
            check(unsafe { (self.entries.pci)(gpu.0.as_ptr(), &raw mut pci) })?;
            if (pci.device_id, pci.subsystem_id) == ((device << 16) | vendor, subsystem) {
                if found.is_some() {
                    return Err(Error::Ambiguous);
                }
                found = Some(gpu);
            }
        }
        found.ok_or(Error::NotFound)
    }

    /// The memory clocks `gpu` may be held between, in MHz, written into
    /// `out`: the library's order, the highest first.
    pub fn memory_clocks<'o>(&self, gpu: Gpu, out: &'o mut [u32]) -> Result<&'o [u32]> {
        let mut count = c_uint::try_from(out.len()).unwrap_or(c_uint::MAX);
        // SAFETY: a GPU of this library's; `count` says how many `out` holds,
        // and the library writes no more than it is told.
        check(unsafe {
            (self.entries.memory_clocks)(gpu.0.as_ptr(), &raw mut count, out.as_mut_ptr())
        })?;
        let written = usize::try_from(count).unwrap_or(usize::MAX).min(out.len());
        Ok(&out[..written])
    }

    /// `gpu`'s memory clock now, in MHz.
    pub fn memory_clock(&self, gpu: Gpu) -> Result<u32> {
        let mut clock: c_uint = 0;
        // SAFETY: a GPU of this library's; the output is a live local.
        check(unsafe { (self.entries.clock)(gpu.0.as_ptr(), CLOCK_MEM, &raw mut clock) })?;
        Ok(clock)
    }

    /// Hold `gpu`'s memory clock between `floor` and `ceiling` MHz, two of
    /// the clocks it lists; the driver chooses between them.
    pub fn hold_memory_clock(&self, gpu: Gpu, floor: u32, ceiling: u32) -> Result<()> {
        // SAFETY: a GPU of this library's.
        check(unsafe { (self.entries.hold)(gpu.0.as_ptr(), floor, ceiling) })
    }

    /// Give `gpu`'s memory clock back to its driver.
    pub fn release_memory_clock(&self, gpu: Gpu) -> Result<()> {
        // SAFETY: a GPU of this library's.
        check(unsafe { (self.entries.release)(gpu.0.as_ptr()) })
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: initialised by `load` and released once here. The library
        // counts its initialisations, so another user's in this process
        // stands.
        unsafe { (self.entries.shutdown)() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every adapter of NVIDIA's the system lists is one GPU of the
    /// library's**, found by the make and the board, with the memory clocks
    /// it may be held between.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn each_nvidia_adapter_is_one_gpu_of_the_library() {
        let nvml = Nvml::load().expect("the management library");
        let d3d11 = crate::d3d11::D3d11::load().expect("the graphics libraries");
        let adapters = d3d11.adapters().expect("the adapters");
        let nvidia: Vec<_> = adapters
            .iter()
            .filter(|adapter| adapter.vendor == 0x10de && adapter.decodes_here())
            .collect();
        assert!(!nvidia.is_empty(), "no NVIDIA adapter renders here");
        for adapter in nvidia {
            let gpu = nvml
                .matching(adapter.vendor, adapter.device, adapter.subsystem)
                .expect("the adapter's GPU");
            let mut listed = [0; 32];
            let clocks = nvml.memory_clocks(gpu, &mut listed).expect("its clocks");
            println!(
                "{} device={:#06x} subsystem={:#010x}: memory clocks {clocks:?} MHz, now {}",
                adapter.description,
                adapter.device,
                adapter.subsystem,
                nvml.memory_clock(gpu).expect("its clock now")
            );
            assert!(!clocks.is_empty(), "no memory clock listed");
        }
    }
}
