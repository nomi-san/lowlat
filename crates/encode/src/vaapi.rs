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
    VABufferID, VABufferType, VAConfigAttrib, VAConfigAttribEncPackedHeaders,
    VAConfigAttribRTFormat, VAConfigAttribRateControl, VAConfigID, VAContextID, VADisplay,
    VAEntrypoint, VAEntrypointEncSlice, VAProfile, VAProfileH264High, VAProfileH264Main,
    VAProfileHEVCMain, VAStatus, VASurfaceID,
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
type CreateBuffer = unsafe extern "C" fn(
    VADisplay,
    VAContextID,
    VABufferType,
    c_uint,
    c_uint,
    *mut core::ffi::c_void,
    *mut VABufferID,
) -> VAStatus;
type DestroyBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
type MapBuffer =
    unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut core::ffi::c_void) -> VAStatus;
type UnmapBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
type BeginPicture = unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
type RenderPicture =
    unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus;
type EndPicture = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
type SyncSurface = unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus;
/// Address a surface's own storage rather than allocating a second copy of
/// it. The alternative pair creates an image and copies into the surface,
/// which is a whole frame of memory traffic per picture to avoid asking.
type DeriveImage =
    unsafe extern "C" fn(VADisplay, VASurfaceID, *mut crate::ffi::va::VAImage) -> VAStatus;
type DestroyImage = unsafe extern "C" fn(VADisplay, crate::ffi::va::VAImageID) -> VAStatus;
/// Present from interface 1.15 onward, and the reason the collect can be a
/// probe rather than a wait. Absent on an older runtime, where the surface
/// sync above is the only option and it blocks.
type SyncBuffer = unsafe extern "C" fn(VADisplay, VABufferID, u64) -> VAStatus;

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
    /// As many pictures are in flight as there are surfaces. Back pressure,
    /// not a fault.
    QueueFull,
    /// The driver has no encode entry point for any codec we speak. A decode
    /// only device reaches here, and it is a refusal rather than a fault.
    NoEncoder,
    /// A surface was offered in a layout this backend does not write, or its
    /// reported extent does not cover the frame. Refused rather than filled
    /// in on a guess, because a wrong guess is a picture that decodes to
    /// something plausible and wrong.
    UnsupportedLayout,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("display runtime not present"),
            Self::MissingSymbol => f.write_str("display runtime is missing an entry point"),
            Self::NoDevice => f.write_str("render node could not be opened"),
            Self::Status(status) => write!(f, "display runtime returned status {status}"),
            Self::QueueFull => f.write_str("every surface is in flight"),
            Self::NoEncoder => f.write_str("device offers no encode entry point"),
            Self::UnsupportedLayout => f.write_str("surface layout cannot be written"),
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
    create_buffer: CreateBuffer,
    destroy_buffer: DestroyBuffer,
    map_buffer: MapBuffer,
    unmap_buffer: UnmapBuffer,
    begin_picture: BeginPicture,
    render_picture: RenderPicture,
    end_picture: EndPicture,
    sync_surface: SyncSurface,
    derive_image: DeriveImage,
    destroy_image: DestroyImage,
    /// Optional: an older runtime does not export it.
    sync_buffer: Option<SyncBuffer>,
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
                create_buffer: libva
                    .symbol(c"vaCreateBuffer")
                    .ok_or(Error::MissingSymbol)?,
                destroy_buffer: libva
                    .symbol(c"vaDestroyBuffer")
                    .ok_or(Error::MissingSymbol)?,
                map_buffer: libva.symbol(c"vaMapBuffer").ok_or(Error::MissingSymbol)?,
                unmap_buffer: libva.symbol(c"vaUnmapBuffer").ok_or(Error::MissingSymbol)?,
                begin_picture: libva
                    .symbol(c"vaBeginPicture")
                    .ok_or(Error::MissingSymbol)?,
                render_picture: libva
                    .symbol(c"vaRenderPicture")
                    .ok_or(Error::MissingSymbol)?,
                end_picture: libva.symbol(c"vaEndPicture").ok_or(Error::MissingSymbol)?,
                sync_surface: libva.symbol(c"vaSyncSurface").ok_or(Error::MissingSymbol)?,
                derive_image: libva.symbol(c"vaDeriveImage").ok_or(Error::MissingSymbol)?,
                destroy_image: libva
                    .symbol(c"vaDestroyImage")
                    .ok_or(Error::MissingSymbol)?,
                // Absent before interface 1.15, and its absence is a
                // capability question rather than a fault.
                sync_buffer: libva.symbol(c"vaSyncBuffer"),
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

            // **The two pools must not intersect.** A picture read from the
            // same surface it reconstructs into makes the driver release the
            // buffer it is about to read, and the fault surfaces pictures
            // later, inside the driver, with nothing naming a surface.
            assert_eq!(context.recon().len(), context.surfaces().len());
            assert!(
                context
                    .recon()
                    .iter()
                    .all(|id| !context.surfaces().contains(id)),
                "a surface is serving as both the source and its own reconstruction"
            );
            println!("  context built with {} surfaces", context.surfaces().len());
        }
    }

    /// Encode real pictures, and answer three things at once: whether this
    /// backend's collect can ask rather than wait, whether the driver takes
    /// our parameter sets, and whether the colour description reads back the
    /// way it is meant to.
    ///
    /// The last one is the check the parameter-set writer has been owed since
    /// it was written: it is verified by a decoder that knows nothing about
    /// how these bytes were produced.
    #[test]
    #[ignore = "requires the open-stack driver"]
    fn it_encodes_and_the_stream_says_what_we_wrote() {
        use std::time::Instant;

        let va = Vaapi::load().expect("runtime");
        let display = va.open(NODE).expect("render node");
        let caps = display.caps(Codec::H264).expect("caps");
        let context = display
            .create_context(caps, 1920, 1080, 4)
            .expect("context");

        let params = crate::h264::Params {
            width: 1920,
            height: 1080,
            fps: 60,
            level_idc: 42,
            log2_max_frame_num_minus4: 0,
            log2_max_poc_lsb_minus4: 0,
            max_num_ref_frames: 1,
        };
        let mut encoder = context.encoder(params, 20_000_000).expect("encoder");
        println!("collect is a probe: {}", encoder.collect_is_a_probe());
        let mut source = lowlat_capture::synthetic::Synthetic::new(1920, 1080);

        let mut stream = Vec::new();
        let mut slowest_pending = std::time::Duration::ZERO;
        let mut pending = 0usize;
        let mut polls = 0usize;

        // The whole pool in flight, which is the shape the pipeline runs in.
        // The knob stays so a single picture can be compared against a full
        // queue, which is how the depth question was settled once already.
        let burst: usize = std::env::var("LOWLAT_VA_BURST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        for _ in 0..burst {
            encoder.submit(&source.acquire()).expect("submit");
        }
        // **More pictures than the pool is deep**, so the run refills behind
        // the drain. At four the queue is filled once and emptied once, and
        // anything that only affects a later submission -- a rate change, for
        // one -- silently affects nothing at all.
        const PICTURES: usize = 8;

        let drain_started = Instant::now();
        let mut collected = 0usize;
        let mut submitted = burst;
        while collected < PICTURES {
            if submitted < PICTURES && encoder.in_flight() < burst {
                encoder.submit(&source.acquire()).expect("submit");
                submitted += 1;
            }
            let started = Instant::now();
            let polled = encoder.poll().expect("poll");
            let took = started.elapsed();
            polls += 1;
            match polled {
                Poll::Ready { bitstream } => {
                    assert!(!bitstream.is_empty(), "an empty picture");
                    stream.extend_from_slice(bitstream);
                    collected += 1;
                }
                Poll::Pending => {
                    pending += 1;
                    slowest_pending = slowest_pending.max(took);
                    std::hint::spin_loop();
                }
            }

            // The rate moves mid-run. **What this proves is that the encoder
            // keeps producing valid pictures across the change**, not that no
            // refresh was emitted: every picture here is a refresh already, so
            // the gate wanting zero keyframes across a reconfigure cannot be
            // stated on this backend until predicted pictures exist.
            if collected == 2 {
                encoder.reconfigure(10_000_000);
            }
        }
        let drain = drain_started.elapsed();
        println!(
            "drain of {collected} {drain:?}, polls {polls} of which {pending} pending \
             (slowest pending {slowest_pending:?}), {} bytes",
            stream.len()
        );

        // A parameter set leads, which means the driver took ours.
        assert_eq!(&stream[..4], &[0, 0, 0, 1], "no start code");
        assert_eq!(
            stream[4] & 0x1F,
            7,
            "the stream does not open with our sequence set"
        );

        // **Every picture is there, and carries its own sets.** Checking only
        // the leading unit passes a stream that emitted one picture and then
        // stopped, which is exactly the failure this backend had. Escaping
        // makes a three-byte start code impossible inside a payload, so
        // counting them is exact rather than approximate.
        let mut units = [0usize; 32];
        for window in stream.windows(4) {
            if window[..3] == [0, 0, 1] {
                units[usize::from(window[3] & 0x1F)] += 1;
            }
        }
        assert_eq!(units[7], collected, "a sequence set per picture");
        assert_eq!(units[8], collected, "a picture set per picture");
        assert_eq!(units[5], collected, "a refresh slice per picture");

        let path = std::env::var("LOWLAT_DUMP").unwrap_or_else(|_| "/tmp/vaapi.h264".into());
        std::fs::write(&path, &stream).expect("write");
        println!("wrote {path}");
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

/// A configured encode context with its two surface pools.
#[derive(Debug)]
pub struct Context<'a> {
    display: &'a Display<'a>,
    config: VAConfigID,
    context: VAContextID,
    /// Where a picture's pixels are read from.
    surfaces: Vec<VASurfaceID>,
    /// Where its reconstruction is written. Never the same surface that the
    /// picture was read from; see [`Display::create_context`].
    recon: Vec<VASurfaceID>,
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

        // **Two pools, and a picture must never take both roles from one
        // surface.** The surface a picture begins on is where its pixels are
        // read from; the surface named in its picture parameters is where the
        // reconstruction is written. Given one surface for both, the driver
        // releases the buffer it is about to read as it takes the surface into
        // its reference store, and encodes from the freed pointer. The first
        // picture survives whenever the allocator hands the same block back,
        // so this presents as an intermittent fault several pictures in.
        let mut pool = match self.surface_pool(width, height, surfaces) {
            Ok(pool) => pool,
            Err(error) => {
                // SAFETY: the configuration was created above and nothing else
                // owns it.
                unsafe { (self.va.destroy_config)(self.raw, config) };
                return Err(error);
            }
        };
        let mut recon = match self.surface_pool(width, height, surfaces) {
            Ok(recon) => recon,
            Err(error) => {
                self.destroy_pool(&mut pool);
                // SAFETY: as above.
                unsafe { (self.va.destroy_config)(self.raw, config) };
                return Err(error);
            }
        };

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
            self.destroy_pool(&mut recon);
            self.destroy_pool(&mut pool);
            // SAFETY: the configuration was created above and nothing else
            // owns it.
            unsafe { (self.va.destroy_config)(self.raw, config) };
            return Err(error);
        }

        Ok(Context {
            display: self,
            config,
            context,
            surfaces: pool,
            recon,
        })
    }

    /// One pool of `count` surfaces in the encoder's runtime format.
    fn surface_pool(&self, width: u32, height: u32, count: usize) -> Result<Vec<VASurfaceID>> {
        let mut pool = vec![0 as VASurfaceID; count];
        // SAFETY: the pool is writable for its own length. No surface
        // attributes: the runtime format already fixes the layout, and an
        // explicit attribute list is where a driver-specific refusal comes
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
        self.va.check(status)?;
        Ok(pool)
    }

    fn destroy_pool(&self, pool: &mut [VASurfaceID]) {
        // SAFETY: every entry came from a successful create.
        unsafe {
            (self.va.destroy_surfaces)(
                self.raw,
                pool.as_mut_ptr(),
                c_int::try_from(pool.len()).unwrap_or(0),
            );
        }
    }
}

impl Context<'_> {
    pub fn raw(&self) -> VAContextID {
        self.context
    }

    /// The input pool, one entry per in-flight picture.
    pub fn surfaces(&self) -> &[VASurfaceID] {
        &self.surfaces
    }

    /// The reconstruction pool, paired by index with the input pool.
    pub fn recon(&self) -> &[VASurfaceID] {
        &self.recon
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
        }
        self.display.destroy_pool(&mut self.recon);
        self.display.destroy_pool(&mut self.surfaces);
        // SAFETY: created once above and destroyed once here.
        unsafe {
            (va.destroy_config)(display, self.config);
        }
    }
}

/// Copy `rows` of `row_bytes` into a destination laid out at its own stride.
///
/// Both sides are walked by their own stride, which is the whole reason this
/// is a row loop rather than one copy.
fn copy_plane(
    destination: &mut [u8],
    offset: usize,
    stride: usize,
    source: lowlat_capture::Plane<'_>,
    row_bytes: usize,
    rows: usize,
) -> Result<()> {
    if stride < row_bytes {
        return Err(Error::UnsupportedLayout);
    }
    for row in 0..rows {
        let from = source
            .row(row)
            .and_then(|full| full.get(..row_bytes))
            .ok_or(Error::UnsupportedLayout)?;
        let at = offset + row * stride;
        let into = destination
            .get_mut(at..at + row_bytes)
            .ok_or(Error::UnsupportedLayout)?;
        into.copy_from_slice(from);
    }
    Ok(())
}

/// What a collect found.
#[derive(Debug)]
pub enum Poll<'a> {
    Ready {
        bitstream: &'a [u8],
    },
    /// Nothing finished. **Not a wait**: the caller goes round its loop.
    Pending,
}

/// One picture in flight.
#[derive(Debug)]
struct InFlight {
    surface: VASurfaceID,
    coded: VABufferID,
}

/// An encoder over a context.
#[derive(Debug)]
pub struct Encoder<'a> {
    context: &'a Context<'a>,
    params: crate::h264::Params,
    bitrate_bps: u32,
    /// One coded buffer per surface, allocated once.
    coded: Vec<VABufferID>,
    next: usize,
    pending: std::collections::VecDeque<InFlight>,
    idr_pic_id: u16,
    collected: Vec<u8>,
}

impl<'a> Context<'a> {
    /// Build an encoder over this context.
    pub fn encoder(&'a self, params: crate::h264::Params, bitrate_bps: u32) -> Result<Encoder<'a>> {
        let va = self.display.va;
        let display = self.display.raw;
        // Generous: a keyframe of a hard scene is far larger than the average,
        // and a coded buffer too small fails the picture rather than truncating
        // it, which is the right failure but an avoidable one.
        let size = params.width * params.height * 3 / 2;

        let mut coded = Vec::with_capacity(self.surfaces.len());
        for _ in 0..self.surfaces.len() {
            let mut id: VABufferID = 0;
            // SAFETY: no initial data, so the pointer is null and the count is
            // one buffer of `size` bytes.
            let status = unsafe {
                (va.create_buffer)(
                    display,
                    self.context,
                    crate::ffi::va::VAEncCodedBufferType,
                    size,
                    1,
                    core::ptr::null_mut(),
                    &raw mut id,
                )
            };
            if let Err(error) = va.check(status) {
                for done in &coded {
                    // SAFETY: each was created above.
                    unsafe { (va.destroy_buffer)(display, *done) };
                }
                return Err(error);
            }
            coded.push(id);
        }

        Ok(Encoder {
            context: self,
            params,
            bitrate_bps,
            coded,
            next: 0,
            pending: std::collections::VecDeque::new(),
            idr_pic_id: 0,
            collected: Vec::new(),
        })
    }
}

impl Encoder<'_> {
    fn va(&self) -> &Vaapi {
        self.context.display.va
    }

    fn display(&self) -> VADisplay {
        self.context.display.raw
    }

    /// Create one parameter buffer holding `value`.
    fn buffer<T>(&self, kind: VABufferType, value: &T) -> Result<VABufferID> {
        let mut id: VABufferID = 0;
        // SAFETY: the interface copies `size` bytes from the pointer during
        // the call, and `value` is live for its duration.
        let status = unsafe {
            (self.va().create_buffer)(
                self.display(),
                self.context.context,
                kind,
                c_uint::try_from(size_of::<T>()).unwrap_or(0),
                1,
                (value as *const T as *mut T).cast(),
                &raw mut id,
            )
        };
        self.va().check(status)?;
        Ok(id)
    }

    /// A packed header, which is two buffers: what it is, then what it says.
    fn packed(
        &self,
        kind: u32,
        bytes: &[u8],
        bit_length: usize,
    ) -> Result<(VABufferID, VABufferID)> {
        let parameter = crate::ffi::va::VAEncPackedHeaderParameterBuffer {
            type_: kind,
            bit_length: u32::try_from(bit_length).unwrap_or(0),
            // The escaping is already applied, and saying otherwise makes the
            // driver apply it a second time.
            has_emulation_bytes: 1,
            va_reserved: [0; 4],
        };
        let head = self.buffer(
            crate::ffi::va::VAEncPackedHeaderParameterBufferType,
            &parameter,
        )?;

        let mut data: VABufferID = 0;
        // SAFETY: the interface copies the bytes during the call.
        let status = unsafe {
            (self.va().create_buffer)(
                self.display(),
                self.context.context,
                crate::ffi::va::VAEncPackedHeaderDataBufferType,
                c_uint::try_from(bytes.len()).unwrap_or(0),
                1,
                bytes.as_ptr().cast_mut().cast(),
                &raw mut data,
            )
        };
        self.va().check(status)?;
        Ok((head, data))
    }

    /// Write a frame into one of our input surfaces.
    ///
    /// **The surface's own strides are used, never the frame's.** A driver
    /// lays a surface out to its own alignment, and the two chroma rows are
    /// not necessarily where a tightly packed frame would put them. Walking
    /// the destination by the source's stride reads correctly for the first
    /// row and drifts further out of line with every row after it, which
    /// looks like a skew or a shear rather than like a stride fault.
    fn upload(&self, surface: VASurfaceID, frame: &lowlat_capture::Frame<'_>) -> Result<()> {
        // SAFETY: plain data, filled by the call below.
        let mut image = unsafe { core::mem::zeroed::<crate::ffi::va::VAImage>() };
        // SAFETY: the surface is live and the output is a live local.
        let status = unsafe { (self.va().derive_image)(self.display(), surface, &raw mut image) };
        self.va().check(status)?;

        let result = self.write_image(&image, frame);

        // SAFETY: derived above; released once whether or not the write
        // succeeded, or the surface stays mapped for the life of the display.
        unsafe { (self.va().destroy_image)(self.display(), image.image_id) };
        result
    }

    fn write_image(
        &self,
        image: &crate::ffi::va::VAImage,
        frame: &lowlat_capture::Frame<'_>,
    ) -> Result<()> {
        // Two planes, luma then interleaved chroma. A driver offering some
        // other layout is refused rather than filled in wrongly.
        if image.num_planes < 2 || image.format.fourcc != crate::ffi::va::VA_FOURCC_NV12 {
            return Err(Error::UnsupportedLayout);
        }

        let mut mapped: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: the image's buffer is live until the image is destroyed.
        let status = unsafe { (self.va().map_buffer)(self.display(), image.buf, &raw mut mapped) };
        self.va().check(status)?;
        if mapped.is_null() {
            return Err(Error::UnsupportedLayout);
        }

        // SAFETY: the interface reports `data_size` writable bytes at the
        // mapped address, and nothing else aliases them while mapped.
        let destination = unsafe {
            core::slice::from_raw_parts_mut(
                mapped.cast::<u8>(),
                usize::try_from(image.data_size).unwrap_or(0),
            )
        };

        let rows = usize::try_from(frame.height).unwrap_or(0);
        let result = copy_plane(
            destination,
            usize::try_from(image.offsets[0]).unwrap_or(0),
            usize::try_from(image.pitches[0]).unwrap_or(0),
            frame.luma,
            usize::try_from(frame.width).unwrap_or(0),
            rows,
        )
        .and_then(|()| {
            copy_plane(
                destination,
                usize::try_from(image.offsets[1]).unwrap_or(0),
                usize::try_from(image.pitches[1]).unwrap_or(0),
                frame.chroma,
                usize::try_from(frame.width.div_ceil(2) * 2).unwrap_or(0),
                rows.div_ceil(2),
            )
        });

        // SAFETY: balances the map above.
        unsafe { (self.va().unmap_buffer)(self.display(), image.buf) };
        result
    }

    /// Encode one picture. Returns as soon as it is queued.
    ///
    /// Every picture here is a keyframe for now: the reference-picture
    /// bookkeeping a predicted picture needs is the next piece, and shipping a
    /// half-correct reference list would produce a stream that decodes into
    /// progressively wrong output rather than failing.
    pub fn submit(&mut self, frame: &lowlat_capture::Frame<'_>) -> Result<()> {
        if self.pending.len() >= self.coded.len() {
            return Err(Error::QueueFull);
        }
        let slot = self.next;
        self.next = (self.next + 1) % self.coded.len();
        let input = *self.context.surfaces.get(slot).ok_or(Error::NoEncoder)?;
        let recon = *self.context.recon.get(slot).ok_or(Error::NoEncoder)?;
        let coded = self.coded[slot];

        // **One input surface per slot, so this cannot overwrite a picture
        // that is still being encoded.** The slot is only reused once its
        // picture has been collected, which is what the queue-full check
        // above enforces.
        self.upload(input, frame)?;

        // The picture is read from the input surface and reconstructed into
        // its counterpart. The two are never the same surface.
        // SAFETY: the display and context are live for the call.
        let status =
            unsafe { (self.va().begin_picture)(self.display(), self.context.context, input) };
        self.va().check(status)?;

        let mut seq =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncSequenceParameterBufferH264>() };
        seq.level_idc = u8::try_from(self.params.level_idc).unwrap_or(42);
        // The sequence has to declare the same thing the pictures do. Every
        // picture here is an instantaneous refresh, so the refresh period is
        // one; leaving it zero says "never" while each picture says "now", and
        // the driver acts on the sequence's answer.
        seq.intra_period = 1;
        seq.intra_idr_period = 1;
        seq.ip_period = 1;
        seq.bits_per_second = self.bitrate_bps;
        seq.max_num_ref_frames = self.params.max_num_ref_frames;
        seq.picture_width_in_mbs = u16::try_from(self.params.width.div_ceil(16)).unwrap_or(0);
        seq.picture_height_in_mbs = u16::try_from(self.params.height.div_ceil(16)).unwrap_or(0);
        // SAFETY: the flags are a union of a bitfield view and a plain word.
        // The structure was zeroed, so either view is a valid value of it, and
        // only the bitfield view is used from here on.
        unsafe {
            let bits = &mut seq.seq_fields.bits;
            bits.set_chroma_format_idc(1);
            bits.set_frame_mbs_only_flag(1);
            bits.set_direct_8x8_inference_flag(1);
            bits.set_log2_max_frame_num_minus4(self.params.log2_max_frame_num_minus4);
            bits.set_pic_order_cnt_type(0);
            bits.set_log2_max_pic_order_cnt_lsb_minus4(self.params.log2_max_poc_lsb_minus4);
        }
        let bottom = (self.params.height.div_ceil(16) * 16 - self.params.height) / 2;
        if bottom > 0 {
            seq.frame_cropping_flag = 1;
            seq.frame_crop_bottom_offset = bottom;
        }
        let seq_buffer = self.buffer(crate::ffi::va::VAEncSequenceParameterBufferType, &seq)?;

        // **The rate the encoder actually runs at is carried here, not in the
        // sequence parameters.** The sequence buffer has a field for it and
        // one driver never reads that field, so a rate set only there leaves
        // the encoder on its own default with nothing reporting a problem.
        // The field above is kept because a different driver does read it.
        #[repr(C)]
        struct RateControl {
            header: crate::ffi::va::VAEncMiscParameterBuffer,
            rate: crate::ffi::va::VAEncMiscParameterRateControl,
        }
        // SAFETY: a plain-data header followed by its payload, which is the
        // layout the interface specifies for this buffer. All-zero is a valid
        // value of every field, and the ones that matter are set below.
        let mut rate = unsafe { core::mem::zeroed::<RateControl>() };
        rate.header.type_ = crate::ffi::va::VAEncMiscParameterTypeRateControl;
        rate.rate.bits_per_second = self.bitrate_bps;
        // A variable-rate target is this percentage of the peak above. Zero
        // asks for a target of nothing, which is what a zeroed buffer says.
        rate.rate.target_percentage = 100;
        rate.rate.window_size = 1000;
        let rate_buffer = self.buffer(crate::ffi::va::VAEncMiscParameterBufferType, &rate)?;

        // **Both parameter sets go into one header of sequence type, and the
        // picture type is never used at all.** The interface names a type per
        // set, which invites one header each; a working encoder on this driver
        // sends them together and never sends the picture type, and sending it
        // is what the driver faults on while assembling the stream.
        let mut sets = [0u8; 384];
        let sps_len =
            crate::h264::sequence_parameter_set(&self.params, &mut sets).ok_or(Error::NoEncoder)?;
        let pps_len =
            crate::h264::picture_parameter_set(sets.get_mut(sps_len..).ok_or(Error::NoEncoder)?)
                .ok_or(Error::NoEncoder)?;
        let sets_len = sps_len + pps_len;
        let (sets_head, sets_data) = self.packed(
            crate::ffi::va::VAEncPackedHeaderSequence,
            &sets[..sets_len],
            sets_len * 8,
        )?;

        let mut pic =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncPictureParameterBufferH264>() };
        pic.CurrPic.picture_id = recon;
        pic.CurrPic.frame_idx = 0;
        pic.CurrPic.TopFieldOrderCnt = 0;
        pic.CurrPic.BottomFieldOrderCnt = pic.CurrPic.TopFieldOrderCnt;
        // Unused entries must be marked invalid, not left zero: zero is a
        // valid surface identifier and the driver would follow it.
        for entry in &mut pic.ReferenceFrames {
            entry.picture_id = crate::ffi::va::VA_INVALID_SURFACE;
            entry.flags = crate::ffi::va::VA_PICTURE_H264_INVALID;
        }
        pic.coded_buf = coded;
        pic.frame_num = 0;
        pic.pic_init_qp = 26;
        // SAFETY: as above, a zeroed union accessed only through one view.
        unsafe {
            let bits = &mut pic.pic_fields.bits;
            bits.set_idr_pic_flag(1);
            bits.set_reference_pic_flag(1);
            bits.set_entropy_coding_mode_flag(1);
            bits.set_deblocking_filter_control_present_flag(1);
            bits.set_transform_8x8_mode_flag(1);
        }
        let pic_buffer = self.buffer(crate::ffi::va::VAEncPictureParameterBufferType, &pic)?;

        let mut slice =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncSliceParameterBufferH264>() };
        slice.num_macroblocks = self.params.width.div_ceil(16) * self.params.height.div_ceil(16);
        // Two is an intra slice; the predicted types come with the reference
        // bookkeeping.
        slice.slice_type = 2;
        // Alternates so two consecutive refreshes are distinguishable, which
        // is what the field is for.
        slice.idr_pic_id = self.idr_pic_id;
        slice.pic_order_cnt_lsb = 0;
        for entry in slice
            .RefPicList0
            .iter_mut()
            .chain(slice.RefPicList1.iter_mut())
        {
            entry.picture_id = crate::ffi::va::VA_INVALID_SURFACE;
            entry.flags = crate::ffi::va::VA_PICTURE_H264_INVALID;
        }
        let slice_buffer = self.buffer(crate::ffi::va::VAEncSliceParameterBufferType, &slice)?;

        // **The driver expects one of these per picture once packed headers
        // are in use.** Without it there is no slice header to emit and the
        // stream cannot be assembled, which is a fault inside the driver
        // rather than a status.
        let mut header = [0u8; 64];
        let written =
            crate::h264::idr_slice_header(&self.params, &mut header).ok_or(Error::NoEncoder)?;
        let (header_head, header_data) = self.packed(
            crate::ffi::va::VAEncPackedHeaderSlice,
            &header[..written.bytes_written],
            written.bit_length,
        )?;

        // The order is load bearing twice over. The parameter sets declare the
        // field widths the slice header is then read with, and the picture
        // buffer decides which fields the slice header is expected to carry at
        // all, so both precede it.
        let mut buffers = [
            seq_buffer,
            rate_buffer,
            sets_head,
            sets_data,
            pic_buffer,
            slice_buffer,
            header_head,
            header_data,
        ];

        // SAFETY: the list is readable for its length.
        let status = unsafe {
            (self.va().render_picture)(
                self.display(),
                self.context.context,
                buffers.as_mut_ptr(),
                c_int::try_from(buffers.len()).unwrap_or(0),
            )
        };
        let rendered = self.va().check(status);

        // SAFETY: the picture began above.
        let status = unsafe { (self.va().end_picture)(self.display(), self.context.context) };
        let ended = self.va().check(status);

        // **These come back to us once the picture is closed.** The interface
        // reads them during the render and does not take them; left alone they
        // accumulate for the life of the context, eight per picture. Released
        // on the failing paths too, because a render that got as far as a
        // status still consumed them.
        for id in buffers {
            // SAFETY: each came from a successful create above and is
            // destroyed once.
            unsafe { (self.va().destroy_buffer)(self.display(), id) };
        }
        rendered?;
        ended?;

        // **Every picture here is an instantaneous refresh, so neither
        // counter advances.** A refresh resets both by definition: it carries
        // frame number zero and restarts the picture order. Incrementing them
        // while still flagging each picture as a refresh states two
        // contradictory things, and the driver acts on the contradiction
        // rather than rejecting it. Both counters start moving when predicted
        // pictures do.
        self.idr_pic_id ^= 1;
        self.pending.push_back(InFlight {
            surface: input,
            coded,
        });
        Ok(())
    }

    /// Collect a finished picture, or report that none is ready.
    ///
    /// **Asks rather than waits, where the runtime allows it.** From interface
    /// 1.15 the coded buffer can be synchronised with a timeout, and zero makes
    /// it a probe. Older runtimes offer only a surface synchronise, which
    /// blocks, and that is reported honestly by the capability below rather
    /// than hidden.
    pub fn poll(&mut self) -> Result<Poll<'_>> {
        let Some(head) = self.pending.front() else {
            return Ok(Poll::Pending);
        };
        let (surface, coded) = (head.surface, head.coded);

        if let Some(sync) = self.va().sync_buffer {
            // SAFETY: the buffer is live. Zero timeout: ask, do not wait.
            let status = unsafe { sync(self.display(), coded, 0) };
            if status == crate::ffi::va::VA_STATUS_ERROR_TIMEDOUT as VAStatus {
                return Ok(Poll::Pending);
            }
            self.va().check(status)?;
        } else {
            // SAFETY: the surface is live. This one blocks; see the note.
            let status = unsafe { (self.va().sync_surface)(self.display(), surface) };
            self.va().check(status)?;
        }

        let mut mapped: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: the buffer is live and the out pointer is a live local.
        let status = unsafe { (self.va().map_buffer)(self.display(), coded, &raw mut mapped) };
        self.va().check(status)?;

        self.collected.clear();
        let mut segment = mapped.cast::<crate::ffi::va::VACodedBufferSegment>();
        // The output is a chain, not one block. Reading only the first segment
        // truncates a picture that happened to span two, which shows up on
        // large frames alone.
        while !segment.is_null() {
            // SAFETY: the interface guarantees the chain while mapped.
            let entry = unsafe { &*segment };
            if !entry.buf.is_null() {
                // SAFETY: the segment reports `size` readable bytes at `buf`.
                let bytes = unsafe {
                    core::slice::from_raw_parts(entry.buf.cast::<u8>(), entry.size as usize)
                };
                self.collected.extend_from_slice(bytes);
            }
            segment = entry.next.cast();
        }

        // SAFETY: balances the map above.
        unsafe { (self.va().unmap_buffer)(self.display(), coded) };
        self.pending.pop_front();

        Ok(Poll::Ready {
            bitstream: &self.collected,
        })
    }

    /// Change the rate every picture from the next one onward is encoded at.
    ///
    /// **Neither reinitialises the encoder nor forces a refresh**, and cannot
    /// fail, because the rate is not encoder state here: it travels with each
    /// picture, so changing it is only a question of what the next picture
    /// carries. The congestion controller moves this many times a minute, and
    /// a reinitialisation at that cadence would be visible as a stutter every
    /// time the network hiccuped.
    pub fn reconfigure(&mut self, bitrate_bps: u32) {
        self.bitrate_bps = bitrate_bps;
    }

    /// True when the collect can ask rather than wait.
    pub fn collect_is_a_probe(&self) -> bool {
        self.va().sync_buffer.is_some()
    }

    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for Encoder<'_> {
    fn drop(&mut self) {
        let va = self.context.display.va;
        let display = self.context.display.raw;
        for id in &self.coded {
            // SAFETY: each came from a successful create.
            unsafe { (va.destroy_buffer)(display, *id) };
        }
    }
}
