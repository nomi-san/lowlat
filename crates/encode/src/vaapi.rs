//! The open-stack encoder.
//!
//! Second backend, and the reason the encoder interface will be a trait rather
//! than one concrete type: an interface shaped by a single implementation
//! encodes that implementation's assumptions, which is what the phase gate
//! requiring two backends exists to prevent.
//!
//! **This one is not the zero-copy pairing on every machine, and neither is the
//! other.** Which backend can take a captured frame without a copy depends on
//! which device drives the display, discovered at construction
//! ([`crate::cuda`] carries the same rule). Where they disagree the pipeline
//! refuses rather than inserting a transfer, per docs/05-host.md section 4.
//!
//! Loaded at runtime like everything else here, so a machine without the
//! driver has a missing backend rather than a process that will not start.

use core::ffi::{CStr, c_char, c_int, c_uint};

use lowlat_common::dynlib::Library;

use crate::ffi::va::{
    VA_ATTRIB_NOT_SUPPORTED, VA_ENC_PACKED_HEADER_PICTURE, VA_ENC_PACKED_HEADER_SEQUENCE,
    VA_PROGRESSIVE, VA_RC_CBR, VA_RC_CQP, VA_RC_VBR, VA_RT_FORMAT_YUV420, VA_STATUS_SUCCESS,
    VAConfigAttrib, VAConfigAttribEncPackedHeaders, VAConfigAttribRTFormat,
    VAConfigAttribRateControl, VAConfigID, VAContextID, VADisplay, VAEntrypoint,
    VAEntrypointEncSlice, VAProfile, VAProfileH264High, VAProfileH264Main, VAProfileHEVCMain,
    VAStatus, VASurfaceID,
};

/// The core interface and its display binding, versioned.
const LIBVA: [&CStr; 2] = [c"libva.so.2", c"libva.so"];
const LIBVA_DRM: [&CStr; 2] = [c"libva-drm.so.2", c"libva-drm.so"];

type GetDisplayDrm = unsafe extern "C" fn(c_int) -> VADisplay;
type Initialize = unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
type Terminate = unsafe extern "C" fn(VADisplay) -> VAStatus;
type MaxNumProfiles = unsafe extern "C" fn(VADisplay) -> c_int;
type QueryConfigProfiles = unsafe extern "C" fn(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus;
type MaxNumEntrypoints = unsafe extern "C" fn(VADisplay) -> c_int;
type QueryConfigEntrypoints =
    unsafe extern "C" fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus;
type ErrorStr = unsafe extern "C" fn(VAStatus) -> *const c_char;
type GetConfigAttributes = unsafe extern "C" fn(
    VADisplay,
    VAProfile,
    VAEntrypoint,
    *mut VAConfigAttrib,
    c_int,
) -> VAStatus;
type CreateConfig = unsafe extern "C" fn(
    VADisplay,
    VAProfile,
    VAEntrypoint,
    *mut VAConfigAttrib,
    c_int,
    *mut VAConfigID,
) -> VAStatus;
type DestroyConfig = unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus;
type CreateSurfaces = unsafe extern "C" fn(
    VADisplay,
    c_uint,
    c_uint,
    c_uint,
    *mut VASurfaceID,
    c_uint,
    *mut core::ffi::c_void,
    c_uint,
) -> VAStatus;
type DestroySurfaces = unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus;
type CreateContext = unsafe extern "C" fn(
    VADisplay,
    VAConfigID,
    c_int,
    c_int,
    c_int,
    *mut VASurfaceID,
    c_int,
    *mut VAContextID,
) -> VAStatus;
type DestroyContext = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;

/// Why the backend could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No such library, which is the ordinary case without the driver.
    Unavailable,
    /// Loaded, but missing an entry point it must export.
    MissingSymbol,
    /// The render node could not be opened. Usually a permission on the device
    /// rather than an absent device.
    NoDevice,
    /// A call failed, carrying its status.
    Status(VAStatus),
    /// The driver has no encode entry point for any codec we speak. A decode
    /// only device reaches here, and it is a refusal rather than a fault.
    NoEncoder,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("display runtime not present"),
            Self::MissingSymbol => f.write_str("display runtime is missing an entry point"),
            Self::NoDevice => f.write_str("render node could not be opened"),
            Self::Status(status) => write!(f, "display runtime returned status {status}"),
            Self::NoEncoder => f.write_str("device offers no encode entry point"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

/// A count the interface reported, as a length.
///
/// These are signed and the interface is entitled to report a negative on
/// failure, so a cast would turn one into an enormous length. Converted rather
/// than cast, with the failure folded into zero.
fn count(reported: c_int) -> usize {
    usize::try_from(reported).unwrap_or(0)
}

/// Which bitstream, in this interface's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    /// Profiles worth trying, best first.
    ///
    /// High before Main for H.264: every decoder that matters handles High,
    /// and it is what a peer expects to receive.
    fn profiles(self) -> &'static [VAProfile] {
        match self {
            Self::H264 => &[VAProfileH264High, VAProfileH264Main],
            Self::H265 => &[VAProfileHEVCMain],
        }
    }
}

/// The loaded runtime.
#[derive(Debug)]
pub struct Vaapi {
    initialize: Initialize,
    terminate: Terminate,
    max_num_profiles: MaxNumProfiles,
    query_config_profiles: QueryConfigProfiles,
    max_num_entrypoints: MaxNumEntrypoints,
    query_config_entrypoints: QueryConfigEntrypoints,
    error_str: ErrorStr,
    get_config_attributes: GetConfigAttributes,
    create_config: CreateConfig,
    destroy_config: DestroyConfig,
    create_surfaces: CreateSurfaces,
    destroy_surfaces: DestroySurfaces,
    create_context: CreateContext,
    destroy_context: DestroyContext,
    get_display_drm: GetDisplayDrm,
    /// Last, so both outlive the addresses taken from them.
    _libva_drm: Library,
    _libva: Library,
}

/// An initialised display bound to one render node.
#[derive(Debug)]
pub struct Display<'a> {
    va: &'a Vaapi,
    raw: VADisplay,
    /// Closed after the display is terminated, never before: the driver holds
    /// this descriptor for as long as the display lives.
    fd: c_int,
    version: (i32, i32),
}

impl Vaapi {
    /// Open the runtime.
    pub fn load() -> Result<Self> {
        let libva = Library::open_first(&LIBVA).ok_or(Error::Unavailable)?;
        let libva_drm = Library::open_first(&LIBVA_DRM).ok_or(Error::Unavailable)?;

        // SAFETY: every signature is transcribed from the vendored headers.
        // These names carry no version suffix, unlike the compute runtime's.
        unsafe {
            Ok(Self {
                initialize: libva.symbol(c"vaInitialize").ok_or(Error::MissingSymbol)?,
                terminate: libva.symbol(c"vaTerminate").ok_or(Error::MissingSymbol)?,
                max_num_profiles: libva
                    .symbol(c"vaMaxNumProfiles")
                    .ok_or(Error::MissingSymbol)?,
                query_config_profiles: libva
                    .symbol(c"vaQueryConfigProfiles")
                    .ok_or(Error::MissingSymbol)?,
                max_num_entrypoints: libva
                    .symbol(c"vaMaxNumEntrypoints")
                    .ok_or(Error::MissingSymbol)?,
                query_config_entrypoints: libva
                    .symbol(c"vaQueryConfigEntrypoints")
                    .ok_or(Error::MissingSymbol)?,
                error_str: libva.symbol(c"vaErrorStr").ok_or(Error::MissingSymbol)?,
                get_config_attributes: libva
                    .symbol(c"vaGetConfigAttributes")
                    .ok_or(Error::MissingSymbol)?,
                create_config: libva
                    .symbol(c"vaCreateConfig")
                    .ok_or(Error::MissingSymbol)?,
                destroy_config: libva
                    .symbol(c"vaDestroyConfig")
                    .ok_or(Error::MissingSymbol)?,
                create_surfaces: libva
                    .symbol(c"vaCreateSurfaces")
                    .ok_or(Error::MissingSymbol)?,
                destroy_surfaces: libva
                    .symbol(c"vaDestroySurfaces")
                    .ok_or(Error::MissingSymbol)?,
                create_context: libva
                    .symbol(c"vaCreateContext")
                    .ok_or(Error::MissingSymbol)?,
                destroy_context: libva
                    .symbol(c"vaDestroyContext")
                    .ok_or(Error::MissingSymbol)?,
                get_display_drm: libva_drm
                    .symbol(c"vaGetDisplayDRM")
                    .ok_or(Error::MissingSymbol)?,
                _libva_drm: libva_drm,
                _libva: libva,
            })
        }
    }

    /// What the runtime says a status means. Diagnostic only.
    pub fn status_text(&self, status: VAStatus) -> &str {
        // SAFETY: the interface returns a pointer to a static string it owns.
        let text = unsafe { (self.error_str)(status) };
        if text.is_null() {
            return "unknown";
        }
        // SAFETY: non-null and NUL terminated by contract.
        unsafe { CStr::from_ptr(text) }
            .to_str()
            .unwrap_or("unknown")
    }

    fn check(&self, status: VAStatus) -> Result<()> {
        if status == VA_STATUS_SUCCESS as VAStatus {
            Ok(())
        } else {
            Err(Error::Status(status))
        }
    }

    /// Bind a display to a render node, by path.
    ///
    /// The **render** node rather than the card node: encoding needs no
    /// display control, and the card node additionally needs a privilege this
    /// process should not hold for a job that does not require it.
    pub fn open(&self, node: &CStr) -> Result<Display<'_>> {
        // SAFETY: the path is NUL terminated. No mode is needed without
        // O_CREAT. Closed on the error paths below and in `Drop`.
        let fd = unsafe { libc::open(node.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(Error::NoDevice);
        }

        // SAFETY: the descriptor is open for the duration.
        let raw = unsafe { (self.get_display_drm)(fd) };
        if raw.is_null() {
            // SAFETY: the descriptor was opened above and is not yet owned by
            // anything else.
            unsafe { libc::close(fd) };
            return Err(Error::NoDevice);
        }

        let mut major: c_int = 0;
        let mut minor: c_int = 0;
        // SAFETY: both out pointers are to live locals.
        let status = unsafe { (self.initialize)(raw, &raw mut major, &raw mut minor) };
        if let Err(error) = self.check(status) {
            // SAFETY: initialise failed, so the display owns nothing; the
            // descriptor is still ours to close.
            unsafe { libc::close(fd) };
            return Err(error);
        }

        Ok(Display {
            va: self,
            raw,
            fd,
            version: (major, minor),
        })
    }
}

impl Display<'_> {
    /// The interface version the driver implements.
    pub fn version(&self) -> (i32, i32) {
        self.version
    }

    /// The raw handle, for the calls that take one.
    pub fn raw(&self) -> VADisplay {
        self.raw
    }

    /// The profile this driver will encode `codec` with, if any.
    ///
    /// **Asked, not assumed.** A device may decode a codec and not encode it,
    /// and the two are separate entry points against the same profile; taking
    /// the profile's presence as proof of encode support is the mistake this
    /// exists to avoid.
    pub fn encode_profile(&self, codec: Codec) -> Result<VAProfile> {
        let mut profiles = vec![0 as VAProfile; self.max_profiles()?];
        let mut found: c_int = 0;
        // SAFETY: the buffer is writable for the length the interface was told
        // about, and the count is written back.
        let status = unsafe {
            (self.va.query_config_profiles)(self.raw, profiles.as_mut_ptr(), &raw mut found)
        };
        self.va.check(status)?;
        let available = profiles.get(..count(found)).unwrap_or(&[]);

        for wanted in codec.profiles() {
            if !available.contains(wanted) {
                continue;
            }
            if self.has_encode_entrypoint(*wanted)? {
                return Ok(*wanted);
            }
        }
        Err(Error::NoEncoder)
    }

    fn max_profiles(&self) -> Result<usize> {
        // SAFETY: the display is live.
        Ok(count(unsafe { (self.va.max_num_profiles)(self.raw) }))
    }

    fn has_encode_entrypoint(&self, profile: VAProfile) -> Result<bool> {
        // SAFETY: the display is live.
        let capacity = count(unsafe { (self.va.max_num_entrypoints)(self.raw) });
        let mut entrypoints = vec![0 as VAEntrypoint; capacity];
        let mut found: c_int = 0;
        // SAFETY: the buffer is writable for the length passed.
        let status = unsafe {
            (self.va.query_config_entrypoints)(
                self.raw,
                profile,
                entrypoints.as_mut_ptr(),
                &raw mut found,
            )
        };
        self.va.check(status)?;
        Ok(entrypoints
            .get(..count(found))
            .unwrap_or(&[])
            .contains(&VAEntrypointEncSlice))
    }
}

impl Drop for Display<'_> {
    fn drop(&mut self) {
        // SAFETY: the display came from a successful initialise and is
        // terminated once. The descriptor is closed after, never before: the
        // driver holds it for the display's life.
        unsafe {
            (self.va.terminate)(self.raw);
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The open-stack device on this machine. Not the one driving the display,
    /// which is the point: this backend is the non-zero-copy one here.
    const NODE: &CStr = c"/dev/dri/renderD128";

    #[test]
    fn profiles_are_listed_best_first() {
        // High before Main, because every decoder that matters takes High and
        // it is what a peer expects.
        assert_eq!(Codec::H264.profiles()[0], VAProfileH264High);
        assert!(Codec::H264.profiles().contains(&VAProfileH264Main));
        assert_eq!(Codec::H265.profiles(), &[VAProfileHEVCMain]);
    }

    /// What the driver offers, and a context built on it.
    ///
    /// The packed-header answer bounds how much code this backend may need,
    /// but does not decide it: it reports what the driver will accept, not
    /// what it requires. Which of those applies is settled by encoding a frame
    /// and seeing whether parameter sets come out unasked.
    #[test]
    #[ignore = "requires the open-stack driver"]
    fn the_driver_reports_what_it_will_do_and_a_context_builds() {
        let va = Vaapi::load().expect("runtime");
        let display = va.open(NODE).expect("render node");

        for codec in [Codec::H264, Codec::H265] {
            let caps = display.caps(codec).expect("caps");
            println!(
                "{codec:?}: profile {} rate-control {:#x} packed-headers {:#x} \
                 (variable rate {}, accepts our headers {})",
                caps.profile,
                caps.rate_control,
                caps.packed_headers,
                caps.has_variable_rate(),
                caps.accepts_packed_headers()
            );
            assert!(
                caps.has_variable_rate(),
                "no bitrate-carrying rate control: the congestion actuator cannot exist"
            );

            let context = display
                .create_context(caps, 1920, 1080, 4)
                .expect("context");
            assert_eq!(context.surfaces().len(), 4);
            assert!(context.surfaces().iter().all(|id| *id != 0));
            println!("  context built with {} surfaces", context.surfaces().len());
        }
    }

    /// Needs the open-stack driver, so it is off by default. Run with
    /// `cargo test -p lowlat-encode -- --ignored`.
    #[test]
    #[ignore = "requires the open-stack driver"]
    fn the_display_opens_and_reports_an_encoder() {
        let va = match Vaapi::load() {
            Ok(va) => va,
            Err(Error::Unavailable) => panic!("no runtime present; this test needs the driver"),
            Err(error) => panic!("{error}"),
        };
        let display = va.open(NODE).expect("render node did not open");
        let (major, minor) = display.version();
        println!("display interface {major}.{minor}");
        assert!(major >= 1);

        for codec in [Codec::H264, Codec::H265] {
            let profile = display.encode_profile(codec).expect("no encode profile");
            println!("  {codec:?} encodes with profile {profile}");
        }

        // A profile the driver does not encode must be refused rather than
        // substituted, which is what makes the answers above mean anything.
        assert!(
            !display
                .has_encode_entrypoint(crate::ffi::va::VAProfileNone)
                .expect("query"),
            "the driver claims an encode entry point for no profile at all"
        );
    }
}

/// What the driver will do for one codec, asked rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub profile: VAProfile,
    /// Rate control modes offered, as the interface's bit set.
    pub rate_control: u32,
    /// Which headers the driver will **accept** from the caller.
    ///
    /// Read this as a capability and not as a requirement: it says what may be
    /// supplied, not what must be. Whether the driver emits parameter sets on
    /// its own when none are supplied is not in this answer and is settled by
    /// encoding a frame and looking at what comes out. Treating the two as the
    /// same thing is how a backend ends up with a hand-written parameter set
    /// generator it never needed, or without one it did.
    pub packed_headers: u32,
}

impl Caps {
    /// Live bitrate change needs a rate control mode that has a bitrate.
    /// Constant quantiser has none, so a device offering only that cannot
    /// carry the congestion actuator at all.
    pub fn has_variable_rate(&self) -> bool {
        self.rate_control & (VA_RC_CBR | VA_RC_VBR) != 0
    }

    /// True when the driver will take parameter sets from the caller.
    ///
    /// Not the same as needing them. See [`Caps::packed_headers`].
    pub fn accepts_packed_headers(&self) -> bool {
        self.packed_headers & (VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE) != 0
    }
}

/// A configured encode context with its surface pool.
#[derive(Debug)]
pub struct Context<'a> {
    display: &'a Display<'a>,
    config: VAConfigID,
    context: VAContextID,
    surfaces: Vec<VASurfaceID>,
}

impl Display<'_> {
    /// Ask what the driver offers for a codec.
    pub fn caps(&self, codec: Codec) -> Result<Caps> {
        let profile = self.encode_profile(codec)?;
        let mut attribs = [
            VAConfigAttrib {
                type_: VAConfigAttribRTFormat,
                value: 0,
            },
            VAConfigAttrib {
                type_: VAConfigAttribRateControl,
                value: 0,
            },
            VAConfigAttrib {
                type_: VAConfigAttribEncPackedHeaders,
                value: 0,
            },
        ];
        // SAFETY: the array is writable for the length passed.
        let status = unsafe {
            (self.va.get_config_attributes)(
                self.raw,
                profile,
                VAEntrypointEncSlice,
                attribs.as_mut_ptr(),
                c_int::try_from(attribs.len()).unwrap_or(0),
            )
        };
        self.va.check(status)?;

        // An unsupported attribute comes back with a sentinel rather than
        // zero, and treating that as a bit set would read every bit as set.
        let value = |attrib: &VAConfigAttrib| {
            if attrib.value == VA_ATTRIB_NOT_SUPPORTED {
                0
            } else {
                attrib.value
            }
        };
        if value(&attribs[0]) & VA_RT_FORMAT_YUV420 == 0 {
            return Err(Error::NoEncoder);
        }
        Ok(Caps {
            profile,
            rate_control: value(&attribs[1]),
            packed_headers: value(&attribs[2]),
        })
    }

    /// Build an encode context and its surface pool.
    pub fn create_context(
        &self,
        caps: Caps,
        width: u32,
        height: u32,
        surfaces: usize,
    ) -> Result<Context<'_>> {
        // Only the two the pipeline depends on are requested. Asking for more
        // than is needed is how a configuration fails on one device for a
        // reason unrelated to anything it does.
        let mut wanted = [
            VAConfigAttrib {
                type_: VAConfigAttribRTFormat,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: VAConfigAttribRateControl,
                value: if caps.rate_control & VA_RC_VBR != 0 {
                    VA_RC_VBR
                } else if caps.rate_control & VA_RC_CBR != 0 {
                    VA_RC_CBR
                } else {
                    VA_RC_CQP
                },
            },
        ];

        let mut config: VAConfigID = 0;
        // SAFETY: the array is readable for the length passed and the output
        // is a live local.
        let status = unsafe {
            (self.va.create_config)(
                self.raw,
                caps.profile,
                VAEntrypointEncSlice,
                wanted.as_mut_ptr(),
                c_int::try_from(wanted.len()).unwrap_or(0),
                &raw mut config,
            )
        };
        self.va.check(status)?;

        let mut pool = vec![0 as VASurfaceID; surfaces];
        // SAFETY: the pool is writable for its own length. No surface
        // attributes: the runtime format above already fixes the layout, and
        // an explicit attribute list is where a driver-specific refusal comes
        // from.
        let status = unsafe {
            (self.va.create_surfaces)(
                self.raw,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                pool.as_mut_ptr(),
                c_uint::try_from(pool.len()).unwrap_or(0),
                core::ptr::null_mut(),
                0,
            )
        };
        if let Err(error) = self.va.check(status) {
            // SAFETY: the configuration was created above and nothing else
            // owns it.
            unsafe { (self.va.destroy_config)(self.raw, config) };
            return Err(error);
        }

        let mut context: VAContextID = 0;
        // SAFETY: the pool is readable for its length and outlives the call;
        // it is retained in the returned value.
        let status = unsafe {
            (self.va.create_context)(
                self.raw,
                config,
                c_int::try_from(width).unwrap_or(0),
                c_int::try_from(height).unwrap_or(0),
                c_int::try_from(VA_PROGRESSIVE).unwrap_or(0),
                pool.as_mut_ptr(),
                c_int::try_from(pool.len()).unwrap_or(0),
                &raw mut context,
            )
        };
        if let Err(error) = self.va.check(status) {
            // SAFETY: both were created above and nothing else owns them.
            unsafe {
                (self.va.destroy_surfaces)(
                    self.raw,
                    pool.as_mut_ptr(),
                    c_int::try_from(pool.len()).unwrap_or(0),
                );
                (self.va.destroy_config)(self.raw, config);
            }
            return Err(error);
        }

        Ok(Context {
            display: self,
            config,
            context,
            surfaces: pool,
        })
    }
}

impl Context<'_> {
    pub fn raw(&self) -> VAContextID {
        self.context
    }

    /// The surface pool, one entry per in-flight picture.
    pub fn surfaces(&self) -> &[VASurfaceID] {
        &self.surfaces
    }
}

impl Drop for Context<'_> {
    /// Torn down innermost first: the context is built on the surfaces and the
    /// configuration, so both outlive it.
    fn drop(&mut self) {
        let va = self.display.va;
        let display = self.display.raw;
        // SAFETY: each was created once by `create_context` and is destroyed
        // once here, in the reverse of the order it was built.
        unsafe {
            (va.destroy_context)(display, self.context);
            (va.destroy_surfaces)(
                display,
                self.surfaces.as_mut_ptr(),
                c_int::try_from(self.surfaces.len()).unwrap_or(0),
            );
            (va.destroy_config)(display, self.config);
        }
    }
}
