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

use crate::{Encoder as EncoderTrait, Poll};

use crate::ffi::va::{
    VA_ATTRIB_NOT_SUPPORTED, VA_ENC_PACKED_HEADER_PICTURE, VA_ENC_PACKED_HEADER_SEQUENCE,
    VA_PROGRESSIVE, VA_RC_CBR, VA_RC_CQP, VA_RC_VBR, VA_RT_FORMAT_YUV420, VA_STATUS_SUCCESS,
    VABufferID, VABufferType, VAConfigAttrib, VAConfigAttribEncPackedHeaders,
    VAConfigAttribRTFormat, VAConfigAttribRateControl, VAConfigID, VAContextID, VADisplay,
    VAEntrypoint, VAEntrypointEncSlice, VAEntrypointEncSliceLP, VAProfile, VAProfileH264High,
    VAProfileH264Main, VAProfileHEVCMain, VAStatus, VASurfaceID,
};
use crate::ffi::va::{
    VA_FOURCC_NV12, VA_SURFACE_ATTRIB_SETTABLE, VAGenericValue, VAGenericValueTypeInteger,
    VAGenericValueTypePointer, VASurfaceAttrib, VASurfaceAttribExternalBufferDescriptor,
    VASurfaceAttribMemoryType,
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

/// What an encoder codes with, which is the codec and that codec's parameters
/// in one value.
///
/// **The two cannot be chosen separately.** Each codec's parameter sets are
/// written by hand and every field the device is told has a counterpart in
/// them, so a codec paired with the other one's parameters is a stream nothing
/// decodes rather than a configuration error.
#[derive(Debug, Clone, Copy)]
pub enum Params {
    H264(crate::h264::Params),
    H265(crate::h265::Params),
}

impl Params {
    pub fn codec(self) -> Codec {
        match self {
            Self::H264(_) => Codec::H264,
            Self::H265(_) => Codec::H265,
        }
    }

    fn width(self) -> u32 {
        match self {
            Self::H264(p) => p.width,
            Self::H265(p) => p.width,
        }
    }

    fn height(self) -> u32 {
        match self {
            Self::H264(p) => p.height,
            Self::H265(p) => p.height,
        }
    }

    /// How far the picture order count runs before it wraps.
    fn poc_period(self) -> u32 {
        let minus4 = match self {
            Self::H264(p) => p.log2_max_poc_lsb_minus4,
            Self::H265(p) => p.log2_max_poc_lsb_minus4,
        };
        1 << (minus4 + 4)
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
    pub fn encode_target(&self, codec: Codec) -> Result<(VAProfile, VAEntrypoint)> {
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
            if let Some(entrypoint) = self.encode_entrypoint(*wanted)? {
                return Ok((*wanted, entrypoint));
            }
        }
        Err(Error::NoEncoder)
    }

    fn max_profiles(&self) -> Result<usize> {
        // SAFETY: the display is live.
        Ok(count(unsafe { (self.va.max_num_profiles)(self.raw) }))
    }

    fn encode_entrypoint(&self, profile: VAProfile) -> Result<Option<VAEntrypoint>> {
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
        let offered = entrypoints.get(..count(found)).unwrap_or(&[]);
        // **The order is what a device already serving us keeps.** Where both
        // are offered the slice entry point is what every shipped run has
        // measured, and preferring the other one on that hardware would be a
        // change to the encode nobody asked for. Which of the two a device
        // that has both should use is a question for a measurement, not for
        // this list.
        Ok([VAEntrypointEncSlice, VAEntrypointEncSliceLP]
            .into_iter()
            .find(|entrypoint| offered.contains(entrypoint)))
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
    use std::ffi::CString;

    /// Which render node these tests run against.
    ///
    /// **Nameable, because a machine with two cards has an encoder on each and
    /// they do not answer the same.** A constant here can only ever measure
    /// whichever node the loader numbered first, and the second card is
    /// exactly where a backend's assumptions get found out. Set
    /// `LOWLAT_VAAPI_NODE` to point it elsewhere.
    fn node() -> CString {
        let named =
            std::env::var("LOWLAT_VAAPI_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
        CString::new(named).expect("a node path with no interior nul")
    }

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
        let display = va.open(&node()).expect("render node");

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
            // **Zero is a surface, not a failure.** The interface spells an
            // absent surface as its own sentinel, and one vendor's driver
            // numbers its pool from one while another numbers it from zero, so
            // reading zero as unallocated fails on the second for no reason.
            assert!(
                context
                    .surfaces()
                    .iter()
                    .all(|id| *id != crate::ffi::va::VA_INVALID_SURFACE)
            );

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
        let display = va.open(&node()).expect("render node");
        let caps = display.caps(Codec::H264).expect("caps");
        let context = display
            .create_context(caps, 1920, 1080, 4)
            .expect("context");

        let params = crate::h264::Params {
            width: 1920,
            height: 1080,
            fps: 60,
            level_idc: 42,
            // **Not zero, deliberately.** These size fixed-width fields in
            // every slice header, so a value the writer and the sequence set
            // disagree on shifts everything after them. Zero would wrap the
            // frame number every sixteen pictures and hide that; four is what
            // a real stream uses and exercises the width plumbing.
            log2_max_frame_num_minus4: 4,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
        };
        let mut encoder = context
            .encoder(Params::H264(params), 20_000_000)
            .expect("encoder");
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
            encoder.submit(&source.acquire(), false).expect("submit");
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
                encoder.submit(&source.acquire(), false).expect("submit");
                submitted += 1;
            }
            let started = Instant::now();
            let polled = encoder.poll().expect("poll");
            let took = started.elapsed();
            polls += 1;
            match polled {
                Poll::Ready { bitstream, .. } => {
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

            // The rate moves mid-run, and the assertions above are what make
            // it mean something: exactly one refresh in the whole run, so a
            // reconfigure that forced one would be caught here rather than
            // merely described.
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
        // **One refresh, and the rest predicted.** A backend that refreshed
        // every picture would pass a count of slices and fail this.
        assert_eq!(units[5], 1, "more than the one unavoidable refresh");
        assert_eq!(units[1], collected - 1, "the rest are not predicted slices");
        assert_eq!(
            units[7], 1,
            "a sequence set travelled with a predicted picture"
        );
        assert_eq!(
            units[8], 1,
            "a picture set travelled with a predicted picture"
        );

        let path = std::env::var("LOWLAT_DUMP").unwrap_or_else(|_| "/tmp/vaapi.h264".into());
        std::fs::write(&path, &stream).expect("write");
        println!("wrote {path}");
    }

    /// The second codec, through the same loop.
    ///
    /// **A half-correct path here encodes without error and decodes to
    /// nothing**, so the checks are on what the units say rather than on the
    /// calls returning success: which types came out, how many of each, and
    /// whether the driver took the three sets we wrote.
    #[test]
    #[ignore = "requires the open-stack driver"]
    fn the_second_codec_encodes_and_the_driver_takes_our_sets() {
        let va = Vaapi::load().expect("runtime");
        let display = va.open(&node()).expect("render node");
        let caps = display.caps(Codec::H265).expect("caps");
        // **The height is a knob because the rounding had to be measured.**
        // The device codes at its own alignment and corrects the size in the
        // sets it is handed, so what that alignment is cannot be read off one
        // resolution: 1080 and 1000 both round up, 1200 does not, and only the
        // three together name the unit. It stays so another device can be
        // asked the same question.
        let height: u32 = std::env::var("LOWLAT_PROBE_HEIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1080);
        let context = display
            .create_context(caps, 1920, height, 4)
            .expect("context");

        let params = crate::h265::Params {
            width: 1920,
            height,
            fps: 60,
            level_idc: 123,
            // As on the other codec: not zero, so the fixed-width order count
            // in every slice header is actually exercised.
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
        };
        let mut encoder = context
            .encoder(Params::H265(params), 20_000_000)
            .expect("encoder");
        let mut source = lowlat_capture::synthetic::Synthetic::new(1920, height);

        const PICTURES: usize = 8;
        const DEPTH: usize = 4;
        let mut stream = Vec::new();
        let mut collected = 0usize;
        let mut submitted = 0usize;
        while collected < PICTURES {
            if submitted < PICTURES && encoder.in_flight() < DEPTH {
                encoder.submit(&source.acquire(), false).expect("submit");
                submitted += 1;
            }
            match encoder.poll().expect("poll") {
                Poll::Ready { bitstream, .. } => {
                    assert!(!bitstream.is_empty(), "an empty picture");
                    stream.extend_from_slice(bitstream);
                    collected += 1;
                }
                Poll::Pending => std::hint::spin_loop(),
            }
        }
        println!("collected {collected} pictures, {} bytes", stream.len());

        // **The type is six bits of the byte after the start code on this
        // codec**, not five, so a reader written for the other one lands on a
        // plausible wrong answer rather than failing.
        let mut units = [0usize; 64];
        for window in stream.windows(4) {
            if window[..3] == [0, 0, 1] {
                units[usize::from((window[3] >> 1) & 0x3F)] += 1;
            }
        }
        assert_eq!(units[32], 1, "the video set is missing or repeated");
        assert_eq!(units[33], 1, "the sequence set is missing or repeated");
        assert_eq!(units[34], 1, "the picture set is missing or repeated");
        // One refresh, and the rest predicted. A backend refreshing every
        // picture would pass a count of slices and fail this.
        assert_eq!(
            units[usize::from(crate::h265::NAL_IDR_N_LP)],
            1,
            "more than the one unavoidable refresh"
        );
        assert_eq!(
            units[usize::from(crate::h265::NAL_TRAIL_R)],
            collected - 1,
            "the rest are not predicted slices"
        );

        let path = std::env::var("LOWLAT_DUMP").unwrap_or_else(|_| "/tmp/vaapi.h265".into());
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
        let display = va.open(&node()).expect("render node did not open");
        let (major, minor) = display.version();
        println!("display interface {major}.{minor}");
        assert!(major >= 1);

        for codec in [Codec::H264, Codec::H265] {
            let (profile, entrypoint) = display.encode_target(codec).expect("no encode profile");
            println!("  {codec:?} encodes with profile {profile} through entry point {entrypoint}");
        }

        // A profile the driver does not encode must be refused rather than
        // substituted, which is what makes the answers above mean anything.
        assert!(
            display
                .encode_entrypoint(crate::ffi::va::VAProfileNone)
                .expect("query")
                .is_none(),
            "the driver claims an encode entry point for no profile at all"
        );
    }
}

/// What the driver will do for one codec, asked rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub profile: VAProfile,
    /// Which entry point answered, and the one every later call must name.
    ///
    /// **A device offers one or the other and not always both.** The slice
    /// entry point runs the encode through the shader cores; the low-power one
    /// runs it on fixed function hardware. Newer parts of one vendor dropped
    /// the first entirely, so a backend that names only it reports no encoder
    /// on hardware that encodes perfectly well.
    pub entrypoint: VAEntrypoint,
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
        let (profile, entrypoint) = self.encode_target(codec)?;
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
                entrypoint,
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
            entrypoint,
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
                caps.entrypoint,
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

    /// Take a frame that already lives on this device as a surface.
    ///
    /// **The whole point of the open backend having a display source.** The
    /// conversion writes into an allocation on this same device and hands over
    /// a descriptor; importing it means the encoder reads those very bytes
    /// rather than a copy of them through system memory.
    ///
    /// The descriptor is borrowed for the call. The runtime duplicates what it
    /// keeps, so the caller still owns the one it passed in.
    pub fn import(
        &self,
        fd: std::os::fd::BorrowedFd<'_>,
        frame: &lowlat_capture::convert::Exported,
    ) -> Result<VASurfaceID> {
        use std::os::fd::AsRawFd;

        let bytes = u64::from(frame.pitch)
            .checked_mul(u64::from(frame.height))
            .and_then(|luma| luma.checked_mul(3))
            .map(|both| both / 2)
            .and_then(|size| u32::try_from(size).ok())
            .ok_or(Error::UnsupportedLayout)?;

        let blank = crate::ffi::va::VADRMPRIMESurfaceDescriptorLayer {
            drm_format: 0,
            num_planes: 0,
            object_index: [0; crate::ffi::va::VA_DRM_PRIME_PLANES],
            offset: [0; crate::ffi::va::VA_DRM_PRIME_PLANES],
            pitch: [0; crate::ffi::va::VA_DRM_PRIME_PLANES],
        };
        let mut descriptor = crate::ffi::va::VADRMPRIMESurfaceDescriptor {
            fourcc: VA_FOURCC_NV12,
            width: frame.width,
            height: frame.height,
            num_objects: 1,
            objects: [crate::ffi::va::VADRMPRIMESurfaceDescriptorObject {
                fd: fd.as_raw_fd(),
                size: bytes,
                drm_format_modifier: frame.modifier,
            }; crate::ffi::va::VA_DRM_PRIME_OBJECTS],
            num_layers: 1,
            layers: [blank; crate::ffi::va::VA_DRM_PRIME_LAYERS],
        };
        // **One layer of two planes, not two layers of one.** Both planes live
        // in the same allocation at offsets of our choosing, and describing
        // them as separate layers is how a driver is told they are separate
        // allocations, which they are not.
        descriptor.layers[0] = crate::ffi::va::VADRMPRIMESurfaceDescriptorLayer {
            drm_format: crate::ffi::va::DRM_FORMAT_NV12,
            num_planes: 2,
            object_index: [0; crate::ffi::va::VA_DRM_PRIME_PLANES],
            offset: [frame.planes[0].offset, frame.planes[1].offset, 0, 0],
            pitch: [frame.planes[0].pitch, frame.planes[1].pitch, 0, 0],
        };

        let mut attributes = [
            VASurfaceAttrib {
                type_: VASurfaceAttribMemoryType,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    type_: VAGenericValueTypeInteger,
                    value: crate::ffi::va::_VAGenericValue__bindgen_ty_1 {
                        i: i32::from_ne_bytes(
                            crate::ffi::va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2.to_ne_bytes(),
                        ),
                    },
                },
            },
            VASurfaceAttrib {
                type_: VASurfaceAttribExternalBufferDescriptor,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    type_: VAGenericValueTypePointer,
                    value: crate::ffi::va::_VAGenericValue__bindgen_ty_1 {
                        p: core::ptr::from_mut(&mut descriptor).cast(),
                    },
                },
            },
        ];

        let mut surface: VASurfaceID = 0;
        // SAFETY: the attribute list and the descriptor it points at both
        // outlive the call, the descriptor names one live descriptor, and the
        // surface is one writable identifier.
        let status = unsafe {
            (self.va.create_surfaces)(
                self.raw,
                VA_RT_FORMAT_YUV420,
                frame.width,
                frame.height,
                core::ptr::from_mut(&mut surface),
                1,
                attributes.as_mut_ptr().cast(),
                c_uint::try_from(attributes.len()).unwrap_or(0),
            )
        };
        self.va.check(status)?;
        Ok(surface)
    }

    /// Release a surface taken by [`Display::import`].
    pub fn release(&self, mut surface: VASurfaceID) {
        // SAFETY: the identifier came from a successful import on this display.
        unsafe {
            (self.va.destroy_surfaces)(self.raw, core::ptr::from_mut(&mut surface), 1);
        }
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

/// Where a picture to encode comes from.
enum Source<'a> {
    /// Bytes in system memory, which have to be uploaded first.
    Bytes(&'a lowlat_capture::Frame<'a>),
    /// A surface already on this device, imported from the allocation the
    /// conversion wrote into.
    Surface(VASurfaceID),
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

/// One picture in flight.
#[derive(Debug)]
struct InFlight {
    surface: VASurfaceID,
    coded: VABufferID,
    /// Answered from what was submitted rather than sniffed back out of the
    /// bitstream, which is the only place it is known without parsing.
    keyframe: bool,
}

/// An encoder over a context.
#[derive(Debug)]
pub struct Encoder<'a> {
    context: &'a Context<'a>,
    params: Params,
    bitrate_bps: u32,
    /// One coded buffer per surface, allocated once.
    coded: Vec<VABufferID>,
    next: usize,
    pending: std::collections::VecDeque<InFlight>,
    idr_pic_id: u16,
    collected: Vec<u8>,
    /// Counts reference pictures since the last refresh, wrapping at what the
    /// sequence parameters declared.
    frame_num: u32,
    /// The one picture a predicted picture is allowed to reference.
    ///
    /// `None` before anything has been encoded, which is what makes the first
    /// picture a refresh whether or not one was asked for: there is nothing to
    /// predict from.
    reference: Option<Reference>,
}

/// A picture an encode may reference, as the driver needs to see it.
#[derive(Debug, Clone, Copy)]
struct Reference {
    surface: VASurfaceID,
    frame_num: u32,
    poc: u32,
}

/// What one submission needs to know about the picture it is coding.
///
/// Passed by value so the two codec halves take the same shape and neither
/// reaches back for counters the other one keeps.
#[derive(Debug, Clone, Copy)]
struct Plan {
    refresh: bool,
    recon: VASurfaceID,
    coded: VABufferID,
    frame_num: u32,
    poc: u32,
}

impl Plan {
    /// The picture as the second codec's writer names it.
    fn hevc_picture(self) -> crate::h265::Picture {
        if self.refresh {
            crate::h265::Picture::Refresh
        } else {
            crate::h265::Picture::Predicted { poc_lsb: self.poc }
        }
    }
}

/// The quantiser a picture opens at, before any per-block delta.
///
/// **Both codecs' picture sets declare no offset from it**, so this value and
/// the zero written there are one setting expressed twice.
const INITIAL_QP: u8 = 26;

/// The interface's way of saying a picture has no collocated reference, which
/// is the case whenever temporal motion vector prediction is off. Zero would
/// name the first reference instead.
const NO_COLLOCATED_PICTURE: u8 = 0xFF;

impl<'a> Context<'a> {
    /// Build an encoder over this context.
    pub fn encoder(&'a self, params: Params, bitrate_bps: u32) -> Result<Encoder<'a>> {
        let va = self.display.va;
        let display = self.display.raw;
        // Generous: a keyframe of a hard scene is far larger than the average,
        // and a coded buffer too small fails the picture rather than truncating
        // it, which is the right failure but an avoidable one.
        let size = params.width() * params.height() * 3 / 2;

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
            frame_num: 0,
            reference: None,
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

    /// The two buffers that carry the rate and the rate it is spent at.
    ///
    /// **The rate the encoder actually runs at is carried here, not in the
    /// sequence parameters.** The sequence buffer has a field for it and one
    /// driver never reads that field, so a rate set only there leaves the
    /// encoder on its own default with nothing reporting a problem. The
    /// sequence field is set as well because a different driver does read it.
    fn rate_buffers(&self, fps: u32) -> Result<(VABufferID, VABufferID)> {
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

        // **A rate is meaningless without the rate it is spent at**, and the
        // driver does not learn the frame rate from anywhere else. Left unset
        // it uses its own default, which is half what this pipeline runs at,
        // so it budgets bits for thirty frames and receives sixty: the stream
        // comes out at exactly twice its target, tracking the setting
        // faithfully and overshooting it by a factor of two at every value.
        // A congestion controller actuating through that is wrong by the same
        // factor, and pushes a path into loss while believing it is well
        // inside the budget.
        #[repr(C)]
        struct FrameRate {
            header: crate::ffi::va::VAEncMiscParameterBuffer,
            rate: crate::ffi::va::VAEncMiscParameterFrameRate,
        }
        // SAFETY: a plain-data header followed by its payload, which is the
        // layout the interface specifies. All-zero is a valid value of every
        // field and the ones that matter are set below.
        let mut frame_rate = unsafe { core::mem::zeroed::<FrameRate>() };
        frame_rate.header.type_ = crate::ffi::va::VAEncMiscParameterTypeFrameRate;
        // Numerator in the low half, denominator in the high half. A zero
        // denominator is read as one, but it is written out because a field
        // that happens to work when left zero is one nobody checks.
        frame_rate.rate.framerate = fps | (1 << 16);
        let fps_buffer = self.buffer(crate::ffi::va::VAEncMiscParameterBufferType, &frame_rate)?;

        Ok((rate_buffer, fps_buffer))
    }

    /// Build one picture's buffers for the first codec.
    ///
    /// **The order buffers are pushed in is load bearing twice over.** The
    /// parameter sets declare the field widths the slice header is then read
    /// with, and the picture buffer decides which fields the slice header is
    /// expected to carry at all, so both precede it.
    fn picture_h264(
        &self,
        params: &crate::h264::Params,
        plan: Plan,
        push: &mut impl FnMut(VABufferID),
    ) -> Result<()> {
        let mut seq =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncSequenceParameterBufferH264>() };
        seq.level_idc = u8::try_from(params.level_idc).unwrap_or(42);
        // **Refreshes happen on request, not on a schedule.** Zero is the
        // interface's way of saying no period applies; a period of one would
        // declare every picture a refresh, which is what this backend used to
        // do and what the delivery gate exists to decide instead. One picture
        // between predicted pictures, because there are no bidirectional ones.
        seq.intra_period = 0;
        seq.intra_idr_period = 0;
        seq.ip_period = 1;
        seq.bits_per_second = self.bitrate_bps;
        seq.max_num_ref_frames = params.max_num_ref_frames;
        seq.picture_width_in_mbs = u16::try_from(params.width.div_ceil(16)).unwrap_or(0);
        seq.picture_height_in_mbs = u16::try_from(params.height.div_ceil(16)).unwrap_or(0);
        // SAFETY: the flags are a union of a bitfield view and a plain word.
        // The structure was zeroed, so either view is a valid value of it, and
        // only the bitfield view is used from here on.
        unsafe {
            let bits = &mut seq.seq_fields.bits;
            bits.set_chroma_format_idc(1);
            bits.set_frame_mbs_only_flag(1);
            bits.set_direct_8x8_inference_flag(1);
            bits.set_log2_max_frame_num_minus4(params.log2_max_frame_num_minus4);
            bits.set_pic_order_cnt_type(0);
            bits.set_log2_max_pic_order_cnt_lsb_minus4(params.log2_max_poc_lsb_minus4);
        }
        let bottom = (params.height.div_ceil(16) * 16 - params.height) / 2;
        if bottom > 0 {
            seq.frame_cropping_flag = 1;
            seq.frame_crop_bottom_offset = bottom;
        }
        push(self.buffer(crate::ffi::va::VAEncSequenceParameterBufferType, &seq)?);

        let (rate_buffer, fps_buffer) = self.rate_buffers(params.fps)?;
        push(rate_buffer);
        push(fps_buffer);

        // **Both parameter sets go into one header of sequence type, and the
        // picture type is never used at all.** The interface names a type per
        // set, which invites one header each; a working encoder on this driver
        // sends them together and never sends the picture type, and sending it
        // is what the driver faults on while assembling the stream.
        // **Only with a refresh.** A parameter set is what makes a refresh
        // decodable on its own, so it travels with one and a guest that joins
        // mid-stream needs no separate out-of-band step. Repeating it on a
        // predicted picture is bytes on the wire that change nothing.
        if plan.refresh {
            let mut sets = [0u8; 384];
            let sps_len =
                crate::h264::sequence_parameter_set(params, &mut sets).ok_or(Error::NoEncoder)?;
            let pps_len = crate::h264::picture_parameter_set(
                sets.get_mut(sps_len..).ok_or(Error::NoEncoder)?,
            )
            .ok_or(Error::NoEncoder)?;
            let sets_len = sps_len + pps_len;
            let (head, data) = self.packed(
                crate::ffi::va::VAEncPackedHeaderSequence,
                &sets[..sets_len],
                sets_len * 8,
            )?;
            push(head);
            push(data);
        }

        let mut pic =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncPictureParameterBufferH264>() };
        pic.CurrPic.picture_id = plan.recon;
        pic.CurrPic.frame_idx = plan.frame_num;
        pic.CurrPic.TopFieldOrderCnt = i32::try_from(plan.poc).unwrap_or(0);
        pic.CurrPic.BottomFieldOrderCnt = pic.CurrPic.TopFieldOrderCnt;
        // Unused entries must be marked invalid, not left zero: zero is a
        // valid surface identifier and the driver would follow it.
        for entry in &mut pic.ReferenceFrames {
            entry.picture_id = crate::ffi::va::VA_INVALID_SURFACE;
            entry.flags = crate::ffi::va::VA_PICTURE_H264_INVALID;
        }
        // **The reference has to be named here even though the slice names it
        // too.** This list is what the driver keeps its reference store from;
        // a picture missing from it is one the driver is entitled to release,
        // and it will, while the slice below still points at it.
        if let (false, Some(reference)) = (plan.refresh, self.reference) {
            if let Some(entry) = pic.ReferenceFrames.first_mut() {
                entry.picture_id = reference.surface;
                entry.frame_idx = reference.frame_num;
                entry.TopFieldOrderCnt = i32::try_from(reference.poc).unwrap_or(0);
                entry.BottomFieldOrderCnt = entry.TopFieldOrderCnt;
                entry.flags = crate::ffi::va::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
            }
        }
        pic.coded_buf = plan.coded;
        pic.frame_num = u16::try_from(plan.frame_num).unwrap_or(0);
        pic.pic_init_qp = INITIAL_QP;
        // SAFETY: as above, a zeroed union accessed only through one view.
        unsafe {
            let bits = &mut pic.pic_fields.bits;
            bits.set_idr_pic_flag(u32::from(plan.refresh));
            bits.set_reference_pic_flag(1);
            bits.set_entropy_coding_mode_flag(1);
            bits.set_deblocking_filter_control_present_flag(1);
            bits.set_transform_8x8_mode_flag(1);
        }
        push(self.buffer(crate::ffi::va::VAEncPictureParameterBufferType, &pic)?);

        let mut slice =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncSliceParameterBufferH264>() };
        slice.num_macroblocks = params.width.div_ceil(16) * params.height.div_ceil(16);
        // Two is intra, zero is predicted. Both are the plain forms; the
        // values above four assert the whole picture has this type, and the
        // packed header carries those.
        slice.slice_type = if plan.refresh { 2 } else { 0 };
        // Alternates so two consecutive refreshes are distinguishable, which
        // is what the field is for.
        slice.idr_pic_id = self.idr_pic_id;
        slice.pic_order_cnt_lsb = u16::try_from(plan.poc).unwrap_or(0);
        for entry in slice
            .RefPicList0
            .iter_mut()
            .chain(slice.RefPicList1.iter_mut())
        {
            entry.picture_id = crate::ffi::va::VA_INVALID_SURFACE;
            entry.flags = crate::ffi::va::VA_PICTURE_H264_INVALID;
        }
        if let (false, Some(reference)) = (plan.refresh, self.reference) {
            if let Some(entry) = slice.RefPicList0.first_mut() {
                entry.picture_id = reference.surface;
                entry.frame_idx = reference.frame_num;
                entry.TopFieldOrderCnt = i32::try_from(reference.poc).unwrap_or(0);
                entry.BottomFieldOrderCnt = entry.TopFieldOrderCnt;
                entry.flags = crate::ffi::va::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
            }
        }
        push(self.buffer(crate::ffi::va::VAEncSliceParameterBufferType, &slice)?);

        // **The driver expects one of these per picture once packed headers
        // are in use.** Without it there is no slice header to emit and the
        // stream cannot be assembled, which is a fault inside the driver
        // rather than a status.
        let mut header = [0u8; 64];
        let kind = if plan.refresh {
            crate::h264::Picture::Refresh {
                idr_pic_id: u32::from(self.idr_pic_id),
            }
        } else {
            crate::h264::Picture::Predicted {
                frame_num: plan.frame_num,
                poc_lsb: plan.poc,
            }
        };
        let written =
            crate::h264::slice_header(params, kind, &mut header).ok_or(Error::NoEncoder)?;
        let (head, data) = self.packed(
            crate::ffi::va::VAEncPackedHeaderSlice,
            &header[..written.bytes_written],
            written.bit_length,
        )?;
        push(head);
        push(data);
        Ok(())
    }

    /// Build one picture's buffers for the second codec.
    ///
    /// **Every tool flag set here has a counterpart in the parameter sets**
    /// ([`crate::h265`]), because this side codes the slice data and those
    /// bytes describe it. The two are written from the same values on purpose;
    /// a disagreement is not caught before the decoder.
    fn picture_h265(
        &self,
        params: &crate::h265::Params,
        plan: Plan,
        push: &mut impl FnMut(VABufferID),
    ) -> Result<()> {
        let (coded_width, coded_height) = params.coded();

        let mut seq =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncSequenceParameterBufferHEVC>() };
        seq.general_profile_idc = u8::try_from(crate::h265::PROFILE_MAIN).unwrap_or(1);
        seq.general_level_idc = u8::try_from(params.level_idc).unwrap_or(123);
        seq.general_tier_flag = 0;
        // As for the other codec: no period, and one picture between predicted
        // ones because there are no bidirectional ones.
        seq.intra_period = 0;
        seq.intra_idr_period = 0;
        seq.ip_period = 1;
        seq.bits_per_second = self.bitrate_bps;
        // **The coded size, not the visible one.** The sets crop the
        // difference with a conformance window and the device is told what it
        // actually codes.
        seq.pic_width_in_luma_samples = u16::try_from(coded_width).unwrap_or(0);
        seq.pic_height_in_luma_samples = u16::try_from(coded_height).unwrap_or(0);
        let byte = |value: u32| u8::try_from(value).unwrap_or(0);
        seq.log2_min_luma_coding_block_size_minus3 = byte(crate::h265::LOG2_MIN_CB - 3);
        seq.log2_diff_max_min_luma_coding_block_size =
            byte(crate::h265::LOG2_CTB - crate::h265::LOG2_MIN_CB);
        seq.log2_min_transform_block_size_minus2 = byte(crate::h265::LOG2_MIN_TB - 2);
        seq.log2_diff_max_min_transform_block_size =
            byte(crate::h265::LOG2_MAX_TB - crate::h265::LOG2_MIN_TB);
        seq.max_transform_hierarchy_depth_inter = byte(crate::h265::TRANSFORM_HIERARCHY_DEPTH);
        seq.max_transform_hierarchy_depth_intra = byte(crate::h265::TRANSFORM_HIERARCHY_DEPTH);
        // SAFETY: a union of a bitfield view and a plain word over a zeroed
        // structure, so either view is a valid value of it, and only the
        // bitfield view is used.
        unsafe {
            let bits = &mut seq.seq_fields.bits;
            bits.set_chroma_format_idc(1);
            bits.set_amp_enabled_flag(1);
            bits.set_sample_adaptive_offset_enabled_flag(1);
            bits.set_strong_intra_smoothing_enabled_flag(0);
            // No temporal motion vector prediction, which is what lets the
            // slice header omit the collocated picture entirely.
            bits.set_sps_temporal_mvp_enabled_flag(0);
        }
        push(self.buffer(crate::ffi::va::VAEncSequenceParameterBufferType, &seq)?);

        let (rate_buffer, fps_buffer) = self.rate_buffers(params.fps)?;
        push(rate_buffer);
        push(fps_buffer);

        // **Three sets on this codec, and all three in one sequence header.**
        // The interface names a type for the picture set too, and the other
        // codec's path found that sending it is what the driver faults on. A
        // decoder that never receives the video set has nothing to attach the
        // sequence set to, so it leads.
        if plan.refresh {
            let mut sets = [0u8; 384];
            let vps_len =
                crate::h265::video_parameter_set(params, &mut sets).ok_or(Error::NoEncoder)?;
            let sps_len = crate::h265::sequence_parameter_set(
                params,
                sets.get_mut(vps_len..).ok_or(Error::NoEncoder)?,
            )
            .ok_or(Error::NoEncoder)?;
            let pps_len = crate::h265::picture_parameter_set(
                sets.get_mut(vps_len + sps_len..).ok_or(Error::NoEncoder)?,
            )
            .ok_or(Error::NoEncoder)?;
            let sets_len = vps_len + sps_len + pps_len;
            let (head, data) = self.packed(
                crate::ffi::va::VAEncPackedHeaderSequence,
                &sets[..sets_len],
                sets_len * 8,
            )?;
            push(head);
            push(data);
        }

        let mut pic =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncPictureParameterBufferHEVC>() };
        pic.decoded_curr_pic.picture_id = plan.recon;
        pic.decoded_curr_pic.pic_order_cnt = i32::try_from(plan.poc).unwrap_or(0);
        // Unused entries must be marked invalid, not left zero: zero is a
        // valid surface identifier and the driver would follow it.
        for entry in &mut pic.reference_frames {
            entry.picture_id = crate::ffi::va::VA_INVALID_SURFACE;
            entry.flags = crate::ffi::va::VA_PICTURE_HEVC_INVALID;
        }
        // The one picture a predicted picture points at, and it is always the
        // one immediately before, which is what makes the reference set in the
        // slice header a single entry one picture back.
        if let (false, Some(reference)) = (plan.refresh, self.reference) {
            if let Some(entry) = pic.reference_frames.first_mut() {
                entry.picture_id = reference.surface;
                entry.pic_order_cnt = i32::try_from(reference.poc).unwrap_or(0);
                entry.flags = crate::ffi::va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
            }
        }
        pic.coded_buf = plan.coded;
        // No collocated picture, which this value is the interface's way of
        // saying. Zero would name the first reference instead.
        pic.collocated_ref_pic_index = NO_COLLOCATED_PICTURE;
        pic.pic_init_qp = INITIAL_QP;
        pic.nal_unit_type = plan.hevc_picture().unit_type();
        // SAFETY: as above, a zeroed union accessed only through one view.
        unsafe {
            let bits = &mut pic.pic_fields.bits;
            bits.set_idr_pic_flag(u32::from(plan.refresh));
            bits.set_coding_type(if plan.refresh { 1 } else { 2 });
            bits.set_reference_pic_flag(1);
            bits.set_transform_skip_enabled_flag(1);
            // **Rate control has no other handle on this codec**: without a
            // per-block quantiser delta the whole picture is stuck at the
            // slice quantiser and the configured bitrate does nothing.
            bits.set_cu_qp_delta_enabled_flag(1);
            bits.set_pps_loop_filter_across_slices_enabled_flag(1);
        }
        push(self.buffer(crate::ffi::va::VAEncPictureParameterBufferType, &pic)?);

        let mut slice =
            unsafe { core::mem::zeroed::<crate::ffi::va::VAEncSliceParameterBufferHEVC>() };
        slice.num_ctu_in_slice = params.ctus();
        slice.slice_type = plan.hevc_picture().slice_type();
        slice.max_num_merge_cand = u8::try_from(crate::h265::MAX_NUM_MERGE_CAND).unwrap_or(5);
        for entry in slice
            .ref_pic_list0
            .iter_mut()
            .chain(slice.ref_pic_list1.iter_mut())
        {
            entry.picture_id = crate::ffi::va::VA_INVALID_SURFACE;
            entry.flags = crate::ffi::va::VA_PICTURE_HEVC_INVALID;
        }
        if let (false, Some(reference)) = (plan.refresh, self.reference) {
            if let Some(entry) = slice.ref_pic_list0.first_mut() {
                entry.picture_id = reference.surface;
                entry.pic_order_cnt = i32::try_from(reference.poc).unwrap_or(0);
                entry.flags = crate::ffi::va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
            }
        }
        // SAFETY: as above, a zeroed union accessed only through one view.
        unsafe {
            let bits = &mut slice.slice_fields.bits;
            bits.set_last_slice_of_pic_flag(1);
            bits.set_slice_sao_luma_flag(1);
            bits.set_slice_sao_chroma_flag(1);
            bits.set_slice_loop_filter_across_slices_enabled_flag(1);
        }
        push(self.buffer(crate::ffi::va::VAEncSliceParameterBufferType, &slice)?);

        let mut header = [0u8; 64];
        let written = crate::h265::slice_header(params, plan.hevc_picture(), &mut header)
            .ok_or(Error::NoEncoder)?;
        let (head, data) = self.packed(
            crate::ffi::va::VAEncPackedHeaderSlice,
            &header[..written.bytes_written],
            written.bit_length,
        )?;
        push(head);
        push(data);
        Ok(())
    }

    /// Encode one picture. Returns as soon as it is queued.
    ///
    /// Every picture here is a keyframe for now: the reference-picture
    /// bookkeeping a predicted picture needs is the next piece, and shipping a
    /// half-correct reference list would produce a stream that decodes into
    /// progressively wrong output rather than failing.
    pub fn submit(
        &mut self,
        frame: &lowlat_capture::Frame<'_>,
        force_keyframe: bool,
    ) -> Result<()> {
        self.submit_from(Source::Bytes(frame), force_keyframe)
    }

    /// Encode a picture that is already on this device.
    ///
    /// **No upload, and that is the whole difference.** The surface was
    /// imported from the allocation the conversion wrote into, so the encoder
    /// reads those very bytes.
    pub fn submit_registered(&mut self, input: VASurfaceID, force_keyframe: bool) -> Result<()> {
        self.submit_from(Source::Surface(input), force_keyframe)
    }

    fn submit_from(&mut self, source: Source<'_>, force_keyframe: bool) -> Result<()> {
        if self.pending.len() >= self.coded.len() {
            return Err(Error::QueueFull);
        }
        // **A refresh when asked for, and whenever there is nothing to predict
        // from.** The second half is not a special case for the first picture:
        // it is the same rule, because a reference we do not hold is one we
        // cannot point at.
        let refresh = force_keyframe || self.reference.is_none();
        let slot = self.next;
        self.next = (self.next + 1) % self.coded.len();
        let recon = *self.context.recon.get(slot).ok_or(Error::NoEncoder)?;
        let coded = self.coded[slot];

        // **One input surface per slot, so this cannot overwrite a picture
        // that is still being encoded.** The slot is only reused once its
        // picture has been collected, which is what the queue-full check
        // above enforces. A picture already on the device brings its own
        // surface and the pool is not touched at all.
        let input = match source {
            Source::Bytes(frame) => {
                let input = *self.context.surfaces.get(slot).ok_or(Error::NoEncoder)?;
                self.upload(input, frame)?;
                input
            }
            Source::Surface(input) => input,
        };

        // The picture is read from the input surface and reconstructed into
        // its counterpart. The two are never the same surface.
        // SAFETY: the display and context are live for the call.
        let status =
            unsafe { (self.va().begin_picture)(self.display(), self.context.context, input) };
        self.va().check(status)?;

        // **A refresh restarts the counters by definition**, and what advances
        // for a predicted picture depends on the codec: one carries a frame
        // number the order count is derived from, the other carries only the
        // order count.
        let (frame_num, poc) = if refresh {
            (0, 0)
        } else {
            match self.params {
                Params::H264(params) => {
                    let next = (self.frame_num + 1) % (1 << (params.log2_max_frame_num_minus4 + 4));
                    (next, (next * 2) % self.params.poc_period())
                }
                Params::H265(_) => (
                    0,
                    (self.reference.map_or(0, |reference| reference.poc) + 1)
                        % self.params.poc_period(),
                ),
            }
        };
        let plan = Plan {
            refresh,
            recon,
            coded,
            frame_num,
            poc,
        };

        let mut buffers = [0 as VABufferID; 9];
        let mut used = 0usize;
        {
            let mut push = |id: VABufferID| {
                if let Some(slot) = buffers.get_mut(used) {
                    *slot = id;
                    used += 1;
                }
            };
            match self.params {
                Params::H264(params) => self.picture_h264(&params, plan, &mut push)?,
                Params::H265(params) => self.picture_h265(&params, plan, &mut push)?,
            }
        }
        let buffers = buffers.get_mut(..used).ok_or(Error::NoEncoder)?;

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
        for id in buffers.iter() {
            // SAFETY: each came from a successful create above and is
            // destroyed once.
            unsafe { (self.va().destroy_buffer)(self.display(), *id) };
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
        if refresh {
            self.idr_pic_id ^= 1;
        }
        self.frame_num = frame_num;
        // The picture just submitted becomes what the next one predicts from.
        // Only one is kept, which is what the sequence parameters declared and
        // what the sliding-window marking in the slice header implements.
        self.reference = Some(Reference {
            surface: recon,
            frame_num,
            poc,
        });
        self.pending.push_back(InFlight {
            surface: input,
            coded,
            keyframe: refresh,
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
        let (surface, coded, keyframe) = (head.surface, head.coded, head.keyframe);

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
            keyframe,
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

impl EncoderTrait for Encoder<'_> {
    type Error = Error;

    fn submit(&mut self, frame: &lowlat_capture::Frame<'_>, force_keyframe: bool) -> Result<()> {
        Encoder::submit(self, frame, force_keyframe)
    }

    fn poll(&mut self) -> Result<Poll<'_>> {
        Encoder::poll(self)
    }

    /// Cannot fail here: the rate travels with each picture rather than being
    /// encoder state, so there is no call to make and nothing to refuse.
    fn reconfigure(&mut self, bitrate_bps: u32) -> Result<()> {
        Encoder::reconfigure(self, bitrate_bps);
        Ok(())
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
