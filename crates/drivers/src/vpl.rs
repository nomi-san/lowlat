//! Intel's own video runtime, reached at run time from where the display
//! driver installs it, and a session decoding or encoding on a device of the
//! library's own.
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
//!
//! **An encoder of the current runtime reads textures of the caller's**, in a
//! session made for one: an allocator that answers no request -- the runtime
//! makes its own reference pictures -- names each input surface's texture by
//! the surface's memory identity, which is the texture itself. The runtime
//! submits each picture from a thread of its own, unordered against the
//! device's context, so its own wait is the only sign a picture is encoded: a
//! fence signalled on the context after the call passes at once.

use core::ffi::{CStr, c_void};
use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use std::ffi::CString;

use lowlat_common::dynlib::Library;

use crate::d3d11::{Adapter, Device};
use crate::ffi::vpl::{
    _mfxSession, MFX_ACCEL_MODE_VIA_D3D11, MFX_BITSTREAM_COMPLETE_FRAME, MFX_ERR_INVALID_HANDLE,
    MFX_ERR_NONE, MFX_ERR_NOT_ENOUGH_BUFFER, MFX_ERR_NULL_PTR, MFX_ERR_UNSUPPORTED,
    MFX_EXTBUFF_THREADS_PARAM, MFX_FRAMETYPE_I, MFX_FRAMETYPE_IDR, MFX_FRAMETYPE_REF,
    MFX_HANDLE_D3D11_DEVICE, MFX_IMPL_HARDWARE, MFX_IMPL_HARDWARE2, MFX_IMPL_HARDWARE3,
    MFX_IMPL_HARDWARE4, MFX_IMPL_VIA_D3D11, MFX_RESOURCE_DX11_TEXTURE, mfxBitstream, mfxEncodeCtrl,
    mfxExtBuffer, mfxExtThreadsParam, mfxFrameAllocRequest, mfxFrameAllocResponse,
    mfxFrameAllocator, mfxFrameData, mfxFrameInfo, mfxFrameSurface1, mfxHDL, mfxHDLPair,
    mfxHandleType, mfxInitParam, mfxInitializationParam, mfxMemId, mfxResourceType, mfxSession,
    mfxStatus, mfxSyncPoint, mfxU32, mfxVersion, mfxVideoParam,
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
type SetFrameAllocator = unsafe extern "C" fn(mfxSession, *mut mfxFrameAllocator) -> mfxStatus;
type EncodeQuery =
    unsafe extern "C" fn(mfxSession, *mut mfxVideoParam, *mut mfxVideoParam) -> mfxStatus;
/// Init, Reset and GetVideoParam.
type EncodeParams = unsafe extern "C" fn(mfxSession, *mut mfxVideoParam) -> mfxStatus;
type EncodeClose = unsafe extern "C" fn(mfxSession) -> mfxStatus;
type EncodeFrameAsync = unsafe extern "C" fn(
    mfxSession,
    *mut mfxEncodeCtrl,
    *mut mfxFrameSurface1,
    *mut mfxBitstream,
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

/// The flag a bitstream holding one whole unit carries, as the sixteen-bit
/// field it is written into.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the assertion bounds the value to the field"
)]
const COMPLETE_FRAME: u16 = {
    assert!(MFX_BITSTREAM_COMPLETE_FRAME >= 0 && MFX_BITSTREAM_COMPLETE_FRAME <= 0xffff);
    MFX_BITSTREAM_COMPLETE_FRAME as u16
};

/// The type a forced keyframe is given: an IDR picture, intra and
/// referenced, as the sixteen-bit field it is written into.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the assertion bounds the value to the field"
)]
const FORCED_IDR: u16 = {
    let idr = MFX_FRAMETYPE_I | MFX_FRAMETYPE_REF | MFX_FRAMETYPE_IDR;
    assert!(idr >= 0 && idr <= 0xffff);
    idr as u16
};

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
    set_frame_allocator: SetFrameAllocator,
    encode_query: EncodeQuery,
    encode_init: EncodeParams,
    encode_reset: EncodeParams,
    encode_params: EncodeParams,
    encode_close: EncodeClose,
    encode: EncodeFrameAsync,
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

/// Whether `adapter`'s own driver names the current runtime, the one
/// [`Vpl::for_adapter`] opens first: read from the driver's key alone,
/// nothing loaded.
pub fn has_current(adapter: &Adapter) -> bool {
    adapter.vendor == INTEL
        && Registry::load().is_ok_and(|registry| {
            registry
                .driver_value(adapter.device, CURRENT_FOLDER)
                .is_some()
        })
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
                set_frame_allocator: symbol(&library, c"MFXVideoCORE_SetFrameAllocator")?,
                encode_query: symbol(&library, c"MFXVideoENCODE_Query")?,
                encode_init: symbol(&library, c"MFXVideoENCODE_Init")?,
                encode_reset: symbol(&library, c"MFXVideoENCODE_Reset")?,
                encode_params: symbol(&library, c"MFXVideoENCODE_GetVideoParam")?,
                encode_close: symbol(&library, c"MFXVideoENCODE_Close")?,
                encode: symbol(&library, c"MFXVideoENCODE_EncodeFrameAsync")?,
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

    /// A session as [`Vpl::session`] makes one, for an encoder reading
    /// textures of the caller's, each through the surface [`input`] makes of
    /// it.
    pub fn encoder_session<'r>(&'r self, device: &'r Device, index: u32) -> Result<Session<'r>> {
        let session = self.session(device, index)?;
        // SAFETY: a live session; the allocator is a static's, never written,
        // whose callbacks keep no state.
        check(unsafe {
            (self.set_frame_allocator)(session.raw(), (&raw const TEXTURES.0).cast_mut())
        })?;
        Ok(session)
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
    /// A warning and no picture: what is left of the unit, if anything, to
    /// be handed over again at once.
    Warning,
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

    /// The parameters of a decoder of `codec` (the runtime's codec number),
    /// filled from the parameter sets in `unit`, which is read and not
    /// decoded; none where it holds none yet.
    pub fn header(&self, unit: &[u8], codec: u32) -> Result<Option<mfxVideoParam>> {
        let len =
            u32::try_from(unit.len()).map_err(|_| Error::Status(MFX_ERR_NOT_ENOUGH_BUFFER))?;
        let mut bitstream: mfxBitstream = zeroed();
        // The runtime reads a unit and never writes it.
        bitstream.Data = unit.as_ptr().cast_mut();
        bitstream.DataLength = len;
        bitstream.MaxLength = len;
        bitstream.DataFlag = COMPLETE_FRAME;
        let mut param: mfxVideoParam = zeroed();
        param.__bindgen_anon_1.mfx.CodecId = codec;
        // SAFETY: a live session; both are locals, the bitstream's data
        // `unit`, which outlives the call; no extension buffer is attached.
        let status = unsafe { (self.vpl.header)(self.raw(), &raw mut bitstream, &raw mut param) };
        if status == crate::ffi::vpl::MFX_ERR_MORE_DATA {
            return Ok(None);
        }
        check(status).map(|()| Some(param))
    }

    /// Whether a decoder of `param` builds here, as the runtime answers for
    /// a whole parameter set; `param` is corrected in place.
    ///
    /// # Safety
    ///
    /// `param`'s extension buffers, if it names any, are live for the call.
    pub unsafe fn query(&self, param: &mut mfxVideoParam) -> Result<()> {
        let param: *mut mfxVideoParam = param;
        // SAFETY: a live session; the runtime reads and writes the one
        // structure, which it allows, and its buffers as the caller says.
        check(unsafe { (self.vpl.query)(self.raw(), param, param) })
    }

    /// The surfaces a decoder of `param` asks for.
    ///
    /// # Safety
    ///
    /// As [`Session::query`].
    pub unsafe fn surfaces(&self, param: &mut mfxVideoParam) -> Result<mfxFrameAllocRequest> {
        let mut request: mfxFrameAllocRequest = zeroed();
        // SAFETY: a live session; both are live for the call, `param`'s
        // buffers as the caller says.
        check(unsafe { (self.vpl.query_io_surf)(self.raw(), param, &raw mut request) })?;
        Ok(request)
    }

    /// Build the session's decoder for `param`. A failure leaves nothing
    /// built: the next build would otherwise be refused.
    ///
    /// # Safety
    ///
    /// As [`Session::query`].
    pub unsafe fn init(&self, param: &mut mfxVideoParam) -> Result<()> {
        // SAFETY: a live session; the parameters are live for the call, their
        // buffers as the caller says.
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
        let nothing = if status > 0 {
            Decoded::Warning
        } else {
            Decoded::MoreData
        };
        let Some(surface) = NonNull::new(out) else {
            return Ok(nothing);
        };
        let picture = Picture {
            surface,
            sync,
            session: self.raw,
            _session: PhantomData,
        };
        if sync.is_null() {
            // A surface with nothing to wait on is no picture; it goes back
            // at once, as every surface handed out does, since the runtime
            // may have counted a reference on it.
            drop(picture);
            return Ok(nothing);
        }
        Ok(Decoded::Picture(picture))
    }

    /// Wait up to `wait_ms` for `picture` to be decoded: whether it is. The
    /// older runtime's; the current one's pictures are ordered on the
    /// device instead. A picture of another session is refused.
    pub fn sync(&self, picture: &Picture<'_>, wait_ms: u32) -> Result<bool> {
        if picture.session != self.raw {
            return Err(Error::Status(MFX_ERR_INVALID_HANDLE));
        }
        // SAFETY: a live session and a sync point it handed out.
        let status = unsafe { (self.vpl.sync_operation)(self.raw(), picture.sync, wait_ms) };
        check(status)?;
        Ok(status == 0)
    }

    /// Whether an encoder of `param` builds here, as the runtime answers for
    /// a whole parameter set; `param` and its buffers are corrected in place.
    ///
    /// # Safety
    ///
    /// As [`Session::query`].
    pub unsafe fn query_encoder(&self, param: &mut mfxVideoParam) -> Result<()> {
        let param: *mut mfxVideoParam = param;
        // SAFETY: a live session; the runtime reads and writes the one
        // structure, which it allows, and its buffers as the caller says.
        check(unsafe { (self.vpl.encode_query)(self.raw(), param, param) })
    }

    /// Build the session's encoder for `param`, on a session
    /// [`Vpl::encoder_session`] made. A failure leaves nothing built: the next
    /// build would otherwise be refused.
    ///
    /// # Safety
    ///
    /// As [`Session::query`].
    pub unsafe fn init_encoder(&self, param: &mut mfxVideoParam) -> Result<()> {
        // SAFETY: a live session; the parameters are live for the call, their
        // buffers as the caller says.
        let status = unsafe { (self.vpl.encode_init)(self.raw(), param) };
        if status < 0 {
            self.close_encoder();
        }
        check(status)
    }

    /// Change the built encoder to `param` without a new build. What the
    /// runtime cannot change within the stream it refuses, or changes with a
    /// new sequence -- a keyframe -- unless `param` carries the reset option
    /// that forbids one.
    ///
    /// # Safety
    ///
    /// As [`Session::query`].
    pub unsafe fn reset_encoder(&self, param: &mut mfxVideoParam) -> Result<()> {
        // SAFETY: as in `init_encoder`.
        check(unsafe { (self.vpl.encode_reset)(self.raw(), param) })
    }

    /// The parameters the built encoder runs with, written into `param` and
    /// the buffers it names.
    ///
    /// # Safety
    ///
    /// As [`Session::query`].
    pub unsafe fn encoder_params(&self, param: &mut mfxVideoParam) -> Result<()> {
        // SAFETY: as in `init_encoder`.
        check(unsafe { (self.vpl.encode_params)(self.raw(), param) })
    }

    /// Drop the session's encoder, if one is built, once every picture it
    /// took is encoded.
    pub fn close_encoder(&self) {
        // SAFETY: a live session; closing no encoder answers an error and
        // changes nothing.
        unsafe { (self.vpl.encode_close)(self.raw()) };
    }

    /// Hand the encoder `surface` for one picture, an IDR picture when `idr`,
    /// its unit to be written into `bitstream` in place of what that held.
    /// The runtime reads the one and writes the other from threads of its
    /// own until the picture is encoded.
    ///
    /// # Safety
    ///
    /// `surface` is one [`input`] made of a texture of the session's device,
    /// and `bitstream`'s data is `MaxLength` bytes of the caller's. For a
    /// picture taken, the surface, the bitstream, the texture and the bytes
    /// stay valid, in place and untouched by the caller until
    /// [`Session::sync_encoded`] says it is encoded or the encoder is closed.
    pub unsafe fn encode(
        &self,
        surface: *mut mfxFrameSurface1,
        bitstream: *mut mfxBitstream,
        idr: bool,
    ) -> Result<Encoded> {
        let mut control: mfxEncodeCtrl = zeroed();
        if idr {
            control.FrameType = FORCED_IDR;
        }
        // SAFETY: the bitstream is live and no picture is writing it, by the
        // caller's contract.
        unsafe {
            (*bitstream).DataOffset = 0;
            (*bitstream).DataLength = 0;
        }
        let mut sync: mfxSyncPoint = core::ptr::null_mut();
        // SAFETY: a live session; the control, which the call copies, and the
        // output are locals; the surface and the bitstream as the caller's
        // contract says.
        let status = unsafe {
            (self.vpl.encode)(
                self.raw(),
                &raw mut control,
                surface,
                bitstream,
                &raw mut sync,
            )
        };
        use crate::ffi::vpl::{MFX_ERR_MORE_DATA, MFX_WRN_DEVICE_BUSY};
        match status {
            MFX_WRN_DEVICE_BUSY => return Ok(Encoded::Busy),
            MFX_ERR_MORE_DATA => return Ok(Encoded::MoreData),
            _ => check(status)?,
        }
        if sync.is_null() {
            return Ok(Encoded::MoreData);
        }
        Ok(Encoded::Pending(Pending {
            sync,
            session: self.raw,
        }))
    }

    /// Wait up to `wait_ms` for `pending`'s picture to be encoded: whether it
    /// is, its unit then in the bitstream it was handed with. A picture of
    /// another session is refused.
    pub fn sync_encoded(&self, pending: &Pending, wait_ms: u32) -> Result<bool> {
        if pending.session != self.raw {
            return Err(Error::Status(MFX_ERR_INVALID_HANDLE));
        }
        // SAFETY: a live session and a sync point it handed out.
        let status = unsafe { (self.vpl.sync_operation)(self.raw(), pending.sync, wait_ms) };
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
    /// The session that handed it out, whose sync point it carries.
    session: NonNull<_mfxSession>,
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

/// What an encode call did with a picture.
#[derive(Debug)]
pub enum Encoded {
    /// Taken: its unit is in the bitstream once the session says so.
    Pending(Pending),
    /// Taken, with no unit to wait for yet: the encoder holds it back.
    MoreData,
    /// The device busy: nothing taken, the call to be made again.
    Busy,
}

/// A picture an encoder took, waited for through its session.
pub struct Pending {
    sync: mfxSyncPoint,
    /// The session that took it, whose sync point it carries.
    session: NonNull<_mfxSession>,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pending")
    }
}

/// The surface an encoder of an encoding session reads `texture` through:
/// an `ID3D11Texture2D` of the session's device, its first subresource the
/// picture, as `info` -- the encoder's own frame description -- describes
/// it. Made once for each texture and handed over with each picture.
pub fn input(info: mfxFrameInfo, texture: *mut c_void) -> mfxFrameSurface1 {
    let mut surface: mfxFrameSurface1 = zeroed();
    surface.Info = info;
    surface.Data.MemId = texture;
    surface
}

/// The allocator an encoding session names its inputs' textures through.
/// It answers no request -- the runtime makes its own reference pictures --
/// and hands a surface's memory identity back as the texture it is. A
/// static, so it outlives every session it is set on however the runtime
/// holds it, and never written.
struct Textures(mfxFrameAllocator);

// SAFETY: never written, and its callbacks keep no state.
unsafe impl Sync for Textures {}

static TEXTURES: Textures = Textures(mfxFrameAllocator {
    reserved: [0; 4],
    // The allocator's own address, as the header asks; nothing reads
    // through it.
    pthis: (&raw const TEXTURES).cast_mut().cast(),
    Alloc: Some(answer_nothing),
    Lock: Some(map_nothing),
    Unlock: Some(map_nothing),
    GetHDL: Some(texture_of),
    Free: Some(free_nothing),
});

/// Every request answered with nothing: the runtime makes what it asks for
/// itself.
unsafe extern "C" fn answer_nothing(
    _: mfxHDL,
    _: *mut mfxFrameAllocRequest,
    response: *mut mfxFrameAllocResponse,
) -> mfxStatus {
    // SAFETY: the runtime hands over a response of its own to fill, or null.
    if let Some(response) = unsafe { response.as_mut() } {
        *response = zeroed();
    }
    MFX_ERR_NONE
}

/// No texture is mapped into memory.
unsafe extern "C" fn map_nothing(_: mfxHDL, _: mfxMemId, _: *mut mfxFrameData) -> mfxStatus {
    MFX_ERR_UNSUPPORTED
}

/// A surface's texture: its memory identity, written with its first
/// subresource into both halves of the pair the runtime hands over -- its
/// own pair, filled, never pointed at one of ours.
unsafe extern "C" fn texture_of(_: mfxHDL, id: mfxMemId, handle: *mut mfxHDL) -> mfxStatus {
    // SAFETY: on a device's session the runtime hands over a pair of its own
    // for the texture and its subresource, or null.
    let Some(pair) = (unsafe { handle.cast::<mfxHDLPair>().as_mut() }) else {
        return MFX_ERR_NULL_PTR;
    };
    pair.first = id;
    pair.second = core::ptr::null_mut();
    MFX_ERR_NONE
}

/// Nothing was answered, so nothing is freed.
unsafe extern "C" fn free_nothing(_: mfxHDL, _: *mut mfxFrameAllocResponse) -> mfxStatus {
    MFX_ERR_NONE
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
    /// opens, with every entry point a decoder and an encoder call; a session
    /// on a device of ours with its lock on, and the device handed over. The
    /// same key answers whether the current runtime is there with nothing
    /// loaded, and no other maker's GPU has one.
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
        assert!(has_current(&adapter), "the current runtime not named");
        // Another maker's GPU names none, whatever Intel's driver says.
        if let Some(other) = d3d11
            .adapters()
            .expect("the adapters")
            .into_iter()
            .find(|a| a.decodes_here() && a.vendor != INTEL)
        {
            assert!(!has_current(&other), "{} named Intel's", other.description);
        }
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

    /// The type of the first unit in `bytes` led by a start code.
    fn first_unit_type(bytes: &[u8], hevc: bool) -> Option<u8> {
        let at = bytes.windows(3).position(|w| w == [0, 0, 1])?;
        let header = *bytes.get(at + 3)?;
        Some(if hevc {
            (header >> 1) & 0x3f
        } else {
            header & 0x1f
        })
    }

    /// An extension buffer's header naming it `id`, sized as `T`.
    fn header<T>(id: i32) -> mfxExtBuffer {
        mfxExtBuffer {
            BufferId: u32::try_from(id).unwrap(),
            BufferSz: u32::try_from(size_of::<T>()).unwrap(),
        }
    }

    /// One picture through `session`'s encoder, waited for: its type as the
    /// runtime reports it, its unit in `bitstream`.
    fn encode_one(
        session: &Session<'_>,
        surface: &mut mfxFrameSurface1,
        bitstream: &mut mfxBitstream,
        idr: bool,
    ) -> u16 {
        // SAFETY: the surface's texture and the bitstream's bytes outlive the
        // wait below, which the picture is done by.
        let pending = match unsafe { session.encode(surface, bitstream, idr) }.unwrap() {
            Encoded::Pending(pending) => pending,
            other => panic!("nothing to wait for: {other:?}"),
        };
        assert!(session.sync_encoded(&pending, 1000).unwrap(), "not in 1 s");
        bitstream.FrameType
    }

    /// **An encoding session on a device of ours builds both encoders in
    /// turn, and each encodes a texture of ours** -- bound for unordered
    /// access alone, as the conversion's are -- named through the session's
    /// allocator: forced, a keyframe whose first unit is a parameter set with
    /// no delimiter ahead of it; not forced, no keyframe, before and after a
    /// change of rate made without a new sequence. The parameters read back
    /// as built, and a picture left in the encoder is encoded by its close.
    #[test]
    #[ignore = "requires an Intel GPU"]
    fn an_encoding_session_encodes_our_texture_in_both_codecs() {
        use crate::ffi::d3d11::{
            D3D11_BIND_UNORDERED_ACCESS, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
        };
        use crate::ffi::vpl::{
            MFX_CHROMAFORMAT_YUV420, MFX_CODEC_AVC, MFX_CODEC_HEVC, MFX_CODINGOPTION_OFF,
            MFX_CODINGOPTION_ON, MFX_EXTBUFF_CODING_OPTION, MFX_EXTBUFF_ENCODER_RESET_OPTION,
            MFX_FOURCC_NV12, MFX_IOPATTERN_IN_VIDEO_MEMORY, MFX_PICSTRUCT_PROGRESSIVE,
            MFX_PROFILE_AVC_HIGH, MFX_PROFILE_HEVC_MAIN, MFX_RATECONTROL_VBR,
            MFX_TARGETUSAGE_BEST_SPEED, mfxExtCodingOption, mfxExtEncoderResetOption,
            mfxFrameInfo__bindgen_ty_1__bindgen_ty_1,
        };
        const WIDTH: u16 = 1280;
        const HEIGHT: u16 = 720;
        let d3d11 = D3d11::load().expect("the system's libraries");
        let adapter = d3d11
            .adapters()
            .expect("the adapters")
            .into_iter()
            .find(|a| a.decodes_here() && a.vendor == INTEL)
            .expect("an Intel GPU");
        let vpl = Vpl::for_adapter(&adapter).expect("the runtime");
        let index = d3d11
            .plain_index(adapter.luid)
            .expect("the enumeration")
            .expect("the adapter");
        let device = d3d11.open(adapter.luid).expect("a device");
        let texture = device
            .texture(&D3D11_TEXTURE2D_DESC {
                Width: u32::from(WIDTH),
                Height: u32::from(HEIGHT),
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: u32::try_from(D3D11_BIND_UNORDERED_ACCESS).unwrap(),
                CPUAccessFlags: 0,
                MiscFlags: 0,
            })
            .expect("a texture");
        let session = vpl.encoder_session(&device, index).expect("a session");
        let mut data = vec![0u8; 4 << 20];
        let mut bitstream: mfxBitstream = zeroed();
        bitstream.Data = data.as_mut_ptr();
        bitstream.MaxLength = u32::try_from(data.len()).unwrap();
        let on = u16::try_from(MFX_CODINGOPTION_ON).unwrap();
        let off = u16::try_from(MFX_CODINGOPTION_OFF).unwrap();
        let keyframe = u16::try_from(MFX_FRAMETYPE_I | MFX_FRAMETYPE_IDR).unwrap();
        for (codec, name, hevc) in [
            (MFX_CODEC_AVC, "H.264", false),
            (MFX_CODEC_HEVC, "HEVC", true),
        ] {
            let mut options: mfxExtCodingOption = zeroed();
            options.Header = header::<mfxExtCodingOption>(MFX_EXTBUFF_CODING_OPTION);
            options.AUDelimiter = off;
            options.NalHrdConformance = off;
            options.PicTimingSEI = off;
            let mut reset: mfxExtEncoderResetOption = zeroed();
            reset.Header = header::<mfxExtEncoderResetOption>(MFX_EXTBUFF_ENCODER_RESET_OPTION);
            reset.StartNewSequence = off;
            let mut buffers: [*mut mfxExtBuffer; 2] =
                [&raw mut options.Header, &raw mut reset.Header];
            let mut param: mfxVideoParam = zeroed();
            param.AsyncDepth = 1;
            param.IOPattern = u16::try_from(MFX_IOPATTERN_IN_VIDEO_MEMORY).unwrap();
            param.ExtParam = buffers.as_mut_ptr();
            param.NumExtParam = 1;
            // SAFETY: the union's encoder half, every field of it plain data.
            let mfx = unsafe { &mut param.__bindgen_anon_1.mfx };
            mfx.CodecId = u32::try_from(codec).unwrap();
            let profile = if hevc {
                MFX_PROFILE_HEVC_MAIN
            } else {
                MFX_PROFILE_AVC_HIGH
            };
            mfx.CodecProfile = u16::try_from(profile).unwrap();
            mfx.LowPower = on;
            // SAFETY: as above.
            let rate = unsafe { &mut mfx.__bindgen_anon_1.__bindgen_anon_1 };
            rate.TargetUsage = u16::try_from(MFX_TARGETUSAGE_BEST_SPEED).unwrap();
            rate.GopPicSize = 0xffff;
            rate.GopRefDist = 1;
            rate.RateControlMethod = u16::try_from(MFX_RATECONTROL_VBR).unwrap();
            rate.BufferSizeInKB = 64;
            rate.__bindgen_anon_1.InitialDelayInKB = 64;
            rate.__bindgen_anon_2.TargetKbps = 10_000;
            rate.__bindgen_anon_3.MaxKbps = 10_000;
            rate.NumSlice = 1;
            rate.NumRefFrame = 1;
            let info = &mut mfx.FrameInfo;
            info.FourCC = u32::try_from(MFX_FOURCC_NV12).unwrap();
            info.ChromaFormat = u16::try_from(MFX_CHROMAFORMAT_YUV420).unwrap();
            info.BitDepthLuma = 8;
            info.BitDepthChroma = 8;
            info.__bindgen_anon_1.__bindgen_anon_1 = mfxFrameInfo__bindgen_ty_1__bindgen_ty_1 {
                Width: WIDTH,
                Height: HEIGHT,
                CropX: 0,
                CropY: 0,
                CropW: WIDTH,
                CropH: HEIGHT,
            };
            info.FrameRateExtN = 60;
            info.FrameRateExtD = 1;
            info.PicStruct = u16::try_from(MFX_PICSTRUCT_PROGRESSIVE).unwrap();
            // SAFETY: the buffers named are live for every call below.
            unsafe { session.query_encoder(&mut param) }.expect("the parameters");
            unsafe { session.init_encoder(&mut param) }.expect("an encoder");
            // SAFETY: the union's encoder half, written above.
            let info = unsafe { param.__bindgen_anon_1.mfx.FrameInfo };
            let mut surface = input(info, texture.as_ptr().cast());
            let parameter_set = if hevc { 32 } else { 7 };
            let unit = |bitstream: &mfxBitstream, data: &[u8]| -> Vec<u8> {
                let at = usize::try_from(bitstream.DataOffset).unwrap();
                let len = usize::try_from(bitstream.DataLength).unwrap();
                data.get(at..at + len).expect("the unit").to_vec()
            };

            let kind = encode_one(&session, &mut surface, &mut bitstream, true);
            let first = unit(&bitstream, &data);
            assert_ne!(kind & keyframe, 0, "{name}: forced, type {kind:#x}");
            assert_eq!(
                first_unit_type(&first, hevc),
                Some(parameter_set),
                "{name}: {} bytes",
                first.len()
            );
            let kind = encode_one(&session, &mut surface, &mut bitstream, false);
            let second = unit(&bitstream, &data);
            assert_eq!(kind & keyframe, 0, "{name}: unforced, type {kind:#x}");
            assert_ne!(first_unit_type(&second, hevc), Some(parameter_set));

            let mut read: mfxExtCodingOption = zeroed();
            read.Header = header::<mfxExtCodingOption>(MFX_EXTBUFF_CODING_OPTION);
            let mut read_buffers: [*mut mfxExtBuffer; 1] = [&raw mut read.Header];
            let mut running: mfxVideoParam = zeroed();
            running.ExtParam = read_buffers.as_mut_ptr();
            running.NumExtParam = 1;
            // SAFETY: the buffer named is live for the call.
            unsafe { session.encoder_params(&mut running) }.expect("the parameters");
            assert_eq!(target_of(&running), 10_000, "{name}");
            assert_eq!(read.AUDelimiter, off, "{name}");

            // The build's own parameters, the rate halved, no new sequence.
            rate_of(&mut param, 5_000);
            param.NumExtParam = 2;
            // SAFETY: the buffers named are live for the call.
            unsafe { session.reset_encoder(&mut param) }.expect("a change of rate");
            let kind = encode_one(&session, &mut surface, &mut bitstream, false);
            assert_eq!(kind & keyframe, 0, "{name}: changed, type {kind:#x}");

            // SAFETY: the surface's texture and the bitstream's bytes outlive
            // the close, which the picture is done by.
            let left = unsafe { session.encode(&mut surface, &mut bitstream, false) };
            assert!(matches!(left, Ok(Encoded::Pending(_))), "{left:?}");
            session.close_encoder();
            assert_ne!(bitstream.DataLength, 0, "{name}: left unencoded");
            println!(
                "{name}: keyframe {} bytes, picture {} bytes",
                first.len(),
                second.len()
            );
        }
    }

    /// `param`'s target.
    fn target_of(param: &mfxVideoParam) -> u16 {
        // SAFETY: the union's encoder half, every field of it plain data.
        let rate = unsafe { param.__bindgen_anon_1.mfx.__bindgen_anon_1.__bindgen_anon_1 };
        // SAFETY: as above.
        unsafe { rate.__bindgen_anon_2.TargetKbps }
    }

    /// `param`'s target and peak, both at `kbps`.
    fn rate_of(param: &mut mfxVideoParam, kbps: u16) {
        // SAFETY: the union's encoder half, every field of it plain data.
        let rate = unsafe { &mut param.__bindgen_anon_1.mfx.__bindgen_anon_1.__bindgen_anon_1 };
        rate.__bindgen_anon_2.TargetKbps = kbps;
        rate.__bindgen_anon_3.MaxKbps = kbps;
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
