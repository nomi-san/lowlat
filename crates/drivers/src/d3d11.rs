//! The system's video decoding interface: its libraries, resolved by name,
//! the adapters that can decode, and a device of the library's own on one of
//! them.
//!
//! **An adapter is named by its identity for the boot**, the locally unique
//! identifier the system gives it, never by its place in the enumeration:
//! the order moves when a GPU is added or its driver restarts, and the same
//! GPU can be enumerated twice. A virtual display's adapter renders on
//! another GPU and enumerates under that GPU's name with an identity of its
//! own; the kernel's adapter type says which is which, and only an adapter
//! that renders itself and is neither a virtual display nor the software
//! rasteriser is offered. The identity changes when the GPU is reset or its
//! driver replaced, so it is never kept past the process.
//!
//! **A device of the library's own, never the application's.** Two users of
//! one device serialise on its lock, and a decode would wait behind the
//! application's present.
//!
//! The interfaces are held by [`Com`], which releases on drop, and called
//! through their tables with [`vcall!`](crate::vcall). What a backend does
//! with the device is the backend's; nothing here makes a decoder.

use core::ffi::{CStr, c_void};
use core::fmt;
use core::ptr::NonNull;

use lowlat_common::dynlib::Library;

use crate::ffi::d3d11::{
    _D3DKMT_ADAPTERTYPE__bindgen_ty_1__bindgen_ty_1 as AdapterFlags, D3D_DRIVER_TYPE_UNKNOWN,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3DKMT_ADAPTERTYPE, D3DKMT_CLOSEADAPTER,
    D3DKMT_OPENADAPTERFROMLUID, D3DKMT_QUERYADAPTERINFO, DXGI_ADAPTER_DESC1, GUID, HRESULT,
    ID3D11Device, ID3D11DeviceContext, ID3D11VideoContext, ID3D11VideoDevice, IDXGIAdapter,
    IDXGIAdapter1, IDXGIFactory1, IUnknown, KMTQAITYPE_ADAPTERTYPE, LARGE_INTEGER, LUID, NTSTATUS,
};
use crate::ffi::d3d11_guids::{
    IID_ID3D11VideoContext, IID_ID3D11VideoDevice, IID_IDXGIDevice, IID_IDXGIFactory1,
};

/// Call a method through an interface's table: `vcall!(pointer, Method,
/// arguments...)`, the pointer passed as the method's first argument.
/// `None` when the table has no entry there, which a live object never
/// lacks but the table's type allows. Only in an `unsafe` block: the pointer
/// must be a live interface of the type the table belongs to.
#[macro_export]
macro_rules! vcall {
    ($this:expr, $method:ident $(, $arg:expr)* $(,)?) => {{
        let this = $this;
        (*(*this).lpVtbl).$method.map(|f| f(this $(, $arg)*))
    }};
}

type CreateDxgiFactory1 = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> HRESULT;
type CreateDevice = unsafe extern "system" fn(
    *mut IDXGIAdapter,
    i32,
    *mut c_void,
    u32,
    *const i32,
    u32,
    u32,
    *mut *mut ID3D11Device,
    *mut i32,
    *mut *mut ID3D11DeviceContext,
) -> HRESULT;
type OpenAdapterFromLuid = unsafe extern "system" fn(*mut D3DKMT_OPENADAPTERFROMLUID) -> NTSTATUS;
type QueryAdapterInfo = unsafe extern "system" fn(*mut D3DKMT_QUERYADAPTERINFO) -> NTSTATUS;
type CloseAdapter = unsafe extern "system" fn(*mut D3DKMT_CLOSEADAPTER) -> NTSTATUS;

/// The enumeration's end.
const DXGI_ERROR_NOT_FOUND: HRESULT = 0x887A_0002_u32 as HRESULT;

/// Why the interface or a device could not be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A system library did not load.
    Unavailable,
    /// Loaded, but missing an entry point or a table entry it must have.
    MissingSymbol,
    /// No adapter with that identity renders here: gone since it was
    /// listed, or never one that can decode.
    NoAdapter,
    /// A call failed, with its result.
    Status(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("system video libraries not available"),
            Self::MissingSymbol => f.write_str("system video library missing an entry point"),
            Self::NoAdapter => f.write_str("no such adapter renders here"),
            Self::Status(s) => write!(f, "system video call returned 0x{s:08x}"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

/// A failed result as an error.
fn check(hr: HRESULT) -> Result<()> {
    if hr < 0 {
        Err(Error::Status(hr))
    } else {
        Ok(())
    }
}

/// An interface pointer this side holds a reference on, released on drop.
pub struct Com<T> {
    ptr: NonNull<T>,
}

impl<T> Com<T> {
    /// Take ownership of a reference.
    ///
    /// # Safety
    ///
    /// `ptr` is null or a live interface of type `T` whose reference the
    /// caller hands over, so the drop's release is the caller's own.
    pub unsafe fn from_raw(ptr: *mut T) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self { ptr })
    }

    pub fn as_ptr(&self) -> *mut T {
        self.ptr.as_ptr()
    }
}

impl<T> fmt::Debug for Com<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Com")
    }
}

impl<T> Drop for Com<T> {
    fn drop(&mut self) {
        // Every interface's table begins with the three of the base
        // interface, so the release is reached through the base's layout.
        let base = self.ptr.as_ptr().cast::<IUnknown>();
        // SAFETY: a live interface whose reference this holds (the
        // constructor's contract), released once.
        unsafe { vcall!(base, Release) };
    }
}

/// An adapter's identity for the boot. Written `luid:HIGH:LOW` in hex,
/// which is how a device is named to the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Luid {
    pub high: i32,
    pub low: u32,
}

impl Luid {
    const PREFIX: &'static str = "luid:";

    fn of(luid: LUID) -> Self {
        Self {
            high: luid.HighPart,
            low: luid.LowPart,
        }
    }

    fn raw(self) -> LUID {
        LUID {
            LowPart: self.low,
            HighPart: self.high,
        }
    }

    /// The identity a device name spells, or `None` for one that spells
    /// none.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix(Self::PREFIX)?;
        let (high, low) = rest.split_once(':')?;
        if high.len() != 8 || low.len() != 8 {
            return None;
        }
        Some(Self {
            high: i32::from_ne_bytes(u32::from_str_radix(high, 16).ok()?.to_ne_bytes()),
            low: u32::from_str_radix(low, 16).ok()?,
        })
    }
}

impl fmt::Display for Luid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let high = u32::from_ne_bytes(self.high.to_ne_bytes());
        write!(f, "{}{high:08x}:{:08x}", Self::PREFIX, self.low)
    }
}

/// One adapter the system enumerates, and what the kernel says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    pub luid: Luid,
    pub vendor: u32,
    pub device: u32,
    /// The adapter's own name.
    pub description: String,
    /// The user-mode driver's version, four parts, where the system says.
    pub driver: Option<[u16; 4]>,
    /// It renders itself, rather than handing its work to another GPU.
    pub renders: bool,
    /// The software rasteriser.
    pub software: bool,
    /// A virtual display's adapter, rendering on another GPU.
    pub indirect: bool,
}

impl Adapter {
    /// Whether this adapter is offered as a place to decode: one that
    /// renders itself and is neither a virtual display nor the software
    /// rasteriser.
    pub fn decodes_here(&self) -> bool {
        self.renders && !self.software && !self.indirect
    }

    /// The maker's name, for a label.
    pub fn maker(&self) -> Option<&'static str> {
        match self.vendor {
            0x10de => Some("NVIDIA"),
            0x1002 | 0x1022 => Some("AMD"),
            0x8086 => Some("Intel"),
            _ => None,
        }
    }
}

/// The libraries and the entry points resolved from them.
pub struct D3d11 {
    _d3d11: Library,
    _dxgi: Library,
    _gdi: Library,
    create_device: CreateDevice,
    create_factory: CreateDxgiFactory1,
    open_adapter: OpenAdapterFromLuid,
    query_adapter: QueryAdapterInfo,
    close_adapter: CloseAdapter,
}

impl fmt::Debug for D3d11 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("D3d11")
    }
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

impl D3d11 {
    /// Open the system's libraries. They ship with every supported system,
    /// so a failure here is a damaged installation.
    pub fn load() -> Result<Self> {
        let d3d11 = Library::open(c"d3d11.dll").ok_or(Error::Unavailable)?;
        let dxgi = Library::open(c"dxgi.dll").ok_or(Error::Unavailable)?;
        let gdi = Library::open(c"gdi32.dll").ok_or(Error::Unavailable)?;
        // SAFETY: each type is the entry point's declaration in the
        // platform's headers, the generated bindings' up to the calling
        // convention, which on this platform is one.
        unsafe {
            Ok(Self {
                create_device: symbol(&d3d11, c"D3D11CreateDevice")?,
                create_factory: symbol(&dxgi, c"CreateDXGIFactory1")?,
                open_adapter: symbol(&gdi, c"D3DKMTOpenAdapterFromLuid")?,
                query_adapter: symbol(&gdi, c"D3DKMTQueryAdapterInfo")?,
                close_adapter: symbol(&gdi, c"D3DKMTCloseAdapter")?,
                _d3d11: d3d11,
                _dxgi: dxgi,
                _gdi: gdi,
            })
        }
    }

    fn factory(&self) -> Result<Com<IDXGIFactory1>> {
        let mut factory: *mut c_void = core::ptr::null_mut();
        // SAFETY: the identifier and the output are live for the call.
        check(unsafe { (self.create_factory)(&IID_IDXGIFactory1, &raw mut factory) })?;
        // SAFETY: a factory of the asked-for interface, whose reference is
        // ours.
        unsafe { Com::from_raw(factory.cast::<IDXGIFactory1>()) }.ok_or(Error::Unavailable)
    }

    /// Walk the enumeration, handing each adapter and its description to
    /// `each` until it returns `Some`.
    fn walk<R>(
        &self,
        mut each: impl FnMut(&Com<IDXGIAdapter1>, &DXGI_ADAPTER_DESC1) -> Result<Option<R>>,
    ) -> Result<Option<R>> {
        let factory = self.factory()?;
        for index in 0.. {
            let mut raw: *mut IDXGIAdapter1 = core::ptr::null_mut();
            // SAFETY: a live factory; the output is a live local.
            let hr = unsafe { vcall!(factory.as_ptr(), EnumAdapters1, index, &raw mut raw) }
                .ok_or(Error::MissingSymbol)?;
            if hr == DXGI_ERROR_NOT_FOUND {
                break;
            }
            check(hr)?;
            // SAFETY: an adapter whose reference the call handed over.
            let Some(adapter) = (unsafe { Com::from_raw(raw) }) else {
                continue;
            };
            // SAFETY: plain data, filled whole by the call.
            let mut desc: DXGI_ADAPTER_DESC1 = unsafe { core::mem::zeroed() };
            // SAFETY: a live adapter; the output is a live local.
            let hr = unsafe { vcall!(adapter.as_ptr(), GetDesc1, &raw mut desc) }
                .ok_or(Error::MissingSymbol)?;
            check(hr)?;
            if let Some(found) = each(&adapter, &desc)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Every adapter the system enumerates, with what the kernel says of
    /// each; [`Adapter::decodes_here`] says which are offered.
    pub fn adapters(&self) -> Result<Vec<Adapter>> {
        let mut out = Vec::new();
        self.walk::<()>(|adapter, desc| {
            out.push(self.describe(adapter, desc));
            Ok(None)
        })?;
        Ok(out)
    }

    fn describe(&self, adapter: &Com<IDXGIAdapter1>, desc: &DXGI_ADAPTER_DESC1) -> Adapter {
        let name = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .map_or(&desc.Description[..], |end| &desc.Description[..end]);
        let mut version: LARGE_INTEGER = LARGE_INTEGER { QuadPart: 0 };
        // SAFETY: a live adapter; the output is a live local. The call asks
        // whether a device interface is supported and says the user-mode
        // driver's version in passing.
        let driver = unsafe {
            vcall!(
                adapter.as_ptr(),
                CheckInterfaceSupport,
                &IID_IDXGIDevice,
                &raw mut version
            )
        }
        .filter(|&hr| hr >= 0)
        .map(|_| {
            // SAFETY: the whole union is the one integer.
            let v = u64::from_ne_bytes(unsafe { version.QuadPart }.to_ne_bytes());
            [48, 32, 16, 0].map(|shift| u16::try_from((v >> shift) & 0xffff).unwrap_or(0))
        });
        let kind = self.kernel_type(Luid::of(desc.AdapterLuid));
        Adapter {
            luid: Luid::of(desc.AdapterLuid),
            vendor: desc.VendorId,
            device: desc.DeviceId,
            description: String::from_utf16_lossy(name),
            driver,
            renders: kind.is_some_and(|t| t.RenderSupported() != 0),
            software: kind.is_some_and(|t| t.SoftwareDevice() != 0),
            indirect: kind.is_some_and(|t| t.IndirectDisplayDevice() != 0),
        }
    }

    /// The kernel's type flags for an adapter, or `None` where it does not
    /// say.
    fn kernel_type(&self, luid: Luid) -> Option<AdapterFlags> {
        let mut open = D3DKMT_OPENADAPTERFROMLUID {
            AdapterLuid: luid.raw(),
            hAdapter: 0,
        };
        // SAFETY: a live local the call fills.
        if unsafe { (self.open_adapter)(&raw mut open) } != 0 {
            return None;
        }
        // SAFETY: plain data; zero is every flag clear.
        let mut kind: D3DKMT_ADAPTERTYPE = unsafe { core::mem::zeroed() };
        let mut query = D3DKMT_QUERYADAPTERINFO {
            hAdapter: open.hAdapter,
            Type: KMTQAITYPE_ADAPTERTYPE,
            pPrivateDriverData: (&raw mut kind).cast(),
            PrivateDriverDataSize: u32::try_from(size_of::<D3DKMT_ADAPTERTYPE>()).unwrap_or(0),
        };
        // SAFETY: the handle was just opened; the output is a live local of
        // the size named.
        let status = unsafe { (self.query_adapter)(&raw mut query) };
        let mut close = D3DKMT_CLOSEADAPTER {
            hAdapter: open.hAdapter,
        };
        // SAFETY: opened above, closed once.
        unsafe { (self.close_adapter)(&raw mut close) };
        // SAFETY: the flags and the whole word are one integer; any bits
        // are valid flags.
        let flags = unsafe { kind.__bindgen_anon_1.__bindgen_anon_1 };
        (status == 0).then_some(flags)
    }

    /// A device of the library's own on the adapter with this identity, with
    /// its video interfaces. Refused for an adapter not offered
    /// ([`Adapter::decodes_here`]).
    pub fn open(&self, luid: Luid) -> Result<Device<'_>> {
        let found = self.walk(|adapter, desc| {
            if Luid::of(desc.AdapterLuid) != luid {
                return Ok(None);
            }
            let described = self.describe(adapter, desc);
            if !described.decodes_here() {
                return Err(Error::NoAdapter);
            }
            self.device_on(adapter, described).map(Some)
        })?;
        found.ok_or(Error::NoAdapter)
    }

    fn device_on(&self, adapter: &Com<IDXGIAdapter1>, described: Adapter) -> Result<Device<'_>> {
        let mut device: *mut ID3D11Device = core::ptr::null_mut();
        let mut context: *mut ID3D11DeviceContext = core::ptr::null_mut();
        // SAFETY: a live adapter; the outputs are live locals. An explicit
        // adapter takes the unknown driver type; the default feature levels
        // are asked for.
        let hr = unsafe {
            (self.create_device)(
                adapter.as_ptr().cast::<IDXGIAdapter>(),
                D3D_DRIVER_TYPE_UNKNOWN,
                core::ptr::null_mut(),
                u32::try_from(D3D11_CREATE_DEVICE_VIDEO_SUPPORT).unwrap_or(0),
                core::ptr::null(),
                0,
                D3D11_SDK_VERSION,
                &raw mut device,
                core::ptr::null_mut(),
                &raw mut context,
            )
        };
        check(hr)?;
        // SAFETY: references the call handed over.
        let device = unsafe { Com::from_raw(device) }.ok_or(Error::Unavailable)?;
        // SAFETY: as above.
        let context = unsafe { Com::from_raw(context) }.ok_or(Error::Unavailable)?;
        let video = query::<ID3D11VideoDevice>(device.as_ptr().cast(), &IID_ID3D11VideoDevice)?;
        let video_context =
            query::<ID3D11VideoContext>(context.as_ptr().cast(), &IID_ID3D11VideoContext)?;
        Ok(Device {
            adapter: described,
            device,
            context,
            video,
            video_context,
            _d3d11: self,
        })
    }
}

/// Another interface of a live object.
fn query<T>(object: *mut IUnknown, iid: &GUID) -> Result<Com<T>> {
    let mut out: *mut c_void = core::ptr::null_mut();
    // SAFETY: a live object; the identifier and the output are live.
    let hr =
        unsafe { vcall!(object, QueryInterface, iid, &raw mut out) }.ok_or(Error::MissingSymbol)?;
    check(hr)?;
    // SAFETY: an interface of the asked-for type, whose reference is ours.
    unsafe { Com::from_raw(out.cast::<T>()) }.ok_or(Error::MissingSymbol)
}

/// A device on one adapter, with its immediate context and video
/// interfaces. One thread uses it.
pub struct Device<'a> {
    pub adapter: Adapter,
    device: Com<ID3D11Device>,
    context: Com<ID3D11DeviceContext>,
    video: Com<ID3D11VideoDevice>,
    video_context: Com<ID3D11VideoContext>,
    _d3d11: &'a D3d11,
}

impl fmt::Debug for Device<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("adapter", &self.adapter)
            .finish()
    }
}

impl Device<'_> {
    pub fn device(&self) -> *mut ID3D11Device {
        self.device.as_ptr()
    }

    pub fn context(&self) -> *mut ID3D11DeviceContext {
        self.context.as_ptr()
    }

    pub fn video(&self) -> *mut ID3D11VideoDevice {
        self.video.as_ptr()
    }

    pub fn video_context(&self) -> *mut ID3D11VideoContext {
        self.video_context.as_ptr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_reads_back_as_it_was_written() {
        for luid in [
            Luid {
                high: 0,
                low: 0x13f82,
            },
            Luid {
                high: -1,
                low: u32::MAX,
            },
            Luid {
                high: 0x1234_5678,
                low: 0x9abc_def0,
            },
        ] {
            let text = luid.to_string();
            assert_eq!(text.len(), "luid:".len() + 17, "{text}");
            assert_eq!(Luid::parse(&text), Some(luid), "{text}");
        }
        assert_eq!(
            Luid::parse("luid:00000000:00013f82"),
            Some(Luid {
                high: 0,
                low: 0x13f82
            })
        );
        for bad in [
            "",
            "luid:",
            "luid:0:13f82",
            "luid:00000000:00013f8g",
            "renderD128",
            "LUID:00000000:00013f82",
        ] {
            assert_eq!(Luid::parse(bad), None, "{bad}");
        }
    }

    /// **Every adapter the system lists, what the kernel says of it, and a
    /// video device on each one offered.** A virtual display's adapter and
    /// the software rasteriser are listed and not offered; on a machine
    /// with a virtual display the first shows here as a second row under a
    /// GPU's name. Needs a GPU, so off by default:
    /// `cargo test -p lowlat-drivers d3d11 -- --ignored --nocapture`, with
    /// `LOWLAT_EXPECT_INDIRECT` set on a machine with a virtual display.
    #[test]
    #[ignore = "requires a GPU"]
    fn every_offered_adapter_opens_a_video_device() {
        let d3d11 = D3d11::load().expect("the system's libraries");
        let adapters = d3d11.adapters().expect("the walk");
        assert!(!adapters.is_empty());
        let mut offered = 0;
        for a in &adapters {
            println!(
                "{} {:04x}:{:04x} {:?} driver {:?} renders {} software {} indirect {} -> {}",
                a.luid,
                a.vendor,
                a.device,
                a.description,
                a.driver,
                a.renders,
                a.software,
                a.indirect,
                if a.decodes_here() {
                    "offered"
                } else {
                    "not offered"
                }
            );
            if a.decodes_here() {
                offered += 1;
                let device = d3d11.open(a.luid).expect("a device on an offered adapter");
                assert_eq!(device.adapter, *a);
                assert!(!device.video().is_null() && !device.video_context().is_null());
            } else {
                assert_eq!(
                    d3d11.open(a.luid).map(|_| ()),
                    Err(Error::NoAdapter),
                    "an adapter not offered opened"
                );
            }
        }
        assert!(offered > 0, "no adapter offered");
        // On a machine known to carry a virtual display, its adapter must be
        // found and must not be offered: the flag that says so is the whole
        // filter, and nothing above fails if it were misread.
        // A virtual display's adapter hands its rendering to another GPU,
        // so a flag read from the wrong bit shows as an adapter that is both.
        for a in &adapters {
            assert!(!(a.indirect && a.renders), "{} reads as both", a.luid);
        }
        if std::env::var_os("LOWLAT_EXPECT_INDIRECT").is_some() {
            assert!(
                adapters.iter().any(|a| a.indirect && !a.decodes_here()),
                "no virtual display's adapter found"
            );
        }
        assert_eq!(
            d3d11.open(Luid { high: -1, low: 0 }).map(|_| ()),
            Err(Error::NoAdapter)
        );
    }
}
