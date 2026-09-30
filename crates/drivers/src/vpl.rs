//! Intel's own video runtime, reached at run time from where the display
//! driver installs it, and a decoding session on a device of the library's
//! own.
//!
//! **The runtime is found by the adapter.** Its display driver names a folder
//! for it in the display device's own registry key: the device among those
//! present whose hardware numbers are the adapter's. The display class's keys
//! are never walked in order, since they keep entries for drivers long
//! removed, pointing at folders that no longer exist. The current runtime is
//! the one named for this interface; a part it does not reach has the older
//! one, named the older way, or before the driver store in the runtime's own
//! registry list. Nothing is looked for on the search path or beside the
//! application and no dispatcher is involved: the runtime is one library,
//! opened by its full path.
//!
//! **A session of the current runtime decodes on the device it is handed**,
//! and the device must have its lock on: without it -- or handed over once
//! the session has touched the hardware -- the runtime refuses it and decodes
//! on a device of its own, which nothing but the pictures' device would show.
//! So the lock is turned on and the device handed over first of all. The
//! runtime makes its decode calls on that device's context, on the calling
//! thread, inside the decode call, so work queued on the context afterwards
//! runs after the decode and nothing here waits for one. It also calls into
//! the context from a thread of its own, which the lock serialises.
//!
//! **The older runtime decodes into memory of the caller's**, on a device of
//! its own on the adapter it is given by number, and is waited for.

use core::ffi::{CStr, c_void};
use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use std::ffi::CString;

use lowlat_common::dynlib::Library;

use crate::d3d11::{Adapter, Device};
use crate::ffi::vpl::{
    _mfxSession, MFX_ACCEL_MODE_VIA_D3D11, MFX_EXTBUFF_THREADS_PARAM, MFX_HANDLE_D3D11_DEVICE,
    MFX_IMPL_HARDWARE, MFX_IMPL_HARDWARE2, MFX_IMPL_HARDWARE3, MFX_IMPL_HARDWARE4,
    MFX_IMPL_VIA_D3D11, MFX_RESOURCE_DX11_TEXTURE, mfxBitstream, mfxExtBuffer, mfxExtThreadsParam,
    mfxFrameAllocRequest, mfxFrameSurface1, mfxHDL, mfxHandleType, mfxInitParam,
    mfxInitializationParam, mfxResourceType, mfxSession, mfxStatus, mfxSyncPoint, mfxU32,
    mfxVersion, mfxVideoParam,
};

type Initialize = unsafe extern "C" fn(mfxInitializationParam, *mut mfxSession) -> mfxStatus;
type InitEx = unsafe extern "C" fn(mfxInitParam, *mut mfxSession) -> mfxStatus;
type Close = unsafe extern "C" fn(mfxSession) -> mfxStatus;
type QueryVersion = unsafe extern "C" fn(mfxSession, *mut mfxVersion) -> mfxStatus;
type SetHandle = unsafe extern "C" fn(mfxSession, mfxHandleType, mfxHDL) -> mfxStatus;
type SyncOperation = unsafe extern "C" fn(mfxSession, mfxSyncPoint, mfxU32) -> mfxStatus;
type DecodeQuery =
    unsafe extern "C" fn(mfxSession, *mut mfxVideoParam, *mut mfxVideoParam) -> mfxStatus;
type DecodeHeader =
    unsafe extern "C" fn(mfxSession, *mut mfxBitstream, *mut mfxVideoParam) -> mfxStatus;
type QueryIoSurf =
    unsafe extern "C" fn(mfxSession, *mut mfxVideoParam, *mut mfxFrameAllocRequest) -> mfxStatus;
type DecodeInit = unsafe extern "C" fn(mfxSession, *mut mfxVideoParam) -> mfxStatus;
type DecodeClose = unsafe extern "C" fn(mfxSession) -> mfxStatus;
type DecodeFrameAsync = unsafe extern "C" fn(
    mfxSession,
    *mut mfxBitstream,
    *mut mfxFrameSurface1,
    *mut *mut mfxFrameSurface1,
    *mut mfxSyncPoint,
) -> mfxStatus;

/// The PCI vendor number of Intel's parts.
const INTEL: u32 = 0x8086;
/// The current runtime's file, in the folder its driver names.
const CURRENT_LIBRARY: &str = "libmfx64-gen.dll";
/// The older runtime's file.
const OLDER_LIBRARY: &str = "libmfxhw64.dll";
/// The value a display device's key names the current runtime's folder by,
/// and the older runtime's.
const CURRENT_FOLDER: &str = "DriverStorePathForVPL";
const OLDER_FOLDER: &str = "DriverStorePathForMediaSDK";
/// The older runtime's own list, before the driver store: one key per
/// installed runtime, each with its hardware numbers and its file's path.
const DISPATCH_KEY: &str = "SOFTWARE\\Intel\\MediaSDK\\Dispatch";
/// The display adapters' device class.
const DISPLAY_CLASS: &str = "{4D36E968-E325-11CE-BFC1-08002BE10318}";

/// The threads a session asks for: its scheduler's work is the completion
/// of each picture, which one thread keeps up with (the default is one per
/// core, sixteen here); two where a runtime will not take one.
const THREADS: [u16; 2] = [1, 2];

/// Why the runtime or a session could not be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A system library did not load.
    Unavailable,
    /// No runtime is installed for the adapter, or it is not an Intel one.
    NoRuntime,
    /// The runtime is missing an entry point it must have.
    MissingSymbol,
    /// The device's lock could not be turned on, without which the runtime
    /// would decode on a device of its own.
    Unprotected,
    /// The adapter is past the older runtime's four.
    Unreachable,
    /// A call failed, with its status.
    Status(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("system libraries not available"),
            Self::NoRuntime => f.write_str("no Intel video runtime for this adapter"),
            Self::MissingSymbol => f.write_str("Intel video runtime missing an entry point"),
            Self::Unprotected => f.write_str("device lock could not be turned on"),
            Self::Unreachable => f.write_str("adapter out of the older runtime's reach"),
            Self::Status(s) => write!(f, "Intel video runtime returned {s}"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

/// A failed status as an error; a warning is the call done.
fn check(status: mfxStatus) -> Result<()> {
    if status < 0 {
        Err(Error::Status(status))
    } else {
        Ok(())
    }
}

// SAFETY: every structure this is used for is plain data the runtime's
// headers define, whose all-zero value is valid and is where a fill starts.
fn zeroed<T>() -> T {
    unsafe { core::mem::zeroed() }
}

/// Which runtime a library is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    /// The current one: decodes on a device of the caller's.
    Current,
    /// The older one: decodes into memory of the caller's.
    Older,
}

/// A runtime loaded and the entry points resolved from it.
pub struct Vpl {
    runtime: Runtime,
    initialize: Option<Initialize>,
    init_ex: Option<InitEx>,
    close: Close,
    query_version: QueryVersion,
    set_handle: SetHandle,
    sync_operation: SyncOperation,
    query: DecodeQuery,
    header: DecodeHeader,
    query_io_surf: QueryIoSurf,
    init: DecodeInit,
    decode_close: DecodeClose,
    decode: DecodeFrameAsync,
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl fmt::Debug for Vpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vpl")
            .field("runtime", &self.runtime)
            .finish()
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

impl Vpl {
    /// The runtime for `adapter`: the current one where its driver names
    /// one, the older one otherwise.
    pub fn for_adapter(adapter: &Adapter) -> Result<Self> {
        if adapter.vendor != INTEL {
            return Err(Error::NoRuntime);
        }
        let registry = Registry::load()?;
        if let Some(folder) = registry.driver_value(adapter.device, CURRENT_FOLDER) {
            return Self::open(&format!("{folder}\\{CURRENT_LIBRARY}"), Runtime::Current);
        }
        if let Some(folder) = registry.driver_value(adapter.device, OLDER_FOLDER) {
            return Self::open(&format!("{folder}\\{OLDER_LIBRARY}"), Runtime::Older);
        }
        let path = registry
            .dispatch_path(adapter.device)
            .ok_or(Error::NoRuntime)?;
        Self::open(&path, Runtime::Older)
    }

    /// The runtime at the absolute `path`, as `runtime`.
    pub fn open(path: &str, runtime: Runtime) -> Result<Self> {
        let name = CString::new(path).map_err(|_| Error::NoRuntime)?;
        let library = Library::open(&name).ok_or(Error::NoRuntime)?;
        // SAFETY: each type is the entry point's declaration in the vendored
        // headers; the calling convention on this platform is one.
        unsafe {
            Ok(Self {
                runtime,
                initialize: library.symbol(c"MFXInitialize"),
                init_ex: library.symbol(c"MFXInitEx"),
                close: symbol(&library, c"MFXClose")?,
                query_version: symbol(&library, c"MFXQueryVersion")?,
                set_handle: symbol(&library, c"MFXVideoCORE_SetHandle")?,
                sync_operation: symbol(&library, c"MFXVideoCORE_SyncOperation")?,
                query: symbol(&library, c"MFXVideoDECODE_Query")?,
                header: symbol(&library, c"MFXVideoDECODE_DecodeHeader")?,
                query_io_surf: symbol(&library, c"MFXVideoDECODE_QueryIOSurf")?,
                init: symbol(&library, c"MFXVideoDECODE_Init")?,
                decode_close: symbol(&library, c"MFXVideoDECODE_Close")?,
                decode: symbol(&library, c"MFXVideoDECODE_DecodeFrameAsync")?,
                _library: library,
            })
        }
    }

    pub fn runtime(&self) -> Runtime {
        self.runtime
    }

    /// A session of the current runtime decoding on `device`, whose lock is
    /// turned on here; `index` is the device's adapter's place in the plain
    /// enumeration. The session holds a reference on the device until it
    /// closes, and makes its calls on the device's context.
    pub fn session<'r>(&'r self, device: &'r Device, index: u32) -> Result<Session<'r>> {
        let initialize = self.initialize.ok_or(Error::MissingSymbol)?;
        if !device.protect() {
            return Err(Error::Unprotected);
        }
        let mut last = Error::MissingSymbol;
        for threads in THREADS {
            let mut param: mfxExtThreadsParam = zeroed();
            param.Header.BufferId = MFX_EXTBUFF_THREADS_PARAM as u32;
            param.Header.BufferSz = u32::try_from(size_of::<mfxExtThreadsParam>()).unwrap_or(0);
            param.NumThread = threads;
            let mut buffers: [*mut mfxExtBuffer; 1] = [&raw mut param.Header];
            let mut init: mfxInitializationParam = zeroed();
            init.AccelerationMode = MFX_ACCEL_MODE_VIA_D3D11;
            init.VendorImplID = index;
            init.NumExtParam = 1;
            init.ExtParam = buffers.as_mut_ptr();
            let mut raw: mfxSession = core::ptr::null_mut();
            // SAFETY: the parameters are plain data passed by value, the
            // buffer they point at live for the call; the output is a local.
            let status = unsafe { initialize(init, &raw mut raw) };
            match (check(status), NonNull::new(raw)) {
                (Ok(()), Some(raw)) => {
                    let session = Session {
                        raw,
                        vpl: self,
                        _device: PhantomData,
                    };
                    // First of all, before the session touches the hardware.
                    // SAFETY: a live session; the device outlives it by the
                    // lifetime above.
                    check(unsafe {
                        (self.set_handle)(
                            raw.as_ptr(),
                            MFX_HANDLE_D3D11_DEVICE,
                            device.device().cast(),
                        )
                    })?;
                    return Ok(session);
                }
                (Err(e), _) => last = e,
                (Ok(()), None) => last = Error::Unavailable,
            }
        }
        Err(last)
    }

    /// A session of the older runtime on the adapter at `index` in the
    /// plain enumeration, of which it reaches the first four, decoding into
    /// memory of the caller's on a device of its own.
    pub fn system_session(&self, index: u32) -> Result<Session<'_>> {
        let init_ex = self.init_ex.ok_or(Error::MissingSymbol)?;
        let hardware = [
            MFX_IMPL_HARDWARE,
            MFX_IMPL_HARDWARE2,
            MFX_IMPL_HARDWARE3,
            MFX_IMPL_HARDWARE4,
        ];
        let which = usize::try_from(index)
            .ok()
            .and_then(|i| hardware.get(i))
            .ok_or(Error::Unreachable)?;
        let mut init: mfxInitParam = zeroed();
        init.Implementation = which | MFX_IMPL_VIA_D3D11;
        init.Version.__bindgen_anon_1.Major = 1;
        init.Version.__bindgen_anon_1.Minor = 0;
        let mut raw: mfxSession = core::ptr::null_mut();
        // SAFETY: the parameters are plain data passed by value; the output
        // is a local.
        check(unsafe { init_ex(init, &raw mut raw) })?;
        let raw = NonNull::new(raw).ok_or(Error::Unavailable)?;
        Ok(Session {
            raw,
            vpl: self,
            _device: PhantomData,
        })
    }
}

/// A session: a decoder's home, closed on drop.
pub struct Session<'r> {
    raw: NonNull<_mfxSession>,
    vpl: &'r Vpl,
    _device: PhantomData<&'r Device>,
}

impl fmt::Debug for Session<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Session")
    }
}

/// What a decode call did with a unit.
#[derive(Debug)]
pub enum Decoded<'s> {
    /// A picture out: with the order the session decodes in, the unit's own.
    Picture(Picture<'s>),
    /// The unit taken, and no picture out of it.
    MoreData,
    /// Every surface of the caller's in use.
    MoreSurface,
    /// The device busy: nothing taken, the call to be made again.
    Busy,
    /// The stream's parameters changed under the decoder, which must be
    /// built again; nothing taken.
    Incompatible,
}

impl Session<'_> {
    fn raw(&self) -> mfxSession {
        self.raw.as_ptr()
    }

    /// The version the runtime implements: major, minor.
    pub fn version(&self) -> Result<(u16, u16)> {
        let mut version: mfxVersion = zeroed();
        // SAFETY: a live session; the output is a local.
        check(unsafe { (self.vpl.query_version)(self.raw(), &raw mut version) })?;
        // SAFETY: the version is two sixteen-bit halves of one word.
        let halves = unsafe { version.__bindgen_anon_1 };
        Ok((halves.Major, halves.Minor))
    }

    /// Fill `param` from the parameter sets in `bitstream`, which is read
    /// and not decoded. `Ok(false)` where it holds none yet.
    pub fn header(&self, bitstream: &mut mfxBitstream, param: &mut mfxVideoParam) -> Result<bool> {
        // SAFETY: a live session; both are live for the call, the bitstream's
        // data the caller's.
        let status = unsafe { (self.vpl.header)(self.raw(), bitstream, param) };
        if status == crate::ffi::vpl::MFX_ERR_MORE_DATA {
            return Ok(false);
        }
        check(status).map(|()| true)
    }

    /// Whether a decoder of `param` builds here, as the runtime answers for
    /// a whole parameter set; `param` is corrected in place.
    pub fn query(&self, param: &mut mfxVideoParam) -> Result<()> {
        let param: *mut mfxVideoParam = param;
        // SAFETY: a live session; the runtime reads and writes the one
        // structure, which it allows.
        check(unsafe { (self.vpl.query)(self.raw(), param, param) })
    }

    /// The surfaces a decoder of `param` asks for.
    pub fn surfaces(&self, param: &mut mfxVideoParam) -> Result<mfxFrameAllocRequest> {
        let mut request: mfxFrameAllocRequest = zeroed();
        // SAFETY: a live session; both are live for the call.
        check(unsafe { (self.vpl.query_io_surf)(self.raw(), param, &raw mut request) })?;
        Ok(request)
    }

    /// Build the session's decoder for `param`. A failure leaves nothing
    /// built: the next build would otherwise be refused.
    pub fn init(&self, param: &mut mfxVideoParam) -> Result<()> {
        // SAFETY: a live session; the parameters are live for the call.
        let status = unsafe { (self.vpl.init)(self.raw(), param) };
        if status < 0 {
            self.close_decoder();
        }
        check(status)
    }

    /// Drop the session's decoder, if one is built.
    pub fn close_decoder(&self) {
        // SAFETY: a live session; closing no decoder answers an error and
        // changes nothing.
        unsafe { (self.vpl.decode_close)(self.raw()) };
    }

    /// Hand the decoder `bitstream`, one whole unit, with `work` a surface
    /// of the caller's for the older runtime and null for the current one,
    /// which draws its own. The unit's bytes are the caller's again once
    /// this returns.
    ///
    /// # Safety
    ///
    /// `work` is null, or a surface of the caller's, unlocked, whose
    /// description and planes stay valid and in place while the session may
    /// still use it -- until the decoder is closed or the surface's lock
    /// count is back to zero.
    pub unsafe fn decode(
        &self,
        bitstream: &mut mfxBitstream,
        work: *mut mfxFrameSurface1,
    ) -> Result<Decoded<'_>> {
        let mut out: *mut mfxFrameSurface1 = core::ptr::null_mut();
        let mut sync: mfxSyncPoint = core::ptr::null_mut();
        // SAFETY: a live session; the bitstream and outputs live for the
        // call, the work surface as the caller's contract says.
        let status =
            unsafe { (self.vpl.decode)(self.raw(), bitstream, work, &raw mut out, &raw mut sync) };
        use crate::ffi::vpl::{
            MFX_ERR_INCOMPATIBLE_VIDEO_PARAM, MFX_ERR_MORE_DATA, MFX_ERR_MORE_SURFACE,
            MFX_WRN_DEVICE_BUSY,
        };
        match status {
            MFX_ERR_MORE_DATA => return Ok(Decoded::MoreData),
            MFX_ERR_MORE_SURFACE => return Ok(Decoded::MoreSurface),
            MFX_ERR_INCOMPATIBLE_VIDEO_PARAM => return Ok(Decoded::Incompatible),
            MFX_WRN_DEVICE_BUSY => return Ok(Decoded::Busy),
            _ => check(status)?,
        }
        match (NonNull::new(out), sync.is_null()) {
            (Some(surface), false) => Ok(Decoded::Picture(Picture {
                surface,
                sync,
                _session: PhantomData,
            })),
            // A warning with no picture: the unit taken, nothing out.
            _ => Ok(Decoded::MoreData),
        }
    }

    /// Wait up to `wait_ms` for `picture` to be decoded: whether it is. The
    /// older runtime's; the current one's pictures are ordered on the
    /// device instead.
    pub fn sync(&self, picture: &Picture<'_>, wait_ms: u32) -> Result<bool> {
        // SAFETY: a live session and a sync point it handed out.
        let status = unsafe { (self.vpl.sync_operation)(self.raw(), picture.sync, wait_ms) };
        check(status)?;
        Ok(status == 0)
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        // SAFETY: a live session, closed once, its decoder with it.
        unsafe { (self.vpl.close)(self.raw()) };
    }
}

/// A picture the decoder handed out, held until dropped: a surface the
/// current runtime counts references on, given back on drop, or one of the
/// caller's own for the older runtime.
pub struct Picture<'s> {
    surface: NonNull<mfxFrameSurface1>,
    sync: mfxSyncPoint,
    _session: PhantomData<&'s ()>,
}

impl fmt::Debug for Picture<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Picture")
    }
}

impl Picture<'_> {
    /// The surface itself: its description and, for a surface of the
    /// caller's, its planes.
    pub fn surface(&self) -> *mut mfxFrameSurface1 {
        self.surface.as_ptr()
    }

    /// The time stamp the unit that completed the picture carried.
    pub fn timestamp(&self) -> u64 {
        // SAFETY: a live surface; the field is plain data.
        unsafe { (*self.surface.as_ptr()).Data.TimeStamp }
    }

    /// The texture the picture is in, an `ID3D11Texture2D` of the session's
    /// device, every plane in the one texture: borrowed from the surface,
    /// valid while the picture is held. Null for a surface of the caller's.
    pub fn texture(&self) -> *mut c_void {
        let surface = self.surface.as_ptr();
        // SAFETY: a live surface; its interface is the runtime's, or null
        // on a surface of the caller's.
        let Some(interface) = (unsafe { (*surface).__bindgen_anon_1.FrameInterface.as_ref() })
        else {
            return core::ptr::null_mut();
        };
        let Some(native) = interface.GetNativeHandle else {
            return core::ptr::null_mut();
        };
        let mut handle: mfxHDL = core::ptr::null_mut();
        let mut kind: mfxResourceType = 0;
        // SAFETY: a live surface and its own interface; the outputs are
        // locals.
        let status = unsafe { native(surface, &raw mut handle, &raw mut kind) };
        if status < 0 || kind != MFX_RESOURCE_DX11_TEXTURE {
            return core::ptr::null_mut();
        }
        handle
    }
}

impl Drop for Picture<'_> {
    fn drop(&mut self) {
        let surface = self.surface.as_ptr();
        // SAFETY: a live surface, whose interface is the runtime's -- one
        // reference the decode call handed over, released once -- or null on
        // a surface of the caller's, which nothing counts.
        unsafe {
            if let Some(interface) = (*surface).__bindgen_anon_1.FrameInterface.as_ref()
                && let Some(release) = interface.Release
            {
                release(surface);
            }
        }
    }
}

type ListSize = unsafe extern "system" fn(*mut u32, *const u16, u32) -> u32;
type List = unsafe extern "system" fn(*const u16, *mut u16, u32, u32) -> u32;
type Locate = unsafe extern "system" fn(*mut u32, *const u16, u32) -> u32;
type OpenDevKey = unsafe extern "system" fn(u32, u32, u32, u32, *mut *mut c_void, u32) -> u32;
type QueryValue = unsafe extern "system" fn(
    *mut c_void,
    *const u16,
    *mut u32,
    *mut u32,
    *mut u8,
    *mut u32,
) -> i32;
type OpenKey =
    unsafe extern "system" fn(*mut c_void, *const u16, u32, u32, *mut *mut c_void) -> i32;
type EnumKey = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *mut u16,
    *mut u32,
    *mut u32,
    *mut u16,
    *mut u32,
    *mut c_void,
) -> i32;
type CloseKey = unsafe extern "system" fn(*mut c_void) -> i32;

/// Only devices present, of one class.
const CM_GETIDLIST_FILTER_PRESENT: u32 = 0x100;
const CM_GETIDLIST_FILTER_CLASS: u32 = 0x200;
const CM_REGISTRY_SOFTWARE: u32 = 1;
const REG_DISPOSITION_OPEN_EXISTING: u32 = 1;
const KEY_READ: u32 = 0x2_0019;
const REG_SZ: u32 = 1;
const REG_DWORD: u32 = 4;
/// The machine's registry root, as the system spells it on a 64-bit
/// process: the 32-bit value sign-extended.
const HKEY_LOCAL_MACHINE: usize = 0xFFFF_FFFF_8000_0002;

/// The configuration manager's and the registry's calls, resolved at run
/// time as every library here is.
struct Registry {
    list_size: ListSize,
    list: List,
    locate: Locate,
    open_device_key: OpenDevKey,
    query_value: QueryValue,
    open_key: OpenKey,
    enum_key: EnumKey,
    close_key: CloseKey,
    _cfgmgr: Library,
    _advapi: Library,
}

/// `text` as the system's wide string, terminated.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(core::iter::once(0)).collect()
}

/// A registry key, closed on drop.
struct Key<'r> {
    raw: *mut c_void,
    registry: &'r Registry,
}

impl Drop for Key<'_> {
    fn drop(&mut self) {
        // SAFETY: a key opened by this registry, closed once.
        unsafe { (self.registry.close_key)(self.raw) };
    }
}

impl Registry {
    fn load() -> Result<Self> {
        let cfgmgr = Library::open(c"cfgmgr32.dll").ok_or(Error::Unavailable)?;
        let advapi = Library::open(c"advapi32.dll").ok_or(Error::Unavailable)?;
        // SAFETY: each type is the entry point's declaration in the
        // platform's headers.
        unsafe {
            Ok(Self {
                list_size: symbol(&cfgmgr, c"CM_Get_Device_ID_List_SizeW")?,
                list: symbol(&cfgmgr, c"CM_Get_Device_ID_ListW")?,
                locate: symbol(&cfgmgr, c"CM_Locate_DevNodeW")?,
                open_device_key: symbol(&cfgmgr, c"CM_Open_DevNode_Key")?,
                query_value: symbol(&advapi, c"RegQueryValueExW")?,
                open_key: symbol(&advapi, c"RegOpenKeyExW")?,
                enum_key: symbol(&advapi, c"RegEnumKeyExW")?,
                close_key: symbol(&advapi, c"RegCloseKey")?,
                _cfgmgr: cfgmgr,
                _advapi: advapi,
            })
        }
    }

    /// The string value `name` of the driver key of a present display
    /// device with Intel's vendor number and the device number `device`.
    fn driver_value(&self, device: u32, name: &str) -> Option<String> {
        let class = wide(DISPLAY_CLASS);
        let flags = CM_GETIDLIST_FILTER_CLASS | CM_GETIDLIST_FILTER_PRESENT;
        let mut len = 0u32;
        // SAFETY: the filter is terminated; the output is a local.
        if unsafe { (self.list_size)(&raw mut len, class.as_ptr(), flags) } != 0 {
            return None;
        }
        let mut ids = vec![0u16; usize::try_from(len).ok()?];
        // SAFETY: the buffer holds the length the size call gave.
        if unsafe { (self.list)(class.as_ptr(), ids.as_mut_ptr(), len, flags) } != 0 {
            return None;
        }
        let wanted = format!("VEN_{INTEL:04X}&DEV_{device:04X}");
        ids.split(|&c| c == 0)
            .filter(|id| !id.is_empty())
            .filter(|id| {
                String::from_utf16_lossy(id)
                    .to_ascii_uppercase()
                    .contains(&wanted)
            })
            .find_map(|id| {
                let mut id = id.to_vec();
                id.push(0);
                self.device_value(&id, name)
            })
    }

    /// The string value `name` of the driver key of the device `id`.
    fn device_value(&self, id: &[u16], name: &str) -> Option<String> {
        let mut node = 0u32;
        // SAFETY: the identifier is terminated; the output is a local.
        if unsafe { (self.locate)(&raw mut node, id.as_ptr(), 0) } != 0 {
            return None;
        }
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a node just located; the output is a local.
        let status = unsafe {
            (self.open_device_key)(
                node,
                KEY_READ,
                0,
                REG_DISPOSITION_OPEN_EXISTING,
                &raw mut raw,
                CM_REGISTRY_SOFTWARE,
            )
        };
        if status != 0 || raw.is_null() {
            return None;
        }
        let key = Key {
            raw,
            registry: self,
        };
        self.string(&key, name)
    }

    /// The string value `name` of `key`.
    fn string(&self, key: &Key<'_>, name: &str) -> Option<String> {
        let name = wide(name);
        let mut kind = 0u32;
        let mut bytes = 0u32;
        // SAFETY: a live key and a terminated name; the outputs are locals.
        let status = unsafe {
            (self.query_value)(
                key.raw,
                name.as_ptr(),
                core::ptr::null_mut(),
                &raw mut kind,
                core::ptr::null_mut(),
                &raw mut bytes,
            )
        };
        if status != 0 || kind != REG_SZ {
            return None;
        }
        let mut text = vec![0u16; usize::try_from(bytes).ok()? / 2 + 1];
        let mut size = bytes;
        // SAFETY: as above; the buffer holds at least the size asked.
        let status = unsafe {
            (self.query_value)(
                key.raw,
                name.as_ptr(),
                core::ptr::null_mut(),
                &raw mut kind,
                text.as_mut_ptr().cast(),
                &raw mut size,
            )
        };
        if status != 0 {
            return None;
        }
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        let value = String::from_utf16_lossy(text.get(..end)?);
        (!value.is_empty()).then_some(value)
    }

    /// The 32-bit value `name` of `key`.
    fn number(&self, key: &Key<'_>, name: &str) -> Option<u32> {
        let name = wide(name);
        let mut kind = 0u32;
        let mut value = 0u32;
        let mut size = 4u32;
        // SAFETY: a live key and a terminated name; the outputs are locals of
        // the size named.
        let status = unsafe {
            (self.query_value)(
                key.raw,
                name.as_ptr(),
                core::ptr::null_mut(),
                &raw mut kind,
                (&raw mut value).cast(),
                &raw mut size,
            )
        };
        (status == 0 && kind == REG_DWORD).then_some(value)
    }

    fn open(&self, parent: *mut c_void, path: &str) -> Option<Key<'_>> {
        let path = wide(path);
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live parent and a terminated path; the output is a local.
        let status = unsafe { (self.open_key)(parent, path.as_ptr(), 0, KEY_READ, &raw mut raw) };
        (status == 0 && !raw.is_null()).then_some(Key {
            raw,
            registry: self,
        })
    }

    /// The older runtime's file from its own list: the entry for Intel's
    /// vendor number and the device number `device` with the highest merit.
    fn dispatch_path(&self, device: u32) -> Option<String> {
        let root = core::ptr::without_provenance_mut::<c_void>(HKEY_LOCAL_MACHINE);
        let list = self.open(root, DISPATCH_KEY)?;
        let mut best: Option<(u32, String)> = None;
        for index in 0.. {
            let mut name = [0u16; 256];
            let mut len = 256u32;
            // SAFETY: a live key; the name buffer holds the length named.
            let status = unsafe {
                (self.enum_key)(
                    list.raw,
                    index,
                    name.as_mut_ptr(),
                    &raw mut len,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            };
            if status != 0 {
                break;
            }
            let name = String::from_utf16_lossy(name.get(..usize::try_from(len).ok()?)?);
            let Some(entry) = self.open(list.raw, &name) else {
                continue;
            };
            let ours = self.number(&entry, "VendorID") == Some(INTEL)
                && self.number(&entry, "DeviceID") == Some(device);
            let merit = self.number(&entry, "Merit").unwrap_or(0);
            if ours
                && best.as_ref().is_none_or(|(m, _)| merit > *m)
                && let Some(path) = self.string(&entry, "Path")
            {
                best = Some((merit, path));
            }
        }
        best.map(|(_, path)| path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d11::D3d11;

    /// **The runtime is found in the folder the adapter's own driver names**
    /// -- a present device's key, never the class's keys in order -- and it
    /// opens, with every entry point a decoder calls; a session on a device
    /// of ours with its lock on, and the device handed over.
    #[test]
    #[ignore = "requires an Intel GPU"]
    fn the_runtime_is_found_by_the_adapter_and_takes_our_device() {
        let d3d11 = D3d11::load().expect("the system's libraries");
        let adapter = d3d11
            .adapters()
            .expect("the adapters")
            .into_iter()
            .find(|a| a.decodes_here() && a.vendor == INTEL)
            .expect("an Intel GPU");
        let vpl = Vpl::for_adapter(&adapter).expect("the runtime");
        assert_eq!(vpl.runtime(), Runtime::Current);
        let index = d3d11
            .plain_index(adapter.luid)
            .expect("the enumeration")
            .expect("the adapter");
        let device = d3d11.open(adapter.luid).expect("a device");
        let session = vpl.session(&device, index).expect("a session");
        let (major, minor) = session.version().expect("the version");
        println!("{} runtime {major}.{minor}", adapter.description);
        assert!(major >= 2, "{major}.{minor}");
    }

    /// **The older runtime's session opens on the adapter by its place in
    /// the plain enumeration**, reached here through the system directory's
    /// loader, which answers with the current runtime in its older role --
    /// the calls, not an older part.
    #[test]
    #[ignore = "requires an Intel GPU and the system directory's loader"]
    fn the_older_session_opens_on_the_adapter_by_its_place() {
        let d3d11 = D3d11::load().expect("the system's libraries");
        let adapter = d3d11
            .adapters()
            .expect("the adapters")
            .into_iter()
            .find(|a| a.decodes_here() && a.vendor == INTEL)
            .expect("an Intel GPU");
        let index = d3d11
            .plain_index(adapter.luid)
            .expect("the enumeration")
            .expect("the adapter");
        let system = std::env::var("SystemRoot").expect("the system root");
        let vpl = Vpl::open(
            &format!("{system}\\System32\\{OLDER_LIBRARY}"),
            Runtime::Older,
        )
        .expect("the loader");
        let session = vpl.system_session(index).expect("a session");
        let (major, _) = session.version().expect("the version");
        assert_eq!(major, 1);
        assert_eq!(
            vpl.system_session(4).map(|_| ()),
            Err(Error::Unreachable),
            "a fifth adapter is out of reach"
        );
    }

    /// Another maker's GPU has no Intel runtime, whatever is installed.
    #[test]
    fn another_makers_gpu_has_no_runtime() {
        let adapter = Adapter {
            luid: crate::d3d11::Luid { high: 0, low: 1 },
            vendor: 0x10de,
            device: 0x1234,
            subsystem: 0,
            revision: 0,
            description: String::from("a GPU"),
            driver: None,
            renders: true,
            software: false,
            indirect: false,
            integrated: false,
        };
        assert_eq!(
            Vpl::for_adapter(&adapter).map(|_| ()),
            Err(Error::NoRuntime)
        );
    }
}
