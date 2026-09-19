//! The vendor's decode interface, opened at runtime beside the compute
//! runtime it decodes through.
//!
//! Only the decoder half is resolved: create, destroy, decode one picture,
//! map and unmap a decoded one, and the capability query. The interface's
//! own bitstream parser is not used, because the readers above produce the
//! picture parameters themselves and one reader serves both backends.

use core::ffi::{CStr, c_int, c_uint};

use lowlat_common::dynlib::Library;

use crate::cuda;
use crate::ffi::cuda::{CUDA_SUCCESS, CUresult};
use crate::ffi::cuvid::{
    CUVIDDECODECAPS, CUVIDDECODECREATEINFO, CUVIDPICPARAMS, CUVIDPROCPARAMS, CUvideodecoder,
};

/// Versioned first, as with the compute runtime.
const SONAMES: [&CStr; 2] = [c"libnvcuvid.so.1", c"libnvcuvid.so"];

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
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl Cuvid {
    /// Open the interface. The compute runtime must already be loaded and
    /// initialised; every call below is made against the context current on
    /// the calling thread.
    pub fn load() -> Result<Self> {
        let library = Library::open_first(&SONAMES).ok_or(Error::Unavailable)?;
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
                _library: library,
            }
        };
        Ok(loaded)
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
}

impl Drop for Decoder<'_> {
    fn drop(&mut self) {
        // SAFETY: created once, destroyed once; the type is neither `Copy`
        // nor `Clone`.
        unsafe { (self.cuvid.destroy_decoder)(self.raw) };
    }
}
