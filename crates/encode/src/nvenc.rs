//! Loading the vendor encoder runtime.
//!
//! The library is opened at runtime rather than linked, so a machine without
//! the driver reports a missing backend instead of failing to start
//! (docs/07-platforms.md section 8).
//!
//! **The version check is the point of this module.** Every structure the
//! interface takes carries a stamp of the header it was compiled against, and
//! the compatibility runs one way: a newer driver accepts an older stamp, an
//! older driver rejects a newer one. It rejects it on *every* call, with a
//! status that says only "invalid version" and names neither number. So the
//! check happens once, here, where both numbers are in hand and the failure can
//! say what it actually is.

use core::ffi::CStr;
use core::mem::MaybeUninit;

use lowlat_common::dynlib::Library;

use crate::ffi::nvenc::{
    NV_ENC_SUCCESS, NV_ENCODE_API_FUNCTION_LIST, NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION,
    NVENCSTATUS,
};
use crate::ffi::versions::NV_ENCODE_API_FUNCTION_LIST_VER;

/// Versioned first. The unversioned alias belongs to the development package
/// and is absent on a machine that merely has the driver.
const SONAMES: [&CStr; 2] = [c"libnvidia-encode.so.1", c"libnvidia-encode.so"];

type CreateInstance = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type MaxSupportedVersion = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;

/// Why the runtime could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No such library. The ordinary case on a machine without the hardware,
    /// and not an error worth raising above a debug line.
    Unavailable,
    /// The library loaded but does not export what it must, which means it is
    /// not the library we think it is.
    MissingSymbol,
    /// The driver predates the interface this was built against. Both numbers
    /// are carried because the fix is to compare them.
    DriverTooOld { compiled: Version, driver: Version },
    /// The runtime refused to hand over its function table.
    Status(NVENCSTATUS),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("encoder runtime not present"),
            Self::MissingSymbol => f.write_str("encoder runtime is missing an entry point"),
            Self::DriverTooOld { compiled, driver } => write!(
                f,
                "driver supports interface {driver}, this build needs {compiled} or newer"
            ),
            Self::Status(status) => write!(f, "encoder runtime returned status {status}"),
        }
    }
}

impl std::error::Error for Error {}

/// An interface version, as major and minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    /// The packing used by the runtime's version query, which is **not** the
    /// packing the structure stamps use. Four bits of minor, the rest major.
    const fn from_packed(packed: u32) -> Self {
        Self {
            major: packed >> 4,
            minor: packed & 0x0F,
        }
    }

    const fn packed(self) -> u32 {
        (self.major << 4) | self.minor
    }

    /// What this build was compiled against.
    pub const COMPILED: Self = Self {
        major: NVENCAPI_MAJOR_VERSION,
        minor: NVENCAPI_MINOR_VERSION,
    };
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The loaded runtime and its function table.
#[derive(Debug)]
pub struct Api {
    functions: NV_ENCODE_API_FUNCTION_LIST,
    driver: Version,
    /// Declared last so it is dropped last. The table above is a set of code
    /// addresses inside this library; unmapping it first would leave them
    /// dangling.
    _library: Library,
}

impl Api {
    /// Open the runtime, check the version, and take the function table.
    pub fn load() -> Result<Self, Error> {
        let library = Library::open_first(&SONAMES).ok_or(Error::Unavailable)?;

        // SAFETY: both signatures are transcribed from the vendored header and
        // are checked against it by the layout assertions on the types they
        // mention. Neither is called before it is resolved.
        let max_version: MaxSupportedVersion =
            unsafe { library.symbol(c"NvEncodeAPIGetMaxSupportedVersion") }
                .ok_or(Error::MissingSymbol)?;
        // SAFETY: as above.
        let create: CreateInstance =
            unsafe { library.symbol(c"NvEncodeAPICreateInstance") }.ok_or(Error::MissingSymbol)?;

        let mut packed = 0u32;
        // SAFETY: the pointer is to a live local for the duration of the call.
        let status = unsafe { max_version(&raw mut packed) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        let driver = Version::from_packed(packed);
        if Version::COMPILED.packed() > packed {
            return Err(Error::DriverTooOld {
                compiled: Version::COMPILED,
                driver,
            });
        }

        // The table is zeroed and then stamped. The runtime reads the stamp to
        // decide how much of the structure it may write, so a zero there is a
        // rejection rather than a default.
        let mut functions = MaybeUninit::<NV_ENCODE_API_FUNCTION_LIST>::zeroed();
        // SAFETY: the type is plain data, so an all-zero bit pattern is a valid
        // value of it, and the only field read before the call is the stamp.
        let functions = unsafe {
            (&raw mut (*functions.as_mut_ptr()).version).write(NV_ENCODE_API_FUNCTION_LIST_VER);
            let status = create(functions.as_mut_ptr());
            if status != NV_ENC_SUCCESS {
                return Err(Error::Status(status));
            }
            functions.assume_init()
        };

        Ok(Self {
            functions,
            driver,
            _library: library,
        })
    }

    /// The interface version the driver supports, which is at least
    /// [`Version::COMPILED`].
    pub fn driver_version(&self) -> Version {
        self.driver
    }

    /// The function table. Every entry is optional in the wire sense: the
    /// runtime fills what it implements and leaves the rest null.
    pub fn functions(&self) -> &NV_ENCODE_API_FUNCTION_LIST {
        &self.functions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packing here is four bits of minor, unlike the structure stamps,
    /// and confusing the two produces a comparison that is wrong only for
    /// some driver versions. Checked in both directions.
    #[test]
    fn the_query_packing_round_trips() {
        for (major, minor) in [(11, 0), (12, 1), (13, 0), (13, 15)] {
            let version = Version { major, minor };
            assert_eq!(Version::from_packed(version.packed()), version);
        }
        // 13.1 as the runtime reports it.
        assert_eq!(
            Version::from_packed(0xD1),
            Version {
                major: 13,
                minor: 1
            }
        );
    }

    /// A driver newer than the pin must be accepted and an older one refused.
    /// This is the whole reason the header is pinned low, so the comparison
    /// gets a test rather than a comment.
    #[test]
    fn an_older_driver_is_refused_and_a_newer_one_is_not() {
        let compiled = Version::COMPILED.packed();
        assert!(
            compiled
                > Version {
                    major: 10,
                    minor: 0
                }
                .packed()
        );
        assert!(
            compiled
                <= Version {
                    major: 11,
                    minor: 0
                }
                .packed()
        );
        assert!(
            compiled
                <= Version {
                    major: 13,
                    minor: 1
                }
                .packed()
        );
    }

    /// Opens a real session and asks the hardware what it will do. This is
    /// where the phase's open question gets its answer: whether the collect
    /// can be non-blocking, and whether the bitrate actuator exists at all.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_hardware_answers_what_the_pipeline_depends_on() {
        let cuda = crate::cuda::Cuda::load().expect("compute runtime");
        let device = cuda.any_device().expect("a device");
        let context = cuda.retain_primary(&device).expect("context");
        let api = Api::load().expect("encoder runtime");
        let session = api.open_session(context).expect("session");

        for codec in [Codec::H264, Codec::H265] {
            let caps = session.caps(codec).expect("caps");
            println!("{codec:?}: {caps:?}");
            assert!(caps.max_width >= 1920 && caps.max_height >= 1080);
            assert!(
                caps.dynamic_bitrate,
                "no live bitrate change: the congestion actuator cannot exist"
            );

            let mut formats = [0; 32];
            let n = session.input_formats(codec, &mut formats).expect("formats");
            assert!(n > 0, "hardware accepts no input format");
            println!("  {n} input formats: {:x?}", &formats[..n]);
        }
    }

    /// Needs the vendor driver, so it is off by default. Run with
    /// `cargo test -p lowlat-encode -- --ignored`.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_runtime_loads_on_this_machine() {
        match Api::load() {
            Ok(api) => {
                let driver = api.driver_version();
                assert!(driver >= Version::COMPILED);
                assert!(
                    api.functions().nvEncOpenEncodeSession.is_some(),
                    "the table came back without its entry points"
                );
                println!(
                    "driver interface {driver}, built against {}",
                    Version::COMPILED
                );
            }
            Err(Error::Unavailable) => panic!("no runtime present; this test needs the driver"),
            Err(error) => panic!("{error}"),
        }
    }
}

/// Which bitstream a session is opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    fn guid(self) -> crate::ffi::nvenc::GUID {
        match self {
            Self::H264 => crate::ffi::guids::NV_ENC_CODEC_H264_GUID,
            Self::H265 => crate::ffi::guids::NV_ENC_CODEC_HEVC_GUID,
        }
    }
}

/// What the hardware will actually do, asked rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Completion delivered through an event object. **Absent on this
    /// platform**, which is why the collect must use the non-blocking lock
    /// instead of waiting on anything.
    pub async_encode: bool,
    /// Bitrate may be changed without reinitialising. The precondition for the
    /// congestion controller's only actuator; without it the phase gate that
    /// counts keyframes across a rate change cannot be met at all.
    pub dynamic_bitrate: bool,
    pub max_width: u32,
    pub max_height: u32,
    /// Reordering frames costs latency, so this is a number we want to be able
    /// to set to zero rather than one we want to be large.
    pub max_bframes: u32,
    pub ten_bit: bool,
}

/// An open encode session.
///
/// Borrows the runtime and owns the context, so the two cannot be dropped out
/// from under it.
#[derive(Debug)]
pub struct Session<'a> {
    api: &'a Api,
    encoder: *mut core::ffi::c_void,
    /// Released after the encoder is destroyed, because the encoder was opened
    /// against it.
    _context: crate::cuda::Context,
}

impl<'a> Api {
    /// Open a session against a compute context.
    ///
    /// The context is taken rather than borrowed: an encoder outliving the
    /// context it was opened against is a use-after-free that presents as a
    /// driver fault with no line number.
    pub fn open_session(&'a self, context: crate::cuda::Context) -> Result<Session<'a>, Error> {
        let open = self
            .functions
            .nvEncOpenEncodeSessionEx
            .ok_or(Error::MissingSymbol)?;

        let mut params = unsafe {
            core::mem::zeroed::<crate::ffi::nvenc::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>()
        };
        params.version = crate::ffi::versions::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.deviceType = crate::ffi::nvenc::NV_ENC_DEVICE_TYPE_CUDA;
        params.device = context.raw().cast();
        params.apiVersion = crate::ffi::nvenc::NVENCAPI_VERSION;

        let mut encoder: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: both pointers are to live storage for the duration of the
        // call, and the parameter block is stamped with its own version.
        let status = unsafe { open(&raw mut params, &raw mut encoder) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }

        Ok(Session {
            api: self,
            encoder,
            _context: context,
        })
    }
}

impl Session<'_> {
    fn cap(&self, codec: Codec, which: crate::ffi::nvenc::NV_ENC_CAPS) -> Result<i32, Error> {
        let query = self
            .api
            .functions
            .nvEncGetEncodeCaps
            .ok_or(Error::MissingSymbol)?;
        let mut param = unsafe { core::mem::zeroed::<crate::ffi::nvenc::NV_ENC_CAPS_PARAM>() };
        param.version = crate::ffi::versions::NV_ENC_CAPS_PARAM_VER;
        param.capsToQuery = which;
        let mut value: i32 = 0;
        // SAFETY: the encoder is live, and both pointers are to live storage.
        let status = unsafe { query(self.encoder, codec.guid(), &raw mut param, &raw mut value) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(value)
    }

    /// Everything the pipeline decides on, in one query.
    pub fn caps(&self, codec: Codec) -> Result<Caps, Error> {
        use crate::ffi::nvenc as f;
        Ok(Caps {
            async_encode: self.cap(codec, f::NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT)? != 0,
            dynamic_bitrate: self.cap(codec, f::NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE)? != 0,
            max_width: self
                .cap(codec, f::NV_ENC_CAPS_WIDTH_MAX)?
                .max(0)
                .unsigned_abs(),
            max_height: self
                .cap(codec, f::NV_ENC_CAPS_HEIGHT_MAX)?
                .max(0)
                .unsigned_abs(),
            max_bframes: self
                .cap(codec, f::NV_ENC_CAPS_NUM_MAX_BFRAMES)?
                .max(0)
                .unsigned_abs(),
            ten_bit: self.cap(codec, f::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE)? != 0,
        })
    }

    /// Input formats the hardware accepts, written into `out`.
    ///
    /// Which of them we are willing to feed is a separate decision: the encoder
    /// accepting a packed format does not mean we may hand it one, because the
    /// conversion it would then do internally is the cost the pipeline exists
    /// to avoid.
    pub fn input_formats(
        &self,
        codec: Codec,
        out: &mut [crate::ffi::nvenc::NV_ENC_BUFFER_FORMAT],
    ) -> Result<usize, Error> {
        let count_fn = self
            .api
            .functions
            .nvEncGetInputFormatCount
            .ok_or(Error::MissingSymbol)?;
        let list_fn = self
            .api
            .functions
            .nvEncGetInputFormats
            .ok_or(Error::MissingSymbol)?;

        let mut count: u32 = 0;
        // SAFETY: the encoder is live and the pointer is to a live local.
        let status = unsafe { count_fn(self.encoder, codec.guid(), &raw mut count) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        let wanted = (count as usize).min(out.len());
        let mut written: u32 = 0;
        // SAFETY: `out` is writable for `wanted` entries, which is what is
        // passed as the capacity.
        let status = unsafe {
            list_fn(
                self.encoder,
                codec.guid(),
                out.as_mut_ptr(),
                u32::try_from(wanted).unwrap_or(0),
                &raw mut written,
            )
        };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(written as usize)
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some(destroy) = self.api.functions.nvEncDestroyEncoder {
            // SAFETY: the encoder came from a successful open and is destroyed
            // once, because this type is neither `Copy` nor `Clone`. This runs
            // before the context field is dropped, which is the required order.
            unsafe { destroy(self.encoder) };
        }
    }
}
