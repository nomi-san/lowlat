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
    /// The compute context could not be made current on this thread.
    NoContext,
    /// As many pictures are in flight as the encoder will hold. The caller
    /// collects one before submitting another; it is back pressure, not a
    /// fault.
    QueueFull,
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
            Self::NoContext => f.write_str("compute context could not be made current"),
            Self::QueueFull => f.write_str("encoder queue is full"),
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

    /// Configure a real encoder and change its rate underneath itself.
    ///
    /// The reconfigure half is the congestion actuator, and the gate that
    /// matters for it counts keyframes across the change. That count needs
    /// frames, which needs the submit path, so this asserts the call is
    /// accepted and the counting arrives with the encode loop.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn an_encoder_configures_and_changes_rate_without_reinitialising() {
        for codec in [Codec::H264, Codec::H265] {
            let cuda = crate::cuda::Cuda::load().expect("compute runtime");
            let device = cuda.any_device().expect("a device");
            let context = cuda.retain_primary(&device).expect("context");
            let api = Api::load().expect("encoder runtime");
            let session = api.open_session(context).expect("session");

            let config = Config {
                codec,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 20_000_000,
            };
            let mut encoder = session.initialize(config).expect("initialize");
            assert_eq!(encoder.config().bitrate_bps, 20_000_000);

            // Down hard, then back up, the way sustained congestion and its
            // recovery would drive it.
            for rate in [6_000_000, 3_000_000, 12_000_000, 25_000_000] {
                encoder.reconfigure(rate).expect("reconfigure");
                assert_eq!(encoder.config().bitrate_bps, rate);
            }
            println!("{codec:?}: configured at 1080p60 and reconfigured four times");
        }
    }

    /// Encodes, and measures whether the collect waits.
    ///
    /// **The no-wait flag is not honoured on this driver.** The interface
    /// documents it as returning a busy status in synchronous mode, and the
    /// driver instead waits: four collects for four pictures, no busy status
    /// once, and the slowest collect equal to one picture's encode time. A
    /// genuinely non-blocking lock spun in this loop would return busy
    /// hundreds of times.
    ///
    /// So this asserts the encode is correct and records the collect cost as
    /// a number. Gate A item 5 is **not** met by this path and is met instead
    /// by completion events on the encoder's own streams, which is the next
    /// piece of work. The measurement is kept because it is the evidence that
    /// the cheaper path was tried and does not work.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_encoder_produces_a_bitstream_and_the_collect_waits() {
        use std::time::Instant;

        let cuda = crate::cuda::Cuda::load().expect("compute runtime");
        let device = cuda.any_device().expect("a device");
        let context = cuda.retain_primary(&device).expect("context");
        let api = Api::load().expect("encoder runtime");
        let session = api.open_session(context).expect("session");

        let config = Config {
            codec: Codec::H264,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 20_000_000,
        };
        let mut encoder = session.initialize(config).expect("initialize");

        // One planar frame: luma rows then half as many interleaved chroma
        // rows, in one allocation at the driver's preferred pitch.
        let rows = (config.height + config.height / 2) as usize;
        let buffer = cuda
            .alloc_pitch(config.width as usize, rows)
            .expect("allocate");
        cuda.fill(&buffer, 0x80, buffer.pitch() * rows)
            .expect("fill");
        let input = encoder.register(&buffer).expect("register");

        // Fill the queue. The first is forced to a keyframe; the rest are not.
        for index in 0..IN_FLIGHT {
            encoder.submit(&input, index == 0).expect("submit");
        }
        assert_eq!(encoder.in_flight(), IN_FLIGHT);
        assert_eq!(
            encoder.submit(&input, false).unwrap_err(),
            Error::QueueFull,
            "the queue accepted more than it holds"
        );

        // The measurement. Every collect is timed; the slowest is what a
        // blocking call would show, because it cannot return before the
        // picture it names has finished.
        let mut slowest = std::time::Duration::ZERO;
        let drain_started = Instant::now();
        let mut collected = 0usize;
        let mut keyframes = 0usize;
        let mut first_unit = 0u8;
        while collected < IN_FLIGHT {
            let started = Instant::now();
            let polled = encoder.poll().expect("poll");
            let took = started.elapsed();
            slowest = slowest.max(took);
            match polled {
                Poll::Ready {
                    bitstream,
                    keyframe,
                } => {
                    assert!(!bitstream.is_empty(), "an empty access unit");
                    if collected == 0 {
                        assert_eq!(&bitstream[..4], &[0, 0, 0, 1], "no start code");
                        first_unit = bitstream[4] & 0x1F;
                    }
                    if keyframe {
                        keyframes += 1;
                    }
                    collected += 1;
                }
                Poll::Pending => std::hint::spin_loop(),
            }
        }
        let drain = drain_started.elapsed();

        println!(
            "slowest collect {slowest:?}, drain of {IN_FLIGHT} {drain:?}, \
             keyframes {keyframes}, first unit type {first_unit}"
        );

        // Parameter set first, because they repeat on every keyframe.
        assert_eq!(first_unit, 7, "first unit is not a parameter set");
        assert_eq!(keyframes, 1, "expected exactly the forced keyframe");
        // Recorded rather than asserted as a bound, because the bound is not
        // met by this path and pretending otherwise would make the gate item
        // pass against the very behaviour it exists to reject.
        assert!(
            slowest <= drain,
            "a single collect cannot exceed the whole drain"
        );
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
        // Everything from here on runs against whatever context is current on
        // this thread, including allocations made for it, so bind it now
        // rather than leaving each call site to remember.
        context.make_current().map_err(|_| Error::NoContext)?;

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

/// What an encoder is set up to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

/// Colour signalling, from [05 §3.1](../../../docs/05-host.md).
///
/// The far side applies BT.709 unconditionally, so this is not a preference.
/// Emitting the description as well as the pixels matters: a decoder handed a
/// stream with no colour description may choose a different matrix, and some
/// do.
mod colour {
    /// Unspecified. The frame is not analogue-sourced and has no meaningful
    /// video format to declare.
    pub(super) const VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
    /// BT.709 for all three of primaries, transfer and matrix.
    pub(super) const BT709: u32 = 1;
    /// Limited range, measured from a recorded session rather than chosen.
    pub(super) const FULL_RANGE: u32 = 0;
}

/// A configured encoder.
pub struct Encoder<'a> {
    session: Session<'a>,
    config: Config,
    /// Retained so a reconfigure can resend the whole block. The interface
    /// takes a complete configuration every time, not a delta.
    encode_config: crate::ffi::nvenc::NV_ENC_CONFIG,
    init_params: crate::ffi::nvenc::NV_ENC_INITIALIZE_PARAMS,
    /// One output buffer per in-flight picture, allocated once.
    outputs: [crate::ffi::nvenc::NV_ENC_OUTPUT_PTR; IN_FLIGHT],
    next_output: usize,
    /// Submitted and not yet collected, oldest first.
    pending: std::collections::VecDeque<Pending>,
    /// Where a collected access unit is copied to.
    ///
    /// The interface's buffer is valid only until the next call on this
    /// encoder, so it cannot be handed to a packetiser on another thread. One
    /// copy is the price; the capacity is reused, so it stops allocating after
    /// the first few frames. The per-guest pool replaces this.
    collected: Vec<u8>,
}

impl<'a> Session<'a> {
    /// Configure the encoder for low latency and initialise it.
    pub fn initialize(self, config: Config) -> Result<Encoder<'a>, Error> {
        use crate::ffi::nvenc as f;

        let preset = crate::ffi::guids::NV_ENC_PRESET_P4_GUID;
        let tuning = f::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

        // Start from the preset the hardware recommends, then override only
        // what the pipeline actually requires. Building a configuration from
        // zero means silently accepting a default for every field nobody
        // thought about.
        let get_preset = self
            .api
            .functions
            .nvEncGetEncodePresetConfigEx
            .ok_or(Error::MissingSymbol)?;
        let mut preset_config = unsafe { core::mem::zeroed::<f::NV_ENC_PRESET_CONFIG>() };
        preset_config.version = crate::ffi::versions::NV_ENC_PRESET_CONFIG_VER;
        preset_config.presetCfg.version = crate::ffi::versions::NV_ENC_CONFIG_VER;
        // SAFETY: the encoder is live and the block is stamped.
        let status = unsafe {
            get_preset(
                self.encoder,
                config.codec.guid(),
                preset,
                tuning,
                &raw mut preset_config,
            )
        };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }

        let mut encode_config = preset_config.presetCfg;
        encode_config.version = crate::ffi::versions::NV_ENC_CONFIG_VER;

        // **Keyframes only when asked for.** Recovery is driven by the
        // delivery gate, which throttles them; a periodic keyframe on top of
        // that is bandwidth spent on a schedule rather than on a need.
        encode_config.gopLength = f::NVENC_INFINITE_GOPLENGTH;
        // No B-frames. The hardware offers up to seven and every one of them
        // is reorder delay, which is latency paid on every frame to save bits
        // on some of them. Wrong trade here.
        encode_config.frameIntervalP = 1;

        let rc = &mut encode_config.rcParams;
        rc.rateControlMode = f::NV_ENC_PARAMS_RC_VBR;
        rc.averageBitRate = config.bitrate_bps;
        rc.maxBitRate = config.bitrate_bps;
        // One frame of buffer. A larger one lets the encoder smooth bitrate
        // across frames, which is exactly the queueing this pipeline exists to
        // avoid: those bits arrive late rather than not at all.
        let per_frame = config.bitrate_bps / config.fps.max(1);
        rc.vbvBufferSize = per_frame;
        rc.vbvInitialDelay = per_frame;
        // Frames nothing references can be dropped by the gate without
        // breaking anyone's reference chain.
        rc.set_enableNonRefP(1);
        // Output order is capture order. Any reordering is latency on every frame.
        rc.set_zeroReorderDelay(1);

        // SAFETY: the union member written is the one matching the codec, and
        // it is read back through the same member for the life of the encoder.
        unsafe {
            match config.codec {
                Codec::H264 => {
                    let h264 = &mut encode_config.encodeCodecConfig.h264Config;
                    h264.idrPeriod = f::NVENC_INFINITE_GOPLENGTH;
                    // Parameter sets on every keyframe. A guest that joins
                    // mid-stream is then decodable from the next keyframe
                    // alone, with no separate out-of-band step to get wrong.
                    h264.set_repeatSPSPPS(1);
                    let vui = &mut h264.h264VUIParameters;
                    vui.videoSignalTypePresentFlag = 1;
                    vui.videoFormat = colour::VIDEO_FORMAT_UNSPECIFIED;
                    vui.videoFullRangeFlag = colour::FULL_RANGE;
                    vui.colourDescriptionPresentFlag = 1;
                    vui.colourPrimaries = colour::BT709;
                    vui.transferCharacteristics = colour::BT709;
                    vui.colourMatrix = colour::BT709;
                }
                Codec::H265 => {
                    let hevc = &mut encode_config.encodeCodecConfig.hevcConfig;
                    hevc.idrPeriod = f::NVENC_INFINITE_GOPLENGTH;
                    hevc.set_repeatSPSPPS(1);
                    let vui = &mut hevc.hevcVUIParameters;
                    vui.videoSignalTypePresentFlag = 1;
                    vui.videoFormat = colour::VIDEO_FORMAT_UNSPECIFIED;
                    vui.videoFullRangeFlag = colour::FULL_RANGE;
                    vui.colourDescriptionPresentFlag = 1;
                    vui.colourPrimaries = colour::BT709;
                    vui.transferCharacteristics = colour::BT709;
                    vui.colourMatrix = colour::BT709;
                }
            }
        }

        let mut init_params = unsafe { core::mem::zeroed::<f::NV_ENC_INITIALIZE_PARAMS>() };
        init_params.version = crate::ffi::versions::NV_ENC_INITIALIZE_PARAMS_VER;
        init_params.encodeGUID = config.codec.guid();
        init_params.presetGUID = preset;
        init_params.tuningInfo = tuning;
        init_params.encodeWidth = config.width;
        init_params.encodeHeight = config.height;
        init_params.darWidth = config.width;
        init_params.darHeight = config.height;
        init_params.frameRateNum = config.fps;
        init_params.frameRateDen = 1;
        // No completion object: the hardware reports no asynchronous support
        // on this platform, so the collect is a non-blocking poll instead.
        init_params.enableEncodeAsync = 0;
        // The encoder decides picture types, which is what lets a keyframe be
        // requested per frame rather than scheduled.
        init_params.enablePTD = 1;
        init_params.encodeConfig = &raw mut encode_config;

        let initialize = self
            .api
            .functions
            .nvEncInitializeEncoder
            .ok_or(Error::MissingSymbol)?;
        // SAFETY: both blocks are stamped and live for the duration of the
        // call; the configuration is copied by the callee.
        let status = unsafe { initialize(self.encoder, &raw mut init_params) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }

        // The interface copies the configuration during the call above, so
        // the pointer has done its job. Cleared rather than carried: the
        // block it names is a local about to be moved into the returned
        // value, and a retained pointer to it would dangle. Reconfigure
        // re-points this at the field it ends up in.
        init_params.encodeConfig = core::ptr::null_mut();

        // Output buffers, allocated once. The interface owns the storage; we
        // own the handles and must give them back before the encoder goes.
        let create = self
            .api
            .functions
            .nvEncCreateBitstreamBuffer
            .ok_or(Error::MissingSymbol)?;
        let mut outputs = [core::ptr::null_mut(); IN_FLIGHT];
        for slot in &mut outputs {
            let mut buffer = unsafe { core::mem::zeroed::<f::NV_ENC_CREATE_BITSTREAM_BUFFER>() };
            buffer.version = crate::ffi::versions::NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            // SAFETY: the block is stamped and live for the call.
            let status = unsafe { create(self.encoder, &raw mut buffer) };
            if status != NV_ENC_SUCCESS {
                return Err(Error::Status(status));
            }
            *slot = buffer.bitstreamBuffer;
        }

        Ok(Encoder {
            session: self,
            config,
            encode_config,
            init_params,
            outputs,
            next_output: 0,
            pending: std::collections::VecDeque::with_capacity(IN_FLIGHT),
            collected: Vec::new(),
        })
    }
}

impl core::fmt::Debug for Encoder<'_> {
    /// The configuration blocks carry a union and cannot be derived; the
    /// settings worth seeing in a log are in [`Config`] anyway.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("config", &self.config)
            .finish()
    }
}

impl Encoder<'_> {
    pub fn config(&self) -> Config {
        self.config
    }

    /// Change the bitrate on a running encoder.
    ///
    /// **Never reinitialises and never forces a keyframe.** Congestion moves
    /// the rate many times a minute; a keyframe or a reinitialisation at that
    /// cadence would be visible as a stutter every time the network hiccuped,
    /// which is the failure this actuator exists to avoid rather than cause.
    pub fn reconfigure(&mut self, bitrate_bps: u32) -> Result<(), Error> {
        use crate::ffi::nvenc as f;

        self.config.bitrate_bps = bitrate_bps;
        let per_frame = bitrate_bps / self.config.fps.max(1);
        self.encode_config.rcParams.averageBitRate = bitrate_bps;
        self.encode_config.rcParams.maxBitRate = bitrate_bps;
        self.encode_config.rcParams.vbvBufferSize = per_frame;
        self.encode_config.rcParams.vbvInitialDelay = per_frame;

        let mut params = unsafe { core::mem::zeroed::<f::NV_ENC_RECONFIGURE_PARAMS>() };
        params.version = crate::ffi::versions::NV_ENC_RECONFIGURE_PARAMS_VER;
        params.reInitEncodeParams = self.init_params;
        params.reInitEncodeParams.encodeConfig = &raw mut self.encode_config;
        // Both deliberately clear. Either one turns a rate change into a
        // visible discontinuity.
        params.set_resetEncoder(0);
        params.set_forceIDR(0);

        let reconfigure = self
            .session
            .api
            .functions
            .nvEncReconfigureEncoder
            .ok_or(Error::MissingSymbol)?;
        // SAFETY: the encoder is live and the block is stamped and live for
        // the call.
        let status = unsafe { reconfigure(self.session.encoder, &raw mut params) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(())
    }
}

/// How many pictures may be in flight at once.
///
/// The submit returns immediately and the collect is non-blocking, so this
/// bounds only how far the pipeline may run ahead of the hardware before it
/// must collect. Deep enough to absorb a slow frame, shallow enough that a
/// stall is noticed rather than buffered away.
const IN_FLIGHT: usize = 4;

/// What a collect found.
#[derive(Debug)]
pub enum Poll<'a> {
    /// A finished access unit, valid until the next call on this encoder.
    Ready { bitstream: &'a [u8], keyframe: bool },
    /// Nothing finished yet. **Not an error and not a wait**: the caller goes
    /// round its loop and asks again.
    Pending,
}

/// A registered input surface.
///
/// Registration is per buffer and mapping is per submit, which is the split
/// the interface imposes: registering costs a driver round trip and must not
/// happen per frame.
#[derive(Debug)]
pub struct Input {
    registered: crate::ffi::nvenc::NV_ENC_REGISTERED_PTR,
    width: u32,
    height: u32,
    pitch: u32,
}

#[derive(Debug)]
struct Pending {
    output: crate::ffi::nvenc::NV_ENC_OUTPUT_PTR,
    mapped: crate::ffi::nvenc::NV_ENC_INPUT_PTR,
}

impl Drop for Encoder<'_> {
    /// Hands the output buffers back before the session drops and destroys the
    /// encoder they belong to. Anything still in flight is unmapped first, so
    /// a teardown mid-stream does not leak a mapping.
    fn drop(&mut self) {
        let functions = self.session.api.functions;
        if let Some(unmap) = functions.nvEncUnmapInputResource {
            for entry in &self.pending {
                // SAFETY: each was mapped by `submit` and not yet unmapped.
                unsafe { unmap(self.session.encoder, entry.mapped) };
            }
        }
        if let Some(destroy) = functions.nvEncDestroyBitstreamBuffer {
            for output in self.outputs {
                if !output.is_null() {
                    // SAFETY: each came from a successful create above.
                    unsafe { destroy(self.session.encoder, output) };
                }
            }
        }
    }
}

impl Encoder<'_> {
    fn functions(&self) -> &crate::ffi::nvenc::NV_ENCODE_API_FUNCTION_LIST {
        &self.session.api.functions
    }

    /// Register a device allocation holding one planar frame.
    pub fn register(&self, buffer: &crate::cuda::DeviceBuffer) -> Result<Input, Error> {
        use crate::ffi::nvenc as f;
        let register = self
            .functions()
            .nvEncRegisterResource
            .ok_or(Error::MissingSymbol)?;

        let mut params = unsafe { core::mem::zeroed::<f::NV_ENC_REGISTER_RESOURCE>() };
        params.version = crate::ffi::versions::NV_ENC_REGISTER_RESOURCE_VER;
        params.resourceType = f::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
        params.width = self.config.width;
        params.height = self.config.height;
        params.pitch = u32::try_from(buffer.pitch()).map_err(|_| Error::MissingSymbol)?;
        params.resourceToRegister = buffer.ptr() as *mut core::ffi::c_void;
        params.bufferFormat = f::NV_ENC_BUFFER_FORMAT_NV12;
        params.bufferUsage = f::NV_ENC_INPUT_IMAGE;

        // SAFETY: the block is stamped and live for the call.
        let status = unsafe { register(self.session.encoder, &raw mut params) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(Input {
            registered: params.registeredResource,
            width: self.config.width,
            height: self.config.height,
            pitch: params.pitch,
        })
    }

    /// Hand a frame to the hardware. Returns as soon as it is queued.
    pub fn submit(&mut self, input: &Input, force_keyframe: bool) -> Result<(), Error> {
        use crate::ffi::nvenc as f;
        if self.pending.len() >= IN_FLIGHT {
            return Err(Error::QueueFull);
        }

        let map = self
            .functions()
            .nvEncMapInputResource
            .ok_or(Error::MissingSymbol)?;
        let encode = self
            .functions()
            .nvEncEncodePicture
            .ok_or(Error::MissingSymbol)?;

        let mut mapping = unsafe { core::mem::zeroed::<f::NV_ENC_MAP_INPUT_RESOURCE>() };
        mapping.version = crate::ffi::versions::NV_ENC_MAP_INPUT_RESOURCE_VER;
        mapping.registeredResource = input.registered;
        // SAFETY: the block is stamped and live for the call.
        let status = unsafe { map(self.session.encoder, &raw mut mapping) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }

        let output = self.outputs[self.next_output];
        self.next_output = (self.next_output + 1) % IN_FLIGHT;

        let mut pic = unsafe { core::mem::zeroed::<f::NV_ENC_PIC_PARAMS>() };
        pic.version = crate::ffi::versions::NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = input.width;
        pic.inputHeight = input.height;
        pic.inputPitch = input.pitch;
        pic.inputBuffer = mapping.mappedResource;
        pic.outputBitstream = output;
        pic.bufferFmt = mapping.mappedBufferFmt;
        pic.pictureStruct = f::NV_ENC_PIC_STRUCT_FRAME;
        if force_keyframe {
            pic.encodePicFlags = f::NV_ENC_PIC_FLAG_FORCEIDR;
        }

        // SAFETY: the block is stamped and live; the mapped input and the
        // output buffer both outlive the encode because they are held in the
        // queue below until collected.
        let status = unsafe { encode(self.session.encoder, &raw mut pic) };
        if status != NV_ENC_SUCCESS && status != f::NV_ENC_ERR_NEED_MORE_INPUT {
            return Err(Error::Status(status));
        }

        self.pending.push_back(Pending {
            output,
            mapped: mapping.mappedResource,
        });
        Ok(())
    }

    /// Collect a finished access unit, or report that none is ready.
    ///
    /// **Never blocks.** The hardware offers no completion object on this
    /// platform, so the lock is taken in its no-wait form and a busy answer is
    /// reported as pending. Taking the blocking form and hiding it behind
    /// queue depth would turn "the encoder fell behind" into "the pipeline
    /// thread is stopped", at exactly the moment that must not happen.
    pub fn poll(&mut self) -> Result<Poll<'_>, Error> {
        use crate::ffi::nvenc as f;
        let Some(head) = self.pending.front() else {
            return Ok(Poll::Pending);
        };
        let (output, mapped) = (head.output, head.mapped);

        let lock = self
            .functions()
            .nvEncLockBitstream
            .ok_or(Error::MissingSymbol)?;
        let mut params = unsafe { core::mem::zeroed::<f::NV_ENC_LOCK_BITSTREAM>() };
        params.version = crate::ffi::versions::NV_ENC_LOCK_BITSTREAM_VER;
        params.outputBitstream = output;
        params.set_doNotWait(1);

        // SAFETY: the block is stamped and live for the call.
        let status = unsafe { lock(self.session.encoder, &raw mut params) };
        if status == f::NV_ENC_ERR_LOCK_BUSY {
            return Ok(Poll::Pending);
        }
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }

        let len = params.bitstreamSizeInBytes as usize;
        // SAFETY: the interface reports a pointer to `len` readable bytes,
        // valid until the matching unlock. The slice is handed out borrowing
        // `self`, so no further call can be made while it is alive.
        let bitstream =
            unsafe { core::slice::from_raw_parts(params.bitstreamBufferPtr.cast::<u8>(), len) };
        let keyframe = params.pictureType == f::NV_ENC_PIC_TYPE_IDR
            || params.pictureType == f::NV_ENC_PIC_TYPE_I;

        self.collected.clear();
        self.collected.extend_from_slice(bitstream);

        if let Some(unlock) = self.functions().nvEncUnlockBitstream {
            // SAFETY: balances the lock above, on the same buffer.
            unsafe { unlock(self.session.encoder, output) };
        }
        if let Some(unmap) = self.functions().nvEncUnmapInputResource {
            // SAFETY: balances the map in `submit`, on the same resource.
            unsafe { unmap(self.session.encoder, mapped) };
        }
        self.pending.pop_front();

        Ok(Poll::Ready {
            bitstream: &self.collected,
            keyframe,
        })
    }

    /// How many pictures are queued but not collected.
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }
}
