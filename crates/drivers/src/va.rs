//! The open-stack video interface: its runtime, resolved by name, and a
//! display bound to one render node.
//!
//! **One table for both halves.** The encoders and the decoders reach the
//! same library, so the entry points either needs are resolved here once; a
//! symbol the encoder never calls costs a lookup at load and nothing after.
//! What a backend does with the table is the backend's; nothing here creates
//! a configuration, a context or a buffer.

use core::ffi::{CStr, c_char, c_int, c_uint};

use lowlat_common::dynlib::Library;

use crate::ffi::va::{
    VA_STATUS_SUCCESS, VABufferID, VABufferType, VAConfigAttrib, VAConfigID, VAContextID,
    VADisplay, VAEntrypoint, VAImage, VAImageFormat, VAImageID, VAProfile, VAStatus,
    VASurfaceAttrib, VASurfaceID,
};

/// The core interface and its display binding, versioned.
const LIBVA: [&CStr; 2] = [c"libva.so.2", c"libva.so"];
const LIBVA_DRM: [&CStr; 2] = [c"libva-drm.so.2", c"libva-drm.so"];

pub type GetDisplayDrm = unsafe extern "C" fn(c_int) -> VADisplay;
pub type Initialize = unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
pub type Terminate = unsafe extern "C" fn(VADisplay) -> VAStatus;
pub type MaxNumProfiles = unsafe extern "C" fn(VADisplay) -> c_int;
pub type QueryConfigProfiles =
    unsafe extern "C" fn(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus;
pub type MaxNumEntrypoints = unsafe extern "C" fn(VADisplay) -> c_int;
pub type QueryConfigEntrypoints =
    unsafe extern "C" fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus;
pub type ErrorStr = unsafe extern "C" fn(VAStatus) -> *const c_char;
pub type QueryVendorString = unsafe extern "C" fn(VADisplay) -> *const c_char;
pub type GetConfigAttributes = unsafe extern "C" fn(
    VADisplay,
    VAProfile,
    VAEntrypoint,
    *mut VAConfigAttrib,
    c_int,
) -> VAStatus;
/// What layouts a configuration will bind as a surface, asked of the driver
/// rather than read off a support matrix. The count is in/out: a null list
/// makes the call report the number of entries, and the second call fills
/// them.
pub type QuerySurfaceAttributes =
    unsafe extern "C" fn(VADisplay, VAConfigID, *mut VASurfaceAttrib, *mut c_uint) -> VAStatus;
pub type CreateConfig = unsafe extern "C" fn(
    VADisplay,
    VAProfile,
    VAEntrypoint,
    *mut VAConfigAttrib,
    c_int,
    *mut VAConfigID,
) -> VAStatus;
pub type DestroyConfig = unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus;
pub type CreateSurfaces = unsafe extern "C" fn(
    VADisplay,
    c_uint,
    c_uint,
    c_uint,
    *mut VASurfaceID,
    c_uint,
    *mut core::ffi::c_void,
    c_uint,
) -> VAStatus;
pub type DestroySurfaces = unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus;
pub type CreateContext = unsafe extern "C" fn(
    VADisplay,
    VAConfigID,
    c_int,
    c_int,
    c_int,
    *mut VASurfaceID,
    c_int,
    *mut VAContextID,
) -> VAStatus;
pub type DestroyContext = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
pub type CreateBuffer = unsafe extern "C" fn(
    VADisplay,
    VAContextID,
    VABufferType,
    c_uint,
    c_uint,
    *mut core::ffi::c_void,
    *mut VABufferID,
) -> VAStatus;
pub type DestroyBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
pub type MapBuffer =
    unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut core::ffi::c_void) -> VAStatus;
pub type UnmapBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
pub type BeginPicture = unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
pub type RenderPicture =
    unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus;
pub type EndPicture = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
pub type SyncSurface = unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus;
/// Address a surface's own storage rather than allocating a second copy of
/// it. The alternative pair creates an image and copies into the surface,
/// which is a whole frame of memory traffic per picture to avoid asking.
pub type DeriveImage = unsafe extern "C" fn(VADisplay, VASurfaceID, *mut VAImage) -> VAStatus;
pub type DestroyImage = unsafe extern "C" fn(VADisplay, VAImageID) -> VAStatus;
/// The copying pair: an image in a layout of the caller's choosing, and a
/// read of a surface into it done by the driver.
pub type CreateImage =
    unsafe extern "C" fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus;
pub type GetImage = unsafe extern "C" fn(
    VADisplay,
    VASurfaceID,
    c_int,
    c_int,
    c_uint,
    c_uint,
    VAImageID,
) -> VAStatus;
pub type MaxNumImageFormats = unsafe extern "C" fn(VADisplay) -> c_int;
pub type QueryImageFormats =
    unsafe extern "C" fn(VADisplay, *mut VAImageFormat, *mut c_int) -> VAStatus;

/// Why the runtime could not be used.
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
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("display runtime not present"),
            Self::MissingSymbol => f.write_str("display runtime is missing an entry point"),
            Self::NoDevice => f.write_str("render node could not be opened"),
            Self::Status(status) => write!(f, "display runtime returned status {status}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

/// A count the interface reported, as a length.
///
/// These are signed and the interface is entitled to report a negative on
/// failure, so a cast would turn one into an enormous length. Converted rather
/// than cast, with the failure folded into zero.
pub fn count(reported: c_int) -> usize {
    usize::try_from(reported).unwrap_or(0)
}

/// The loaded runtime.
///
/// The entry points are public because a backend calls them directly; every
/// call is `unsafe` at the site, with the argument contract stated there.
#[derive(Debug)]
pub struct Vaapi {
    pub initialize: Initialize,
    pub terminate: Terminate,
    pub max_num_profiles: MaxNumProfiles,
    pub query_config_profiles: QueryConfigProfiles,
    pub max_num_entrypoints: MaxNumEntrypoints,
    pub query_config_entrypoints: QueryConfigEntrypoints,
    pub error_str: ErrorStr,
    pub query_vendor_string: QueryVendorString,
    pub get_config_attributes: GetConfigAttributes,
    pub query_surface_attributes: QuerySurfaceAttributes,
    pub create_config: CreateConfig,
    pub destroy_config: DestroyConfig,
    pub create_surfaces: CreateSurfaces,
    pub destroy_surfaces: DestroySurfaces,
    pub create_context: CreateContext,
    pub destroy_context: DestroyContext,
    pub create_buffer: CreateBuffer,
    pub destroy_buffer: DestroyBuffer,
    pub map_buffer: MapBuffer,
    pub unmap_buffer: UnmapBuffer,
    pub begin_picture: BeginPicture,
    pub render_picture: RenderPicture,
    pub end_picture: EndPicture,
    pub sync_surface: SyncSurface,
    pub derive_image: DeriveImage,
    pub destroy_image: DestroyImage,
    pub create_image: CreateImage,
    pub get_image: GetImage,
    pub max_num_image_formats: MaxNumImageFormats,
    pub query_image_formats: QueryImageFormats,
    pub get_display_drm: GetDisplayDrm,
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

        macro_rules! symbol {
            ($name:literal) => {
                libva.symbol($name).ok_or(Error::MissingSymbol)?
            };
        }

        // SAFETY: every signature is transcribed from the vendored headers.
        // These names carry no version suffix, unlike the compute runtime's.
        unsafe {
            Ok(Self {
                initialize: symbol!(c"vaInitialize"),
                terminate: symbol!(c"vaTerminate"),
                max_num_profiles: symbol!(c"vaMaxNumProfiles"),
                query_config_profiles: symbol!(c"vaQueryConfigProfiles"),
                max_num_entrypoints: symbol!(c"vaMaxNumEntrypoints"),
                query_config_entrypoints: symbol!(c"vaQueryConfigEntrypoints"),
                error_str: symbol!(c"vaErrorStr"),
                query_vendor_string: symbol!(c"vaQueryVendorString"),
                get_config_attributes: symbol!(c"vaGetConfigAttributes"),
                query_surface_attributes: symbol!(c"vaQuerySurfaceAttributes"),
                create_config: symbol!(c"vaCreateConfig"),
                destroy_config: symbol!(c"vaDestroyConfig"),
                create_surfaces: symbol!(c"vaCreateSurfaces"),
                destroy_surfaces: symbol!(c"vaDestroySurfaces"),
                create_context: symbol!(c"vaCreateContext"),
                destroy_context: symbol!(c"vaDestroyContext"),
                create_buffer: symbol!(c"vaCreateBuffer"),
                destroy_buffer: symbol!(c"vaDestroyBuffer"),
                map_buffer: symbol!(c"vaMapBuffer"),
                unmap_buffer: symbol!(c"vaUnmapBuffer"),
                begin_picture: symbol!(c"vaBeginPicture"),
                render_picture: symbol!(c"vaRenderPicture"),
                end_picture: symbol!(c"vaEndPicture"),
                sync_surface: symbol!(c"vaSyncSurface"),
                derive_image: symbol!(c"vaDeriveImage"),
                destroy_image: symbol!(c"vaDestroyImage"),
                create_image: symbol!(c"vaCreateImage"),
                get_image: symbol!(c"vaGetImage"),
                max_num_image_formats: symbol!(c"vaMaxNumImageFormats"),
                query_image_formats: symbol!(c"vaQueryImageFormats"),
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

    /// A status as a result.
    pub fn check(&self, status: VAStatus) -> Result<()> {
        if status == VA_STATUS_SUCCESS as VAStatus {
            Ok(())
        } else {
            Err(Error::Status(status))
        }
    }

    /// Bind a display to a render node, by path.
    ///
    /// The **render** node rather than the card node: coding needs no display
    /// control, and the card node additionally needs a privilege this process
    /// should not hold for a job that does not require it.
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

impl<'a> Display<'a> {
    /// The runtime this display was opened through.
    pub fn va(&self) -> &'a Vaapi {
        self.va
    }

    /// The interface version the driver implements.
    pub fn version(&self) -> (i32, i32) {
        self.version
    }

    /// The raw handle, for the calls that take one.
    pub fn raw(&self) -> VADisplay {
        self.raw
    }

    /// Every profile the driver offers, for any entry point.
    pub fn profiles(&self) -> Result<Vec<VAProfile>> {
        // SAFETY: the display is live.
        let capacity = count(unsafe { (self.va.max_num_profiles)(self.raw) });
        let mut profiles = vec![0 as VAProfile; capacity];
        let mut found: c_int = 0;
        // SAFETY: the buffer is writable for the length the interface was told
        // about, and the count is written back.
        let status = unsafe {
            (self.va.query_config_profiles)(self.raw, profiles.as_mut_ptr(), &raw mut found)
        };
        self.va.check(status)?;
        profiles.truncate(count(found));
        Ok(profiles)
    }

    /// The driver's own description of itself, for a label. Empty when the
    /// driver gives none.
    pub fn vendor(&self) -> String {
        // SAFETY: the display is live; the string is the driver's, valid
        // for the display's life, and copied out at once.
        let text = unsafe { (self.va.query_vendor_string)(self.raw) };
        if text.is_null() {
            return String::new();
        }
        // SAFETY: a NUL-terminated string the driver owns.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }

    /// The largest picture one profile decodes, as the driver reports it;
    /// zero where the driver does not say.
    pub fn max_picture(&self, profile: VAProfile, entrypoint: VAEntrypoint) -> (u32, u32) {
        let mut attribs = [
            VAConfigAttrib {
                type_: crate::ffi::va::VAConfigAttribMaxPictureWidth,
                value: 0,
            },
            VAConfigAttrib {
                type_: crate::ffi::va::VAConfigAttribMaxPictureHeight,
                value: 0,
            },
        ];
        // SAFETY: the display is live and the array is writable for the
        // count passed.
        let status = unsafe {
            (self.va.get_config_attributes)(self.raw, profile, entrypoint, attribs.as_mut_ptr(), 2)
        };
        if self.va.check(status).is_err() {
            return (0, 0);
        }
        // An attribute the driver does not support comes back as all ones.
        let read = |value: u32| if value == u32::MAX { 0 } else { value };
        (read(attribs[0].value), read(attribs[1].value))
    }

    /// The entry points the driver offers for one profile.
    pub fn entrypoints(&self, profile: VAProfile) -> Result<Vec<VAEntrypoint>> {
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
        entrypoints.truncate(count(found));
        Ok(entrypoints)
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
