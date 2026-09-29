//! The vendor's decode interface, opened at runtime beside the compute
//! runtime it decodes through.
//!
//! Only the decoder half is resolved: create, destroy, decode one picture,
//! map and unmap a decoded one, and the capability query. The interface's
//! own bitstream parser is not used, because the readers above produce the
//! picture parameters themselves and one reader serves both backends.
//!
//! **Where the driver has them, a decoder decodes into surfaces of the
//! caller's own and in the order of a stream** (the interface's 13.1): the
//! caller registers arrays it made, and each picture's decode is queued on
//! a stream, so work queued behind it on that stream sees the picture and
//! nothing on the calling thread waits for it. No map, no unmap.

use core::ffi::{CStr, c_int, c_uint, c_void};

use lowlat_common::dynlib::Library;

use crate::cuda;
use crate::ffi::cuda::{CUDA_SUCCESS, CUarray, CUdeviceptr, CUresult, CUstream};
use crate::ffi::cuvid::{
    CUVIDDECODECAPS, CUVIDDECODECREATEINFO, CUVIDPICPARAMS, CUVIDPROCPARAMS, CUvideodecoder,
    cudaVideoSurfaceFormat,
};

/// The output layouts of a decoder decoding into the caller's own surfaces,
/// beside the vendored header's four: the same layouts, in arrays.
pub mod opaque {
    use super::cudaVideoSurfaceFormat;

    pub const NV12: cudaVideoSurfaceFormat = 6;
    pub const P016: cudaVideoSurfaceFormat = 7;
    pub const YUV444: cudaVideoSurfaceFormat = 8;
    pub const YUV444_16BIT: cudaVideoSurfaceFormat = 9;
}

/// The most surfaces a decoder takes of the caller's own.
pub const MAX_REGISTERED_SURFACES: usize = 32;

/// The caller's own surfaces, as a decoder is told of them. Declared here
/// because the vendored header predates it; the layout is the 13.1 header's,
/// asserted below.
#[repr(C)]
struct RegisterDecodeSurfacesInfo {
    count: c_uint,
    reserved: [c_uint; 31],
    surfaces: *mut CUarray,
    stats: *mut CUdeviceptr,
    reserved_pointers: [*mut c_void; 30],
}

const _: () = assert!(core::mem::size_of::<RegisterDecodeSurfacesInfo>() == 384);
const _: () = assert!(core::mem::offset_of!(RegisterDecodeSurfacesInfo, surfaces) == 128);

/// Versioned first, as with the compute runtime.
#[cfg(unix)]
const SONAMES: &[&CStr] = &[c"libnvcuvid.so.1", c"libnvcuvid.so"];
/// Installed with the display driver, in the system directory.
#[cfg(windows)]
const SONAMES: &[&CStr] = &[c"nvcuvid.dll"];

type GetDecoderCaps = unsafe extern "C" fn(*mut CUVIDDECODECAPS) -> CUresult;
type CreateDecoder =
    unsafe extern "C" fn(*mut CUvideodecoder, *mut CUVIDDECODECREATEINFO) -> CUresult;
type DestroyDecoder = unsafe extern "C" fn(CUvideodecoder) -> CUresult;
type DecodePicture = unsafe extern "C" fn(CUvideodecoder, *mut CUVIDPICPARAMS) -> CUresult;
type MapVideoFrame64 = unsafe extern "C" fn(
    CUvideodecoder,
    c_int,
    *mut u64,
    *mut c_uint,
    *mut CUVIDPROCPARAMS,
) -> CUresult;
type UnmapVideoFrame64 = unsafe extern "C" fn(CUvideodecoder, u64) -> CUresult;
type RegisterDecodeSurfaces =
    unsafe extern "C" fn(CUvideodecoder, *mut RegisterDecodeSurfacesInfo) -> CUresult;
type DecodePictureAsync =
    unsafe extern "C" fn(CUvideodecoder, *mut CUVIDPICPARAMS, CUstream) -> CUresult;

/// Why the interface could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No such library, which is the ordinary case without the driver.
    Unavailable,
    /// Loaded, but missing an entry point it must export.
    MissingSymbol,
    /// The compute runtime beneath it.
    Cuda(cuda::Error),
    /// A call failed, with its status.
    Status(CUresult),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("decode interface not available"),
            Self::MissingSymbol => f.write_str("decode interface is missing an entry point"),
            Self::Cuda(e) => write!(f, "{e}"),
            Self::Status(s) => write!(f, "decode interface returned status {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<cuda::Error> for Error {
    fn from(e: cuda::Error) -> Self {
        Self::Cuda(e)
    }
}

type Result<T> = core::result::Result<T, Error>;

fn check(status: CUresult) -> Result<()> {
    if status == CUDA_SUCCESS {
        Ok(())
    } else {
        Err(Error::Status(status))
    }
}

/// The loaded decode interface.
#[derive(Debug)]
pub struct Cuvid {
    get_decoder_caps: GetDecoderCaps,
    create_decoder: CreateDecoder,
    destroy_decoder: DestroyDecoder,
    decode_picture: DecodePicture,
    map_video_frame: MapVideoFrame64,
    unmap_video_frame: UnmapVideoFrame64,
    /// Decoding into the caller's own surfaces, in a stream's order: both
    /// or neither, from the 13.1 interface's driver on.
    register_decode_surfaces: Option<RegisterDecodeSurfaces>,
    decode_picture_async: Option<DecodePictureAsync>,
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl Cuvid {
    /// Open the interface. The compute runtime must already be loaded and
    /// initialised; every call below is made against the context current on
    /// the calling thread.
    pub fn load() -> Result<Self> {
        let library = Library::open_first(SONAMES).ok_or(Error::Unavailable)?;
        // SAFETY: every signature is transcribed from the vendored header.
        let loaded = unsafe {
            Self {
                get_decoder_caps: library
                    .symbol(c"cuvidGetDecoderCaps")
                    .ok_or(Error::MissingSymbol)?,
                create_decoder: library
                    .symbol(c"cuvidCreateDecoder")
                    .ok_or(Error::MissingSymbol)?,
                destroy_decoder: library
                    .symbol(c"cuvidDestroyDecoder")
                    .ok_or(Error::MissingSymbol)?,
                decode_picture: library
                    .symbol(c"cuvidDecodePicture")
                    .ok_or(Error::MissingSymbol)?,
                map_video_frame: library
                    .symbol(c"cuvidMapVideoFrame64")
                    .ok_or(Error::MissingSymbol)?,
                unmap_video_frame: library
                    .symbol(c"cuvidUnmapVideoFrame64")
                    .ok_or(Error::MissingSymbol)?,
                register_decode_surfaces: library.symbol(c"cuvidRegisterDecodeSurfaces"),
                decode_picture_async: library.symbol(c"cuvidDecodePictureAsync"),
                _library: library,
            }
        };
        Ok(loaded)
    }

    /// Whether this driver decodes into the caller's own surfaces, in a
    /// stream's order.
    pub fn asynchronous(&self) -> bool {
        self.register_decode_surfaces.is_some() && self.decode_picture_async.is_some()
    }

    /// What the device says about one codec, chroma and depth: the size
    /// limits and the output layouts. **Not a probe**: the answer has been
    /// known to say yes to a combination the device then failed to create,
    /// so a capability is decided by [`Self::create`] succeeding.
    pub fn caps(&self, caps: &mut CUVIDDECODECAPS) -> Result<()> {
        // SAFETY: a live structure the interface fills.
        check(unsafe { (self.get_decoder_caps)(caps) })
    }

    /// Create a decoder from filled creation info.
    pub fn create(&self, info: &mut CUVIDDECODECREATEINFO) -> Result<Decoder<'_>> {
        let mut raw: CUvideodecoder = core::ptr::null_mut();
        // SAFETY: both pointers are to live locals for the call.
        check(unsafe { (self.create_decoder)(&raw mut raw, info) })?;
        Ok(Decoder { cuvid: self, raw })
    }
}

/// One decoder: destroyed with it.
#[derive(Debug)]
pub struct Decoder<'a> {
    cuvid: &'a Cuvid,
    raw: CUvideodecoder,
}

impl Decoder<'_> {
    /// Submit one picture's parameters and slice data. Asynchronous: the
    /// picture is decoded into the surface `CurrPicIdx` names, and the map
    /// waits for it.
    pub fn decode(&self, params: &mut CUVIDPICPARAMS) -> Result<()> {
        // SAFETY: the decoder is live; the parameters are a live structure
        // whose bitstream and offset pointers the caller keeps valid for the
        // call, after which the interface holds no reference to them.
        check(unsafe { (self.cuvid.decode_picture)(self.raw, params) })
    }

    /// Map a decoded surface as a device pointer with its pitch, waiting for
    /// the decode to finish. Unmapped by [`Self::unmap`], and no more than
    /// the created count may be mapped at once.
    pub fn map(&self, picture: c_int, params: &mut CUVIDPROCPARAMS) -> Result<(u64, usize)> {
        let mut ptr: u64 = 0;
        let mut pitch: c_uint = 0;
        // SAFETY: the decoder is live; the out pointers are live locals.
        check(unsafe {
            (self.cuvid.map_video_frame)(self.raw, picture, &raw mut ptr, &raw mut pitch, params)
        })?;
        Ok((ptr, usize::try_from(pitch).unwrap_or(0)))
    }

    pub fn unmap(&self, ptr: u64) -> Result<()> {
        // SAFETY: the pointer came from `map` on this decoder.
        check(unsafe { (self.cuvid.unmap_video_frame)(self.raw, ptr) })
    }

    /// Hand the decoder the surfaces it decodes into, in the order a
    /// picture's surface index counts them. For a decoder created for the
    /// caller's own surfaces, once, before its first picture; the arrays
    /// must outlive it.
    pub fn register(&self, surfaces: &[cuda::Array]) -> Result<()> {
        let register = self
            .cuvid
            .register_decode_surfaces
            .ok_or(Error::MissingSymbol)?;
        let mut list = [core::ptr::null_mut(); MAX_REGISTERED_SURFACES];
        let listed = list.get_mut(..surfaces.len()).ok_or(Error::Status(0))?;
        for (slot, surface) in listed.iter_mut().zip(surfaces) {
            *slot = surface.raw();
        }
        // SAFETY: plain data; every field not set below is zero, as the
        // interface asks of its reserved fields.
        let mut info: RegisterDecodeSurfacesInfo = unsafe { core::mem::zeroed() };
        info.count = c_uint::try_from(surfaces.len()).map_err(|_| Error::Status(0))?;
        info.surfaces = list.as_mut_ptr();
        // SAFETY: the decoder is live; the list is live for the call, which
        // reads the handles and keeps none of the list itself.
        check(unsafe { register(self.raw, &raw mut info) })
    }

    /// As [`Self::decode`], queued on `stream` for a decoder given surfaces
    /// of its own: the picture is in its surface for whatever is queued on
    /// `stream` after this, and the calling thread waits for nothing.
    pub fn decode_async(&self, params: &mut CUVIDPICPARAMS, stream: &cuda::Stream) -> Result<()> {
        let decode = self
            .cuvid
            .decode_picture_async
            .ok_or(Error::MissingSymbol)?;
        // SAFETY: as `decode`; the stream belongs to the current context.
        check(unsafe { decode(self.raw, params, stream.raw()) })
    }
}

impl Drop for Decoder<'_> {
    fn drop(&mut self) {
        // SAFETY: created once, destroyed once; the type is neither `Copy`
        // nor `Clone`.
        unsafe { (self.cuvid.destroy_decoder)(self.raw) };
    }
}
