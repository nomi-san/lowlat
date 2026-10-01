//! The system's media framework and its own decoders, reached at run time
//! from the system's directory, driven in software alone.
//!
//! **Loaded once per process and never shut down.** The framework is started
//! once and the process's multithreaded apartment is kept alive with it, so
//! any thread that never initialised COM -- the library's own, or one of the
//! application's -- can make and drive a decoder, and no thread of the
//! application's is ever put into an apartment by the library. Shutting the
//! framework down while another thread enumerates its decoders is a race the
//! framework leaves to its callers; never shutting it down has none.
//!
//! **A decoder is the system's own for H.264**, picked by its class among
//! whatever the enumeration lists, and the first listed for HEVC, which is
//! the system's extension where it is installed: a package of the store, with
//! no class of its own, made only through what the enumeration hands back and
//! refused where the package has no licence. No device manager is ever given
//! to either, so both decode on the processor.

use core::ffi::c_void;
use core::fmt;
use core::mem::ManuallyDrop;
use std::sync::OnceLock;

use lowlat_common::dynlib::Library;

use crate::d3d11::Com;
use crate::ffi::mf::{
    GUID, HRESULT, ICodecAPI, IMFActivate, IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample,
    IMFTransform, IUnknown, MF_API_VERSION, MF_SDK_VERSION, MFNominalRange_0_255,
    MFSTARTUP_NOSOCKET, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFT_REGISTER_TYPE_INFO, MFVideoArea, VARIANT, VT_I4,
};
use crate::ffi::mf_guids::{
    CLSID_CMSH264DecoderMFT, CODECAPI_AVDecNumWorkerThreads, IID_ICodecAPI, IID_IMFTransform,
    MF_E_MEDIA_EXTENSION_PACKAGE_LICENSE_INVALID, MF_E_NO_MORE_TYPES, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_E_TRANSFORM_TYPE_NOT_SET,
    MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MFMediaType_Video,
    MFT_CATEGORY_VIDEO_DECODER, MFT_TRANSFORM_CLSID_Attribute, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoFormat_P010,
};
use crate::vcall;

type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type TransformEnum = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut *mut IMFActivate,
    *mut u32,
) -> HRESULT;
type CreateMediaType = unsafe extern "system" fn(*mut *mut IMFMediaType) -> HRESULT;
type CreateSample = unsafe extern "system" fn(*mut *mut IMFSample) -> HRESULT;
type CreateMemoryBuffer = unsafe extern "system" fn(u32, *mut *mut IMFMediaBuffer) -> HRESULT;
type IncrementMtaUsage = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type TaskMemFree = unsafe extern "system" fn(*mut c_void);
type VersionSize = unsafe extern "system" fn(*const u16, *mut u32) -> u32;
type VersionInfo = unsafe extern "system" fn(*const u16, u32, u32, *mut c_void) -> i32;
type VersionValue =
    unsafe extern "system" fn(*const c_void, *const u16, *mut *mut c_void, *mut u32) -> i32;

// The system's own module calls, present in every process.
unsafe extern "system" {
    fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
    fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
}

/// A setting's value tagged as a signed word, as the tag's field holds it.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the assertion bounds the value to the field"
)]
const SIGNED_WORD: u16 = {
    assert!(VT_I4 >= 0 && VT_I4 <= 0xffff);
    VT_I4 as u16
};

/// A module found by an address inside it, its count left alone.
const MODULE_FROM_ADDRESS: u32 = 0x4 | 0x2;

/// Why the framework or a decoder could not be had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The framework is not on this system -- an edition without its media
    /// features -- or would not start.
    Unavailable,
    /// Loaded, but missing an entry point or a table entry it must have.
    MissingSymbol,
    /// No decoder of this codec is installed.
    NoDecoder,
    /// The decoder is a package whose licence the system refused.
    Licence,
    /// The decoder offers no output in the layout asked for.
    NoLayout,
    /// A call failed, with its result.
    Status(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("system media framework not available"),
            Self::MissingSymbol => f.write_str("system media framework missing an entry point"),
            Self::NoDecoder => f.write_str("no system decoder for this codec"),
            Self::Licence => f.write_str("system decoder package not licensed"),
            Self::NoLayout => f.write_str("system decoder offers no such output"),
            Self::Status(s) => write!(f, "system media call returned 0x{s:08x}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

fn check(hr: HRESULT) -> Result<()> {
    if hr < 0 {
        Err(Error::Status(hr))
    } else {
        Ok(())
    }
}

// SAFETY: every structure this is used for is plain data the platform's
// headers define, whose all-zero value is valid and is where a fill starts.
fn zeroed<T>() -> T {
    unsafe { core::mem::zeroed() }
}

/// The codec a decoder is made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    H264,
    Hevc,
}

impl Coding {
    fn subtype(self) -> &'static GUID {
        match self {
            Self::H264 => &MFVideoFormat_H264,
            Self::Hevc => &MFVideoFormat_HEVC,
        }
    }
}

/// The framework, loaded and started for the process.
pub struct Mf {
    _mfplat: Library,
    _ole32: Library,
    version: Option<Library>,
    transform_enum: TransformEnum,
    media_type: CreateMediaType,
    sample: CreateSample,
    memory_buffer: CreateMemoryBuffer,
    task_free: TaskMemFree,
}

impl fmt::Debug for Mf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Mf")
    }
}

static MF: OnceLock<Result<Mf>> = OnceLock::new();

/// The framework, loaded and started the first time it is asked for and
/// kept for the process; why not, where it cannot be.
pub fn load() -> Result<&'static Mf> {
    MF.get_or_init(Mf::open).as_ref().map_err(|e| *e)
}

impl Mf {
    fn open() -> Result<Self> {
        let ole32 = Library::open_system(c"ole32.dll").ok_or(Error::Unavailable)?;
        let mfplat = Library::open_system(c"mfplat.dll").ok_or(Error::Unavailable)?;
        // SAFETY: each type is the platform's declaration of the entry point
        // of that name.
        let (increment, task_free, startup, transform_enum, media_type, sample, memory_buffer) = unsafe {
            (
                ole32.symbol::<IncrementMtaUsage>(c"CoIncrementMTAUsage"),
                ole32.symbol::<TaskMemFree>(c"CoTaskMemFree"),
                mfplat.symbol::<Startup>(c"MFStartup"),
                mfplat.symbol::<TransformEnum>(c"MFTEnumEx"),
                mfplat.symbol::<CreateMediaType>(c"MFCreateMediaType"),
                mfplat.symbol::<CreateSample>(c"MFCreateSample"),
                mfplat.symbol::<CreateMemoryBuffer>(c"MFCreateMemoryBuffer"),
            )
        };
        let missing = Error::MissingSymbol;
        let (increment, task_free, startup) = (
            increment.ok_or(missing)?,
            task_free.ok_or(missing)?,
            startup.ok_or(missing)?,
        );
        let mut cookie: *mut c_void = core::ptr::null_mut();
        // SAFETY: the output is a local. The usage is never given back: the
        // apartment lives as long as the process, as the framework does.
        check(unsafe { increment(&raw mut cookie) }).map_err(|_| Error::Unavailable)?;
        // SAFETY: the version the headers name and the light start, which
        // starts no network layer.
        check(unsafe { startup(MF_SDK_VERSION << 16 | MF_API_VERSION, MFSTARTUP_NOSOCKET) })
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            transform_enum: transform_enum.ok_or(missing)?,
            media_type: media_type.ok_or(missing)?,
            sample: sample.ok_or(missing)?,
            memory_buffer: memory_buffer.ok_or(missing)?,
            task_free,
            version: Library::open_system(c"version.dll"),
            _mfplat: mfplat,
            _ole32: ole32,
        })
    }

    /// The system's decoder for `coding`, made and not yet configured.
    pub fn decoder(&'static self, coding: Coding) -> Result<Transform> {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: *coding.subtype(),
        };
        let mut list: *mut *mut IMFActivate = core::ptr::null_mut();
        let mut count = 0u32;
        // Synchronous decoders of this process's registration and the
        // system's, in the system's order; never a hardware or an
        // asynchronous one.
        let flags =
            (MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER) as u32;
        // SAFETY: the outputs are locals; the list and its references are
        // the caller's, taken over below.
        check(unsafe {
            (self.transform_enum)(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                &raw const input,
                core::ptr::null(),
                &raw mut list,
                &raw mut count,
            )
        })?;
        let activates: Vec<Com<IMFActivate>> = (0..count as usize)
            .filter_map(|i| {
                // SAFETY: `count` entries, each a reference handed over.
                unsafe { Com::from_raw(*list.add(i)) }
            })
            .collect();
        if !list.is_null() {
            // SAFETY: the array the enumeration allocated, freed once; its
            // entries' references are held above.
            unsafe { (self.task_free)(list.cast()) };
        }
        let chosen = match coding {
            Coding::H264 => activates
                .into_iter()
                .find(|a| class_of(a).is_some_and(|c| same(&c, &CLSID_CMSH264DecoderMFT))),
            Coding::Hevc => activates.into_iter().next(),
        }
        .ok_or(Error::NoDecoder)?;
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live activation object; the output is a local.
        let hr = unsafe {
            vcall!(
                chosen.as_ptr(),
                ActivateObject,
                &IID_IMFTransform,
                &raw mut raw
            )
        }
        .ok_or(Error::MissingSymbol)?;
        if hr == MF_E_MEDIA_EXTENSION_PACKAGE_LICENSE_INVALID {
            return Err(Error::Licence);
        }
        check(hr)?;
        // SAFETY: the reference the activation handed over.
        let transform =
            unsafe { Com::from_raw(raw.cast::<IMFTransform>()) }.ok_or(Error::Status(hr))?;
        Ok(Transform {
            transform: ManuallyDrop::new(transform),
            activate: chosen,
            coding,
            mf: self,
        })
    }

    /// A sample over one buffer of `capacity` bytes, for a decoder's input or
    /// output.
    pub fn sample(&self, capacity: u32) -> Result<Sample> {
        let mut buffer: *mut IMFMediaBuffer = core::ptr::null_mut();
        // SAFETY: the output is a local.
        check(unsafe { (self.memory_buffer)(capacity, &raw mut buffer) })?;
        // SAFETY: the reference the call handed over.
        let buffer = unsafe { Com::from_raw(buffer) }.ok_or(Error::Unavailable)?;
        let mut sample: *mut IMFSample = core::ptr::null_mut();
        // SAFETY: as above.
        check(unsafe { (self.sample)(&raw mut sample) })?;
        // SAFETY: as above.
        let sample = unsafe { Com::from_raw(sample) }.ok_or(Error::Unavailable)?;
        // SAFETY: both live; the sample takes a reference of its own.
        check(
            unsafe { vcall!(sample.as_ptr(), AddBuffer, buffer.as_ptr()) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        Ok(Sample {
            sample,
            buffer,
            capacity,
        })
    }

    fn media_type(&self) -> Result<Com<IMFMediaType>> {
        let mut raw: *mut IMFMediaType = core::ptr::null_mut();
        // SAFETY: the output is a local.
        check(unsafe { (self.media_type)(&raw mut raw) })?;
        // SAFETY: the reference the call handed over.
        unsafe { Com::from_raw(raw) }.ok_or(Error::Unavailable)
    }

    /// The file version of the module `address` is in, as its four parts.
    fn version_at(&self, address: *const c_void) -> Option<[u16; 4]> {
        let version = self.version.as_ref()?;
        // SAFETY: the platform's declarations of the entry points.
        let (size, info, value) = unsafe {
            (
                version.symbol::<VersionSize>(c"GetFileVersionInfoSizeW")?,
                version.symbol::<VersionInfo>(c"GetFileVersionInfoW")?,
                version.symbol::<VersionValue>(c"VerQueryValueW")?,
            )
        };
        let mut module: *mut c_void = core::ptr::null_mut();
        // SAFETY: an address inside a loaded module; the output is a local
        // and the module's count is left alone.
        if unsafe { GetModuleHandleExW(MODULE_FROM_ADDRESS, address.cast(), &raw mut module) } == 0
        {
            return None;
        }
        let mut path = [0u16; 1024];
        // SAFETY: a loaded module; the buffer's length is passed.
        let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), 1024) };
        if len == 0 || len >= 1024 {
            return None;
        }
        let mut ignored = 0u32;
        // SAFETY: a NUL-terminated path; the output is a local.
        let bytes = unsafe { size(path.as_ptr(), &raw mut ignored) };
        if bytes == 0 {
            return None;
        }
        let mut block = vec![0u8; bytes as usize];
        // SAFETY: the block is `bytes` long, as the size asked says.
        if unsafe { info(path.as_ptr(), 0, bytes, block.as_mut_ptr().cast()) } == 0 {
            return None;
        }
        let root = [u16::from(b'\\'), 0];
        let mut fixed: *mut c_void = core::ptr::null_mut();
        let mut fixed_len = 0u32;
        // SAFETY: the block filled above; the outputs are locals, the pointer
        // into the block.
        if unsafe {
            value(
                block.as_ptr().cast(),
                root.as_ptr(),
                &raw mut fixed,
                &raw mut fixed_len,
            )
        } == 0
            || fixed.is_null()
            || fixed_len < 16
        {
            return None;
        }
        // SAFETY: the fixed part's file version, two words at eight and
        // twelve bytes, inside the block.
        let (high, low) = unsafe {
            (
                fixed.cast::<u8>().add(8).cast::<u32>().read_unaligned(),
                fixed.cast::<u8>().add(12).cast::<u32>().read_unaligned(),
            )
        };
        #[expect(clippy::cast_possible_truncation, reason = "halves of a word")]
        Some([
            (high >> 16) as u16,
            high as u16,
            (low >> 16) as u16,
            low as u16,
        ])
    }
}

/// Whether two identifiers are one.
fn same(a: &GUID, b: &GUID) -> bool {
    (a.Data1, a.Data2, a.Data3, a.Data4) == (b.Data1, b.Data2, b.Data3, b.Data4)
}

/// The class an activation object makes, where it names one.
fn class_of(activate: &Com<IMFActivate>) -> Option<GUID> {
    let mut class: GUID = zeroed();
    // SAFETY: a live activation object, whose table begins with the
    // attributes'; the output is a local.
    let hr = unsafe {
        vcall!(
            activate.as_ptr().cast::<IMFAttributes>(),
            GetGUID,
            &MFT_TRANSFORM_CLSID_Attribute,
            &raw mut class
        )
    }?;
    (hr >= 0).then_some(class)
}

/// The layout a decoder hands its pictures out in, once its output is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The coded size: what the rows and the chroma's start follow.
    pub width: u32,
    pub height: u32,
    /// The bytes a row takes.
    pub stride: u32,
    /// The visible part: its offset and size.
    pub visible: (u32, u32, u32, u32),
    /// Whether the decoder says the samples are the full range.
    pub full_range: bool,
    /// Whether the output is ten bits in sixteen, rather than eight.
    pub ten_bit: bool,
    /// The bytes an output buffer must hold.
    pub size: u32,
}

/// What an output call amounted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Out {
    /// A picture, in the sample handed in.
    Picture,
    /// Nothing until more input.
    NeedInput,
    /// The stream's format changed: the output is to be set again, and the
    /// call made again.
    Changed,
}

/// One of the system's decoders, released and shut down on drop.
pub struct Transform {
    transform: ManuallyDrop<Com<IMFTransform>>,
    activate: Com<IMFActivate>,
    coding: Coding,
    mf: &'static Mf,
}

impl fmt::Debug for Transform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transform")
            .field("coding", &self.coding)
            .finish()
    }
}

impl Drop for Transform {
    fn drop(&mut self) {
        // SAFETY: dropped once, here, and never used after.
        unsafe { ManuallyDrop::drop(&mut self.transform) };
        // SAFETY: a live activation object, whose decoder is released above.
        unsafe { vcall!(self.activate.as_ptr(), ShutdownObject) };
    }
}

impl Transform {
    fn raw(&self) -> *mut IMFTransform {
        self.transform.as_ptr()
    }

    pub fn coding(&self) -> Coding {
        self.coding
    }

    fn attributes(&self) -> Result<Com<IMFAttributes>> {
        let mut raw: *mut IMFAttributes = core::ptr::null_mut();
        // SAFETY: a live decoder; the output is a local.
        check(
            unsafe { vcall!(self.raw(), GetAttributes, &raw mut raw) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: the reference the call handed over.
        unsafe { Com::from_raw(raw) }.ok_or(Error::MissingSymbol)
    }

    /// Each picture out as soon as it is decodable, never held for the
    /// stream's reorder depth to fill.
    pub fn low_latency(&self) -> Result<()> {
        let attributes = self.attributes()?;
        // SAFETY: a live attribute store.
        check(
            unsafe { vcall!(attributes.as_ptr(), SetUINT32, &MF_LOW_LATENCY, 1) }
                .ok_or(Error::MissingSymbol)?,
        )
    }

    /// The worker threads the decoder starts: through its codec interface
    /// where it has one, which takes the count as a signed word alone, and
    /// as an attribute where it has none.
    pub fn workers(&self, count: u32) -> Result<()> {
        let mut api: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live decoder, asked through the base's table; the output
        // is a local.
        let hr = unsafe {
            vcall!(
                self.raw().cast::<IUnknown>(),
                QueryInterface,
                &IID_ICodecAPI,
                &raw mut api
            )
        }
        .ok_or(Error::MissingSymbol)?;
        // SAFETY: the reference the call handed over, where it succeeded.
        match unsafe { Com::from_raw(api.cast::<ICodecAPI>()) }.filter(|_| hr >= 0) {
            Some(api) => {
                let mut value: VARIANT = zeroed();
                // SAFETY: the value's tagged half, plain data all zero until
                // filled here.
                let inner = unsafe { &mut value.__bindgen_anon_1.__bindgen_anon_1 };
                inner.vt = SIGNED_WORD;
                inner.__bindgen_anon_1.lVal = i32::try_from(count).map_err(|_| Error::Status(0))?;
                // SAFETY: a live codec interface; the value is a local.
                check(
                    unsafe {
                        vcall!(
                            api.as_ptr(),
                            SetValue,
                            &CODECAPI_AVDecNumWorkerThreads,
                            &raw mut value
                        )
                    }
                    .ok_or(Error::MissingSymbol)?,
                )
            }
            None => {
                let attributes = self.attributes()?;
                // SAFETY: a live attribute store.
                check(
                    unsafe {
                        vcall!(
                            attributes.as_ptr(),
                            SetUINT32,
                            &CODECAPI_AVDecNumWorkerThreads,
                            count
                        )
                    }
                    .ok_or(Error::MissingSymbol)?,
                )
            }
        }
    }

    /// The input: the codec, at `width` by `height`, which a decoder that
    /// needs a size takes and one that does not corrects at its first
    /// picture.
    pub fn set_input(&self, width: u32, height: u32) -> Result<()> {
        let media = self.mf.media_type()?;
        let m = media.as_ptr().cast::<IMFAttributes>();
        // SAFETY: a live media type, whose table begins with the attributes'.
        unsafe {
            check(
                vcall!(m, SetGUID, &MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .ok_or(Error::MissingSymbol)?,
            )?;
            check(
                vcall!(m, SetGUID, &MF_MT_SUBTYPE, self.coding.subtype())
                    .ok_or(Error::MissingSymbol)?,
            )?;
            let size = u64::from(width) << 32 | u64::from(height);
            check(vcall!(m, SetUINT64, &MF_MT_FRAME_SIZE, size).ok_or(Error::MissingSymbol)?)?;
        }
        // SAFETY: a live decoder and media type.
        check(
            unsafe { vcall!(self.raw(), SetInputType, 0, media.as_ptr(), 0) }
                .ok_or(Error::MissingSymbol)?,
        )
    }

    /// The output, eight-bit 4:2:0 or ten-bit, as the decoder offers it for
    /// the stream it has seen; its layout. A decoder offering neither is
    /// refused.
    pub fn set_output(&self, ten_bit: bool) -> Result<Layout> {
        let wanted = if ten_bit {
            &MFVideoFormat_P010
        } else {
            &MFVideoFormat_NV12
        };
        let mut index = 0u32;
        let chosen = loop {
            let mut raw: *mut IMFMediaType = core::ptr::null_mut();
            // SAFETY: a live decoder; the output is a local.
            let hr = unsafe { vcall!(self.raw(), GetOutputAvailableType, 0, index, &raw mut raw) }
                .ok_or(Error::MissingSymbol)?;
            if hr == MF_E_NO_MORE_TYPES {
                return Err(Error::NoLayout);
            }
            check(hr)?;
            // SAFETY: the reference the call handed over.
            let offered = unsafe { Com::from_raw(raw) }.ok_or(Error::NoLayout)?;
            if guid_of(&offered, &MF_MT_SUBTYPE).is_some_and(|g| same(&g, wanted)) {
                break offered;
            }
            index += 1;
        };
        // SAFETY: a live decoder and media type.
        check(
            unsafe { vcall!(self.raw(), SetOutputType, 0, chosen.as_ptr(), 0) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        self.layout()
    }

    /// The layout of the output set.
    fn layout(&self) -> Result<Layout> {
        let mut raw: *mut IMFMediaType = core::ptr::null_mut();
        // SAFETY: a live decoder; the output is a local.
        check(
            unsafe { vcall!(self.raw(), GetOutputCurrentType, 0, &raw mut raw) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        // SAFETY: the reference the call handed over.
        let current = unsafe { Com::from_raw(raw) }.ok_or(Error::NoLayout)?;
        let m = current.as_ptr().cast::<IMFAttributes>();
        let mut size = 0u64;
        let mut stride = 0u32;
        let mut range = 0u32;
        let mut area: MFVideoArea = zeroed();
        let area_bytes = u32::try_from(size_of::<MFVideoArea>()).unwrap_or(16);
        // SAFETY: a live media type; the outputs are locals.
        let (has_stride, has_area, has_range) = unsafe {
            check(
                vcall!(m, GetUINT64, &MF_MT_FRAME_SIZE, &raw mut size)
                    .ok_or(Error::MissingSymbol)?,
            )?;
            (
                vcall!(m, GetUINT32, &MF_MT_DEFAULT_STRIDE, &raw mut stride)
                    .is_some_and(|hr| hr >= 0),
                vcall!(
                    m,
                    GetBlob,
                    &MF_MT_MINIMUM_DISPLAY_APERTURE,
                    (&raw mut area).cast::<u8>(),
                    area_bytes,
                    core::ptr::null_mut()
                )
                .is_some_and(|hr| hr >= 0),
                vcall!(m, GetUINT32, &MF_MT_VIDEO_NOMINAL_RANGE, &raw mut range)
                    .is_some_and(|hr| hr >= 0),
            )
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the two halves of the size"
        )]
        let (width, height) = ((size >> 32) as u32, size as u32);
        let mut info: MFT_OUTPUT_STREAM_INFO = zeroed();
        // SAFETY: a live decoder; the output is a local.
        check(
            unsafe { vcall!(self.raw(), GetOutputStreamInfo, 0, &raw mut info) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        let sample =
            if guid_of(&current, &MF_MT_SUBTYPE).is_some_and(|g| same(&g, &MFVideoFormat_P010)) {
                2
            } else {
                1
            };
        // A negative stride is a picture stored bottom up, which no decoder
        // here writes; the width's own bytes where none is said.
        let stride = if has_stride {
            i32::from_ne_bytes(stride.to_ne_bytes()).unsigned_abs()
        } else {
            width * sample
        };
        let visible = if has_area {
            let at = |o: crate::ffi::mf::MFOffset| u32::try_from(o.value).unwrap_or(0);
            (
                at(area.OffsetX),
                at(area.OffsetY),
                u32::try_from(area.Area.cx).unwrap_or(0),
                u32::try_from(area.Area.cy).unwrap_or(0),
            )
        } else {
            (0, 0, width, height)
        };
        Ok(Layout {
            width,
            height,
            stride,
            visible,
            full_range: has_range && range == MFNominalRange_0_255 as u32,
            ten_bit: sample == 2,
            size: info.cbSize,
        })
    }

    /// Start the decoder's streaming -- its workers start here, rather than
    /// in the first picture's call. Only once an output is set: the HEVC
    /// extension faults on it before.
    pub fn begin(&self) -> Result<()> {
        // SAFETY: a live decoder.
        check(
            unsafe {
                vcall!(
                    self.raw(),
                    ProcessMessage,
                    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
                    0
                )
            }
            .ok_or(Error::MissingSymbol)?,
        )
    }

    /// Let every picture held for the stream's order out, as at the
    /// stream's end: a test's need, since a live stream never ends this way
    /// and a decoder drained takes no more input.
    pub fn drain(&self) -> Result<()> {
        // SAFETY: a live decoder.
        check(
            unsafe { vcall!(self.raw(), ProcessMessage, MFT_MESSAGE_COMMAND_DRAIN, 0) }
                .ok_or(Error::MissingSymbol)?,
        )
    }

    /// Hand the decoder `sample`'s unit. `false` where it takes no more
    /// until its output is taken.
    pub fn input(&self, sample: &Sample) -> Result<bool> {
        // SAFETY: a live decoder and sample.
        let hr = unsafe { vcall!(self.raw(), ProcessInput, 0, sample.sample.as_ptr(), 0) }
            .ok_or(Error::MissingSymbol)?;
        if hr == MF_E_NOTACCEPTING {
            return Ok(false);
        }
        check(hr).map(|()| true)
    }

    /// Take a picture out into `sample`, whose length is cleared first.
    pub fn output(&self, sample: &Sample) -> Result<Out> {
        sample.clear()?;
        let mut buffer: MFT_OUTPUT_DATA_BUFFER = zeroed();
        buffer.pSample = sample.sample.as_ptr();
        let mut status = 0u32;
        // SAFETY: a live decoder; the structure and status are locals, the
        // sample live.
        let hr = unsafe {
            vcall!(
                self.raw(),
                ProcessOutput,
                0,
                1,
                &raw mut buffer,
                &raw mut status
            )
        }
        .ok_or(Error::MissingSymbol)?;
        if !buffer.pEvents.is_null() {
            // SAFETY: a reference the call handed over, released once.
            unsafe { vcall!(buffer.pEvents.cast::<IUnknown>(), Release) };
        }
        if !buffer.pSample.is_null() && buffer.pSample != sample.sample.as_ptr() {
            // A sample of the decoder's own, which a decoder in software
            // never hands out: given back.
            // SAFETY: a reference the call handed over, released once.
            unsafe { vcall!(buffer.pSample.cast::<IUnknown>(), Release) };
            return Err(Error::NoLayout);
        }
        match hr {
            MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Out::NeedInput),
            MF_E_TRANSFORM_STREAM_CHANGE | MF_E_TRANSFORM_TYPE_NOT_SET => Ok(Out::Changed),
            _ => check(hr).map(|()| Out::Picture),
        }
    }

    /// The version of the module the decoder's code is in, as its four
    /// parts.
    pub fn version(&self) -> Option<[u16; 4]> {
        // SAFETY: a live decoder, whose table's entries are its own code.
        let entry = unsafe { (*(*self.raw()).lpVtbl).ProcessOutput }?;
        self.mf.version_at(entry as *const c_void)
    }
}

/// A media type's GUID attribute.
fn guid_of(media: &Com<IMFMediaType>, key: &GUID) -> Option<GUID> {
    let mut value: GUID = zeroed();
    // SAFETY: a live media type, whose table begins with the attributes';
    // the output is a local.
    let hr = unsafe {
        vcall!(
            media.as_ptr().cast::<IMFAttributes>(),
            GetGUID,
            key,
            &raw mut value
        )
    }?;
    (hr >= 0).then_some(value)
}

/// A sample over one buffer of the framework's, made once and used again
/// for every unit or picture.
pub struct Sample {
    sample: Com<IMFSample>,
    buffer: Com<IMFMediaBuffer>,
    capacity: u32,
}

impl fmt::Debug for Sample {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sample")
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl Sample {
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Set the buffer's length to nothing: what a decoder writing into it
    /// expects of a buffer used again.
    pub fn clear(&self) -> Result<()> {
        // SAFETY: a live buffer.
        check(
            unsafe { vcall!(self.buffer.as_ptr(), SetCurrentLength, 0) }
                .ok_or(Error::MissingSymbol)?,
        )
    }

    /// Fill the buffer with `parts`, one after another; refused where they
    /// do not fit.
    pub fn write(&self, parts: &[&[u8]]) -> Result<()> {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let total32 = u32::try_from(total).map_err(|_| Error::Status(0))?;
        if total32 > self.capacity {
            return Err(Error::Status(0));
        }
        self.locked(|bytes| {
            let mut at = 0;
            for part in parts {
                if let Some(to) = bytes.get_mut(at..at + part.len()) {
                    to.copy_from_slice(part);
                }
                at += part.len();
            }
        })?;
        // SAFETY: a live buffer; the length is within its capacity.
        check(
            unsafe { vcall!(self.buffer.as_ptr(), SetCurrentLength, total32) }
                .ok_or(Error::MissingSymbol)?,
        )
    }

    /// Read the buffer's bytes, as long as it says it holds.
    pub fn read<R>(&self, read: impl FnOnce(&[u8]) -> R) -> Result<R> {
        let mut current = 0u32;
        // SAFETY: a live buffer; the output is a local.
        check(
            unsafe { vcall!(self.buffer.as_ptr(), GetCurrentLength, &raw mut current) }
                .ok_or(Error::MissingSymbol)?,
        )?;
        self.locked(|bytes| read(bytes.get(..current as usize).unwrap_or(bytes)))
    }

    /// The buffer's whole capacity, locked for the closure.
    fn locked<R>(&self, with: impl FnOnce(&mut [u8]) -> R) -> Result<R> {
        let mut data: *mut u8 = core::ptr::null_mut();
        let mut most = 0u32;
        // SAFETY: a live buffer; the outputs are locals.
        check(
            unsafe {
                vcall!(
                    self.buffer.as_ptr(),
                    Lock,
                    &raw mut data,
                    &raw mut most,
                    core::ptr::null_mut()
                )
            }
            .ok_or(Error::MissingSymbol)?,
        )?;
        let result = if data.is_null() {
            Err(Error::Status(0))
        } else {
            // SAFETY: the buffer's memory, `most` bytes, locked for this
            // borrow and written by nothing else while it is.
            Ok(with(unsafe {
                core::slice::from_raw_parts_mut(data, most as usize)
            }))
        };
        // SAFETY: locked above, unlocked once.
        unsafe { vcall!(self.buffer.as_ptr(), Unlock) };
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The framework loads and both decoders are made where installed**:
    /// the system's H.264 decoder on every edition that has the framework,
    /// and the HEVC extension, licensed, here; each takes its settings, its
    /// input and an output of eight bits, and names its module's version.
    #[test]
    #[ignore = "requires the system's media framework, and its HEVC extension"]
    fn both_decoders_are_made_and_configured() {
        let mf = load().expect("the framework");
        for coding in [Coding::H264, Coding::Hevc] {
            let decoder = mf.decoder(coding).expect("a decoder");
            decoder.low_latency().expect("low latency");
            decoder.workers(2).expect("workers");
            decoder.set_input(1920, 1080).expect("input");
            let layout = decoder.set_output(false).expect("an eight-bit output");
            decoder.begin().expect("streaming");
            let version = decoder.version().expect("a version");
            println!("  {coding:?}: {layout:?}, version {version:?}");
            assert_eq!((layout.width, layout.stride), (1920, 1920));
        }
    }

    /// A sample takes a unit in parts and reads it back whole; one too large
    /// is refused.
    #[test]
    #[ignore = "requires the system's media framework"]
    fn a_sample_holds_its_parts() {
        let mf = load().expect("the framework");
        let sample = mf.sample(8).expect("a sample");
        sample.write(&[&[1, 2, 3], &[4, 5]]).expect("write");
        assert_eq!(sample.read(<[u8]>::to_vec).expect("read"), [1, 2, 3, 4, 5]);
        assert!(sample.write(&[&[0; 9]]).is_err());
        sample.clear().expect("clear");
        assert_eq!(sample.read(<[u8]>::len).expect("read"), 0);
    }
}
