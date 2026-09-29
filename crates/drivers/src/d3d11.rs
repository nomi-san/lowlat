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
//! driver replaced, so it is never kept past the process. **The order is
//! the system's high-performance one** where it has one (a discrete GPU
//! before an integrated one), the plain enumeration where it does not, so
//! the first adapter offered is the one an unnamed decoder settles on.
//!
//! **A device of the library's own, never the application's.** Two users of
//! one device serialise on its lock, and a decode would wait behind the
//! application's present.
//!
//! **A picture handed out as textures is known finished by a fence**, where
//! the system has one (Windows 10 1703 on): the device's work is queued, the
//! fence signalled behind it, and whoever needs the picture reads the fence's
//! value or sleeps on an event until it passes. The textures are shared in the
//! legacy form -- a handle another device on the same adapter opens -- one per
//! plane, with the view a shader writes them through.
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
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_FENCE_FLAG_NONE, D3D11_RESOURCE_MISC_SHARED, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3DKMT_ADAPTERTYPE, D3DKMT_CLOSEADAPTER, D3DKMT_OPENADAPTERFROMLUID,
    D3DKMT_QUERYADAPTERINFO, DXGI_ADAPTER_DESC1, DXGI_FORMAT, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
    DXGI_SAMPLE_DESC, GUID, HANDLE, HRESULT, ID3D11Device, ID3D11Device5, ID3D11DeviceContext,
    ID3D11DeviceContext4, ID3D11Fence, ID3D11Resource, ID3D11Texture2D, ID3D11UnorderedAccessView,
    ID3D11VideoContext, ID3D11VideoDevice, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1,
    IDXGIFactory6, IDXGIResource, IUnknown, KMTQAITYPE_ADAPTERTYPE, LARGE_INTEGER, LUID, NTSTATUS,
};
use crate::ffi::d3d11_guids::{
    IID_ID3D11Device5, IID_ID3D11DeviceContext4, IID_ID3D11Fence, IID_ID3D11Texture2D,
    IID_ID3D11VideoContext, IID_ID3D11VideoDevice, IID_IDXGIAdapter1, IID_IDXGIDevice,
    IID_IDXGIFactory1, IID_IDXGIFactory6, IID_IDXGIResource,
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

    /// The identity as one value, low part first: as the compute runtime
    /// reports the adapter one of its devices is.
    pub fn value(self) -> u64 {
        u64::from(self.low) | (u64::from(u32::from_ne_bytes(self.high.to_ne_bytes())) << 32)
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
    /// The board's and the silicon's revisions: with the two above, what
    /// finds the same GPU again once its identity has changed.
    pub subsystem: u32,
    pub revision: u32,
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
    /// The integrated GPU of a machine that has a discrete one beside it.
    pub integrated: bool,
}

impl Adapter {
    /// Whether this adapter is offered as a place to decode: one that
    /// renders itself and is neither a virtual display nor the software
    /// rasteriser.
    pub fn decodes_here(&self) -> bool {
        self.renders && !self.software && !self.indirect
    }

    /// Whether `other` is the same GPU, whatever identity each carries: the
    /// maker's and the board's numbers, which survive a reset and a driver
    /// restart where the identity does not.
    pub fn same_hardware(&self, other: &Adapter) -> bool {
        (self.vendor, self.device, self.subsystem, self.revision)
            == (other.vendor, other.device, other.subsystem, other.revision)
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
        // The high-performance order, from the factory that has it (Windows
        // 10 1803 on); the plain enumeration on a system without it.
        let preferred = query::<IDXGIFactory6>(factory.as_ptr().cast(), &IID_IDXGIFactory6).ok();
        for index in 0.. {
            let mut raw: *mut IDXGIAdapter1 = core::ptr::null_mut();
            // SAFETY: a live factory; the identifier and the output are live
            // locals, the output of the asked-for interface.
            let hr = unsafe {
                match &preferred {
                    Some(six) => vcall!(
                        six.as_ptr(),
                        EnumAdapterByGpuPreference,
                        index,
                        DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
                        &IID_IDXGIAdapter1,
                        (&raw mut raw).cast()
                    ),
                    None => vcall!(factory.as_ptr(), EnumAdapters1, index, &raw mut raw),
                }
            }
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
            subsystem: desc.SubSysId,
            revision: desc.Revision,
            description: String::from_utf16_lossy(name),
            driver,
            renders: kind.is_some_and(|t| t.RenderSupported() != 0),
            software: kind.is_some_and(|t| t.SoftwareDevice() != 0),
            indirect: kind.is_some_and(|t| t.IndirectDisplayDevice() != 0),
            integrated: kind.is_some_and(|t| t.HybridIntegrated() != 0),
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
    pub fn open(&self, luid: Luid) -> Result<Device> {
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

    fn device_on(&self, adapter: &Com<IDXGIAdapter1>, described: Adapter) -> Result<Device> {
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
        // The fence's two interfaces, asked of the device rather than read
        // off the system's version: both or neither.
        let fences = query::<ID3D11Device5>(device.as_ptr().cast(), &IID_ID3D11Device5)
            .ok()
            .zip(
                query::<ID3D11DeviceContext4>(context.as_ptr().cast(), &IID_ID3D11DeviceContext4)
                    .ok(),
            );
        Ok(Device {
            adapter: described,
            device,
            context,
            video,
            video_context,
            fences,
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
/// interfaces, and the fence's interfaces where the system has them.
///
/// **The immediate context is the opening thread's**: every call through
/// [`context`](Self::context), [`video_context`](Self::video_context) and
/// [`signal`](Self::signal) is made on it. The device itself, its fences and
/// the textures made on it are free-threaded, which is what lets a picture's
/// textures and fence outlive the thread and be read on the application's.
pub struct Device {
    pub adapter: Adapter,
    device: Com<ID3D11Device>,
    context: Com<ID3D11DeviceContext>,
    video: Com<ID3D11VideoDevice>,
    video_context: Com<ID3D11VideoContext>,
    fences: Option<(Com<ID3D11Device5>, Com<ID3D11DeviceContext4>)>,
}

// SAFETY: the device and its children are free-threaded; the immediate
// context is used on the opening thread alone, which the type's contract
// puts on the caller, and releasing any of these on another thread once
// nothing uses them is what reference counting is for.
unsafe impl Send for Device {}
// SAFETY: as above; `&self` from another thread reaches only the device's
// free-threaded calls.
unsafe impl Sync for Device {}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("adapter", &self.adapter)
            .field("fences", &self.fences.is_some())
            .finish()
    }
}

impl Device {
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

    /// Whether the device can tell when its work is done: the fence's
    /// interfaces are there (Windows 10 1703 on).
    pub fn has_fences(&self) -> bool {
        self.fences.is_some()
    }

    /// Whether the device is gone -- removed, hung, reset, its driver
    /// restarted -- which every call on it answers with from then on.
    pub fn lost(&self) -> bool {
        // SAFETY: a live device; the call is free-threaded.
        unsafe { vcall!(self.device.as_ptr(), GetDeviceRemovedReason) }.is_some_and(|hr| hr < 0)
    }

    /// A fence of this device's, at `initial`.
    pub fn fence(&self, initial: u64) -> Result<Fence> {
        let (device5, _) = self.fences.as_ref().ok_or(Error::MissingSymbol)?;
        let mut out: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live device; the identifier and the output are live.
        let hr = unsafe {
            vcall!(
                device5.as_ptr(),
                CreateFence,
                initial,
                D3D11_FENCE_FLAG_NONE,
                &IID_ID3D11Fence,
                &raw mut out
            )
        }
        .ok_or(Error::MissingSymbol)?;
        check(hr)?;
        // SAFETY: a fence of the asked-for interface, whose reference is ours.
        let fence =
            unsafe { Com::from_raw(out.cast::<ID3D11Fence>()) }.ok_or(Error::Unavailable)?;
        Ok(Fence { fence })
    }

    /// Queue `fence` reaching `value` behind the work queued so far, and hand
    /// the queue to the device. The opening thread's, as the context is.
    pub fn signal(&self, fence: &Fence, value: u64) -> Result<()> {
        let (_, context4) = self.fences.as_ref().ok_or(Error::MissingSymbol)?;
        // SAFETY: a live context on its own thread and a fence of this device.
        let hr = unsafe { vcall!(context4.as_ptr(), Signal, fence.fence.as_ptr(), value) }
            .ok_or(Error::MissingSymbol)?;
        check(hr)?;
        // SAFETY: as above. Without it the work waits in the runtime's buffer
        // for the next call that flushes, and the fence with it.
        unsafe { vcall!(self.context.as_ptr(), Flush) };
        Ok(())
    }

    /// A texture a shader writes and another device on the adapter opens by
    /// its handle: one plane of a picture, `format` at `width` x `height`.
    pub fn shared_texture(
        &self,
        format: DXGI_FORMAT,
        width: u32,
        height: u32,
    ) -> Result<SharedTexture> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: u32::try_from(D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS)
                .unwrap_or(0),
            CPUAccessFlags: 0,
            MiscFlags: u32::try_from(D3D11_RESOURCE_MISC_SHARED).unwrap_or(0),
        };
        let mut raw: *mut ID3D11Texture2D = core::ptr::null_mut();
        // SAFETY: a live device; the description and output are live.
        let hr = unsafe {
            vcall!(
                self.device.as_ptr(),
                CreateTexture2D,
                &raw const desc,
                core::ptr::null(),
                &raw mut raw
            )
        }
        .ok_or(Error::MissingSymbol)?;
        check(hr)?;
        // SAFETY: a texture whose reference the call handed over.
        let texture = unsafe { Com::from_raw(raw) }.ok_or(Error::Unavailable)?;
        let mut view: *mut ID3D11UnorderedAccessView = core::ptr::null_mut();
        // SAFETY: a live device and texture; no description is the whole
        // texture at its own format.
        let hr = unsafe {
            vcall!(
                self.device.as_ptr(),
                CreateUnorderedAccessView,
                texture.as_ptr().cast::<ID3D11Resource>(),
                core::ptr::null(),
                &raw mut view
            )
        }
        .ok_or(Error::MissingSymbol)?;
        check(hr)?;
        // SAFETY: a view whose reference the call handed over.
        let view = unsafe { Com::from_raw(view) }.ok_or(Error::Unavailable)?;
        let resource = query::<IDXGIResource>(texture.as_ptr().cast(), &IID_IDXGIResource)?;
        let mut handle: HANDLE = core::ptr::null_mut();
        // SAFETY: a live resource; the output is a live local.
        let hr = unsafe { vcall!(resource.as_ptr(), GetSharedHandle, &raw mut handle) }
            .ok_or(Error::MissingSymbol)?;
        check(hr)?;
        if handle.is_null() {
            return Err(Error::Unavailable);
        }
        Ok(SharedTexture {
            texture,
            view,
            handle: handle as u64,
        })
    }

    /// A texture another device shared by its handle, opened on this one.
    pub fn open_shared(&self, handle: u64) -> Result<Com<ID3D11Texture2D>> {
        let mut out: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live device; the handle is a value the system checks,
        // and the identifier and output are live.
        let hr = unsafe {
            vcall!(
                self.device.as_ptr(),
                OpenSharedResource,
                handle as HANDLE,
                &IID_ID3D11Texture2D,
                &raw mut out
            )
        }
        .ok_or(Error::MissingSymbol)?;
        check(hr)?;
        // SAFETY: a texture of the asked-for interface, whose reference is
        // ours.
        unsafe { Com::from_raw(out.cast::<ID3D11Texture2D>()) }.ok_or(Error::Unavailable)
    }
}

/// A fence: a value the device raises as the work queued before each signal
/// finishes. Read and waited on from any thread.
pub struct Fence {
    fence: Com<ID3D11Fence>,
}

// SAFETY: a fence is free-threaded; reading its value and asking for an
// event at a value are safe from any thread.
unsafe impl Send for Fence {}
// SAFETY: as above.
unsafe impl Sync for Fence {}

impl fmt::Debug for Fence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fence")
            .field("completed", &self.completed())
            .finish()
    }
}

impl Fence {
    /// The value the device has reached. **Past every value once the device
    /// is lost**, so a lost device's work reads as finished: a caller that
    /// must not trust it asks the device first.
    pub fn completed(&self) -> u64 {
        // SAFETY: a live fence.
        unsafe { vcall!(self.fence.as_ptr(), GetCompletedValue) }.unwrap_or(u64::MAX)
    }

    /// Have `event` set once the fence reaches `value`, at once if it has.
    pub fn notify_at(&self, value: u64, event: &Event) -> Result<()> {
        // SAFETY: a live fence and a live event.
        let hr = unsafe { vcall!(self.fence.as_ptr(), SetEventOnCompletion, value, event.0) }
            .ok_or(Error::MissingSymbol)?;
        check(hr)
    }
}

/// An event a thread sleeps on: set by a fence reaching a value, or by hand.
/// Auto-resetting, so one wake is taken by one wait.
pub struct Event(HANDLE);

// SAFETY: an event is a kernel object reached by its handle from any thread.
unsafe impl Send for Event {}
// SAFETY: as above.
unsafe impl Sync for Event {}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Event")
    }
}

// The system's own synchronisation calls, present in every process.
unsafe extern "system" {
    fn CreateEventW(attributes: *mut c_void, manual: i32, initial: i32, name: *const u16)
    -> HANDLE;
    fn SetEvent(event: HANDLE) -> i32;
    fn WaitForSingleObject(object: HANDLE, milliseconds: u32) -> u32;
    fn CloseHandle(object: HANDLE) -> i32;
}

impl Event {
    pub fn new() -> Result<Self> {
        // SAFETY: an unnamed auto-reset event, unset, with default security.
        let handle = unsafe { CreateEventW(core::ptr::null_mut(), 0, 0, core::ptr::null()) };
        if handle.is_null() {
            return Err(Error::Unavailable);
        }
        Ok(Self(handle))
    }

    pub fn set(&self) {
        // SAFETY: a live event.
        unsafe { SetEvent(self.0) };
    }

    /// Sleep until the event is set or `timeout` passes, whichever is first;
    /// true when it was set. A timeout below a millisecond waits one, so a
    /// wait never turns into a poll.
    pub fn wait(&self, timeout: core::time::Duration) -> bool {
        let ms = u32::try_from(timeout.as_micros().div_ceil(1000))
            .unwrap_or(u32::MAX - 1)
            .max(1);
        // SAFETY: a live event.
        unsafe { WaitForSingleObject(self.0, ms) == 0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the event `new` made, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// One plane of a picture in a texture another device opens by `handle`,
/// with the view a shader writes it through.
pub struct SharedTexture {
    texture: Com<ID3D11Texture2D>,
    view: Com<ID3D11UnorderedAccessView>,
    /// The legacy shared handle: valid while the texture lives, and reused by
    /// the system once it is freed.
    pub handle: u64,
}

// SAFETY: device children are free-threaded; the view is bound only on the
// thread that owns the device's immediate context.
unsafe impl Send for SharedTexture {}
// SAFETY: as above.
unsafe impl Sync for SharedTexture {}

impl fmt::Debug for SharedTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedTexture")
            .field("handle", &self.handle)
            .finish()
    }
}

impl SharedTexture {
    pub fn texture(&self) -> *mut ID3D11Texture2D {
        self.texture.as_ptr()
    }

    pub fn view(&self) -> *mut ID3D11UnorderedAccessView {
        self.view.as_ptr()
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
                "{} {:04x}:{:04x} {:?} driver {:?} renders {} software {} indirect {} integrated {} -> {}",
                a.luid,
                a.vendor,
                a.device,
                a.description,
                a.driver,
                a.renders,
                a.software,
                a.indirect,
                a.integrated,
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
        // The high-performance order: where a GPU that is not the integrated
        // one is offered, the integrated one is not first.
        let order: Vec<&Adapter> = adapters.iter().filter(|a| a.decodes_here()).collect();
        if order.iter().any(|a| !a.integrated) {
            assert!(!order[0].integrated, "the integrated GPU came first");
        }
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

    /// **On every offered adapter: the fence says when the device's work is
    /// done, and a shared plane opens on a second device.** A signalled value
    /// is reached and wakes an event; a value never signalled does not (the
    /// wait times out, so a wake that ignored the value would fail here); a
    /// shared texture opens by its handle on another device on the same
    /// adapter, at the size and format it was made with; and the adapter
    /// found again by its hardware is itself. Needs a GPU, so off by default.
    #[test]
    #[ignore = "requires a GPU"]
    fn a_fence_wakes_its_event_and_a_shared_plane_opens_elsewhere() {
        use crate::ffi::d3d11::DXGI_FORMAT_R8G8_UNORM;
        let d3d11 = D3d11::load().expect("the system's libraries");
        let adapters = d3d11.adapters().expect("the walk");
        for a in adapters.iter().filter(|a| a.decodes_here()) {
            let device = d3d11.open(a.luid).expect("a device");
            assert!(device.has_fences(), "{}: no fence interfaces", a.luid);
            let fence = device.fence(40).expect("a fence");
            assert_eq!(fence.completed(), 40, "the initial value");
            let event = Event::new().expect("an event");
            device.signal(&fence, 41).expect("a signal");
            fence.notify_at(41, &event).expect("a notification");
            assert!(
                event.wait(core::time::Duration::from_secs(2)),
                "{}: the signalled value never woke the event",
                a.luid
            );
            assert!(fence.completed() >= 41);
            fence.notify_at(42, &event).expect("a notification");
            assert!(
                !event.wait(core::time::Duration::from_millis(50)),
                "{}: a value never signalled woke the event",
                a.luid
            );

            let plane = device
                .shared_texture(DXGI_FORMAT_R8G8_UNORM, 640, 360)
                .expect("a shared plane");
            let other = d3d11.open(a.luid).expect("a second device");
            let opened = other.open_shared(plane.handle).expect("the plane opened");
            // SAFETY: plain data the call fills whole.
            let mut desc: D3D11_TEXTURE2D_DESC = unsafe { core::mem::zeroed() };
            // SAFETY: a live texture; the output is a live local.
            unsafe { vcall!(opened.as_ptr(), GetDesc, &raw mut desc) };
            assert_eq!(
                (desc.Width, desc.Height, desc.Format),
                (640, 360, DXGI_FORMAT_R8G8_UNORM)
            );
            let again = adapters
                .iter()
                .filter(|b| b.decodes_here() && b.same_hardware(a))
                .map(|b| b.luid)
                .collect::<Vec<_>>();
            assert!(again.contains(&a.luid), "{}: not its own hardware", a.luid);
            println!(
                "{} {:?}: fence, event and shared plane ok; same hardware {again:?}",
                a.luid, a.description
            );
        }
    }
}
