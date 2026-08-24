//! The same import and conversion over the display stack's other interface.
//!
//! **Selected explicitly and never as a rescue.** Nothing here is reached
//! because the primary interface failed; a caller names this one or it is not
//! built. Falling back on its own would hide the reason the first interface
//! refused, which is the diagnostic that says whether a machine can do this at
//! all -- see [`crate::vulkan::Error::Unsupported`].
//!
//! It exists because the requirements the primary interface puts on a driver
//! are newer than the ones here: describing a buffer's tiling, taking it in by
//! descriptor, and writing single-component planes are each a decade old on
//! this interface and comparatively recent on the other. Whether that gap is
//! real on any machine that also carries an encoder is the open question, and
//! this is what lets it be answered by measurement.
//!
//! **The colour rules are not restated and not reimplemented.** The shader is
//! the same file the primary interface compiles, taken as text rather than
//! precompiled; what differs between them is nine lines of declarations behind
//! a conditional and nothing that touches a pixel.
//!
//! **A context belongs to the thread that made it current.** Unlike the
//! primary interface, which hands out a device usable from anywhere, everything
//! here is bound to one thread by the interface itself, so [`Device`] is
//! deliberately neither `Send` nor `Sync`.

use core::ffi::{CStr, c_void};
use std::path::Path;

use khronos_egl as egl;

use crate::vulkan::Imports;

/// What went wrong.
///
/// Shaped like the primary interface's so a caller reads one vocabulary. The
/// two are separate types because the causes are not the same set, and a
/// machine that refuses both should say so twice rather than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The loader is absent or refused to load.
    NoLoader,
    /// A call into the display interface failed.
    Egl(i32),
    /// A call into the drawing interface failed.
    Gl(u32),
    /// No device on this machine drives the display node that was asked for.
    NoDeviceForNode,
    /// The device cannot do part of this. Named individually, because a
    /// missing piece is a deployment fact rather than a bug.
    Unsupported(&'static str),
    /// An entry point the conversion calls is not exported.
    NoEntryPoint(&'static str),
    /// The captured framebuffer carries more planes than this interface names.
    TooManyPlanes,
    /// The shader beside this file did not build. Its log goes to the log.
    BadShader,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoLoader => f.write_str("no usable display interface on this system"),
            Self::Egl(code) => write!(f, "display interface call failed, result {code:#x}"),
            Self::Gl(code) => write!(f, "drawing interface call failed, result {code:#x}"),
            Self::NoDeviceForNode => f.write_str("no device reports driving that display node"),
            Self::Unsupported(what) => write!(f, "the display's device does not support {what}"),
            Self::NoEntryPoint(name) => write!(f, "the driver does not export {name}"),
            Self::TooManyPlanes => f.write_str("the captured buffer has more planes than four"),
            Self::BadShader => f.write_str("the conversion shader did not build"),
        }
    }
}

impl std::error::Error for Error {}

/// The conversion, as text. The other interface compiles the same file.
const CONVERT: &str = include_str!("../shaders/convert.comp");

/// Invocations per workgroup, per axis, matching the shader's own declaration.
const GROUP: u32 = 8;

/// Two thirty-two bit accumulators, which is what the shader writes.
const DIGEST: usize = 8;

// Constants this interface names by number rather than by header. Each is the
// value the specification assigns and none of them can change.
const PLATFORM_DEVICE: egl::Enum = 0x313F;
const DRM_DEVICE_FILE: egl::Int = 0x3233;
const LINUX_DMA_BUF: egl::Enum = 0x3270;
const LINUX_DRM_FOURCC: egl::Attrib = 0x3271;
const WIDTH: egl::Attrib = 0x3057;
const HEIGHT: egl::Attrib = 0x3056;

/// Where one plane's descriptor, offset, pitch and modifier halves sit in the
/// attribute vocabulary.
///
/// **Four entries because the specification names four planes**, and a
/// captured buffer on one driver here already uses three. They are not
/// consecutive: the first three planes were assigned together and the fourth
/// much later, which is why this is a table rather than arithmetic.
const PLANE_ATTRIBUTES: [[egl::Attrib; 5]; 4] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];

/// What the display interface must offer beyond its own core.
const REQUIRED_DISPLAY: [&str; 2] = [
    // Take a captured buffer in by descriptor...
    "EGL_EXT_image_dma_buf_import",
    // ...and describe its tiling rather than assuming it is untiled. This is
    // the one that decides whether a machine can do this at all: a scanout
    // buffer is tiled or compressed on both drivers here and cannot be read
    // without being told how.
    "EGL_EXT_image_dma_buf_import_modifiers",
];

/// What the drawing interface must offer beyond version 4.3.
///
/// **Only one, and it is the load-bearing one.** Taking a captured buffer as a
/// texture has been possible for far longer, but only as a mutable one, which
/// the conversion cannot write through as an image. This is what makes the
/// texture immutable and therefore usable.
const REQUIRED_DRAWING: [&str; 1] = ["GL_EXT_EGL_image_storage"];

/// Numbers the drawing interface names.
mod raw {
    pub(super) const TEXTURE_2D: u32 = 0x0DE1;
    pub(super) const TEXTURE0: u32 = 0x84C0;
    pub(super) const COMPUTE_SHADER: u32 = 0x91B9;
    pub(super) const COMPILE_STATUS: u32 = 0x8B81;
    pub(super) const LINK_STATUS: u32 = 0x8B82;
    pub(super) const INFO_LOG_LENGTH: u32 = 0x8B84;
    pub(super) const SHADER_STORAGE_BUFFER: u32 = 0x90D2;
    pub(super) const DYNAMIC_READ: u32 = 0x88E9;
    pub(super) const WRITE_ONLY: u32 = 0x88B9;
    pub(super) const R8: u32 = 0x8229;
    pub(super) const RG8: u32 = 0x822B;
    pub(super) const RGBA8: u32 = 0x8058;
    pub(super) const RED: u32 = 0x1903;
    pub(super) const RG: u32 = 0x8227;
    pub(super) const RGBA: u32 = 0x1908;
    pub(super) const UNSIGNED_BYTE: u32 = 0x1401;
    pub(super) const TEXTURE_MIN_FILTER: u32 = 0x2801;
    pub(super) const TEXTURE_MAG_FILTER: u32 = 0x2800;
    pub(super) const TEXTURE_WRAP_S: u32 = 0x2802;
    pub(super) const TEXTURE_WRAP_T: u32 = 0x2803;
    pub(super) const NEAREST: i32 = 0x2600;
    pub(super) const CLAMP_TO_EDGE: i32 = 0x812F;
    pub(super) const NUM_EXTENSIONS: u32 = 0x821D;
    pub(super) const EXTENSIONS: u32 = 0x1F03;
    pub(super) const PACK_ALIGNMENT: u32 = 0x0D05;
    pub(super) const NO_ERROR: u32 = 0;
    /// Everything the conversion's writes have to be visible to afterwards.
    /// Asked for as one mask rather than three calls, because the interface
    /// takes a mask and separate calls would each cost a round trip.
    pub(super) const AFTER_DISPATCH: u32 = 0x0000_0020 | 0x0000_2000 | 0x0000_0200;
}

/// The entry points the conversion calls.
///
/// **Resolved once and by name.** The drawing interface exports almost nothing
/// directly; a driver hands out addresses through the display interface, and a
/// missing one is a device that cannot do this rather than a link error.
#[expect(non_snake_case, reason = "the interface's own names, kept verbatim")]
struct Gl {
    GetError: unsafe extern "system" fn() -> u32,
    GetIntegerv: unsafe extern "system" fn(u32, *mut i32),
    GetStringi: unsafe extern "system" fn(u32, u32) -> *const u8,
    CreateShader: unsafe extern "system" fn(u32) -> u32,
    ShaderSource: unsafe extern "system" fn(u32, i32, *const *const u8, *const i32),
    CompileShader: unsafe extern "system" fn(u32),
    GetShaderiv: unsafe extern "system" fn(u32, u32, *mut i32),
    GetShaderInfoLog: unsafe extern "system" fn(u32, i32, *mut i32, *mut u8),
    DeleteShader: unsafe extern "system" fn(u32),
    CreateProgram: unsafe extern "system" fn() -> u32,
    AttachShader: unsafe extern "system" fn(u32, u32),
    LinkProgram: unsafe extern "system" fn(u32),
    GetProgramiv: unsafe extern "system" fn(u32, u32, *mut i32),
    GetProgramInfoLog: unsafe extern "system" fn(u32, i32, *mut i32, *mut u8),
    UseProgram: unsafe extern "system" fn(u32),
    DeleteProgram: unsafe extern "system" fn(u32),
    GenTextures: unsafe extern "system" fn(i32, *mut u32),
    DeleteTextures: unsafe extern "system" fn(i32, *const u32),
    BindTexture: unsafe extern "system" fn(u32, u32),
    ActiveTexture: unsafe extern "system" fn(u32),
    TexStorage2D: unsafe extern "system" fn(u32, i32, u32, i32, i32),
    TexSubImage2D: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    GetTexImage: unsafe extern "system" fn(u32, i32, u32, u32, *mut c_void),
    TexParameteri: unsafe extern "system" fn(u32, u32, i32),
    BindImageTexture: unsafe extern "system" fn(u32, u32, i32, u8, i32, u32, u32),
    GenBuffers: unsafe extern "system" fn(i32, *mut u32),
    DeleteBuffers: unsafe extern "system" fn(i32, *const u32),
    BindBuffer: unsafe extern "system" fn(u32, u32),
    BufferData: unsafe extern "system" fn(u32, isize, *const c_void, u32),
    BufferSubData: unsafe extern "system" fn(u32, isize, isize, *const c_void),
    GetBufferSubData: unsafe extern "system" fn(u32, isize, isize, *mut c_void),
    BindBufferBase: unsafe extern "system" fn(u32, u32, u32),
    Uniform2i: unsafe extern "system" fn(i32, i32, i32),
    Uniform1ui: unsafe extern "system" fn(i32, u32),
    DispatchCompute: unsafe extern "system" fn(u32, u32, u32),
    MemoryBarrier: unsafe extern "system" fn(u32),
    Finish: unsafe extern "system" fn(),
    PixelStorei: unsafe extern "system" fn(u32, i32),
    EGLImageTargetTexStorage: unsafe extern "system" fn(u32, *mut c_void, *const i32),
}

/// Resolve one entry point, or say which one is missing.
///
/// **The suffixed name is tried too.** An interface promoted from an extension
/// keeps its suffixed spelling on some drivers and drops it on others, and a
/// driver that exports only one of the two is not a driver that cannot do this.
fn entry<T: Copy>(
    egl: &egl::DynamicInstance<egl::EGL1_5>,
    name: &'static str,
    suffixed: &str,
) -> Result<T, Error> {
    const {
        assert!(
            size_of::<T>() == size_of::<*mut c_void>(),
            "an entry point must be pointer sized"
        );
    }
    let address = egl
        .get_proc_address(name)
        .or_else(|| egl.get_proc_address(suffixed))
        .ok_or(Error::NoEntryPoint(name))?;
    // SAFETY: the address came from the driver for this exact name, and the
    // signature above is the one the interface specifies for it. `T` is
    // asserted pointer sized.
    Ok(unsafe { core::mem::transmute_copy::<extern "system" fn(), T>(&address) })
}

impl Gl {
    /// Resolve everything the conversion calls.
    fn load(egl: &egl::DynamicInstance<egl::EGL1_5>) -> Result<Self, Error> {
        // A name with no suffixed spelling passes its own, which resolves the
        // same way and costs one failed lookup that never happens.
        macro_rules! plain {
            ($name:literal) => {
                entry(egl, concat!("gl", $name), concat!("gl", $name))?
            };
        }
        Ok(Self {
            GetError: plain!("GetError"),
            GetIntegerv: plain!("GetIntegerv"),
            GetStringi: plain!("GetStringi"),
            CreateShader: plain!("CreateShader"),
            ShaderSource: plain!("ShaderSource"),
            CompileShader: plain!("CompileShader"),
            GetShaderiv: plain!("GetShaderiv"),
            GetShaderInfoLog: plain!("GetShaderInfoLog"),
            DeleteShader: plain!("DeleteShader"),
            CreateProgram: plain!("CreateProgram"),
            AttachShader: plain!("AttachShader"),
            LinkProgram: plain!("LinkProgram"),
            GetProgramiv: plain!("GetProgramiv"),
            GetProgramInfoLog: plain!("GetProgramInfoLog"),
            UseProgram: plain!("UseProgram"),
            DeleteProgram: plain!("DeleteProgram"),
            GenTextures: plain!("GenTextures"),
            DeleteTextures: plain!("DeleteTextures"),
            BindTexture: plain!("BindTexture"),
            ActiveTexture: plain!("ActiveTexture"),
            TexStorage2D: plain!("TexStorage2D"),
            TexSubImage2D: plain!("TexSubImage2D"),
            GetTexImage: plain!("GetTexImage"),
            TexParameteri: plain!("TexParameteri"),
            BindImageTexture: plain!("BindImageTexture"),
            GenBuffers: plain!("GenBuffers"),
            DeleteBuffers: plain!("DeleteBuffers"),
            BindBuffer: plain!("BindBuffer"),
            BufferData: plain!("BufferData"),
            BufferSubData: plain!("BufferSubData"),
            GetBufferSubData: plain!("GetBufferSubData"),
            BindBufferBase: plain!("BindBufferBase"),
            Uniform2i: plain!("Uniform2i"),
            Uniform1ui: plain!("Uniform1ui"),
            DispatchCompute: plain!("DispatchCompute"),
            MemoryBarrier: plain!("MemoryBarrier"),
            Finish: plain!("Finish"),
            PixelStorei: plain!("PixelStorei"),
            EGLImageTargetTexStorage: entry(
                egl,
                "glEGLImageTargetTexStorageEXT",
                "glEGLImageTargetTexStorageEXT",
            )?,
        })
    }

    /// Whatever the interface has been holding against us, as an error.
    ///
    /// **Called after each step rather than at the end.** This interface
    /// records a fault and carries on, so a single check at the end names the
    /// last call and not the one that failed.
    fn check(&self) -> Result<(), Error> {
        // SAFETY: a context is current, which every caller of this holds.
        let code = unsafe { (self.GetError)() };
        if code == raw::NO_ERROR {
            Ok(())
        } else {
            Err(Error::Gl(code))
        }
    }
}

/// The device the display is on, ready to import from it.
///
/// Not `Send`: the context below is current on the thread that built it, and
/// the interface offers no way to use it from another without moving it first.
pub struct Device {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    context: egl::Context,
    gl: Gl,
    name: String,
}

/// What one device came up as, before the instance is moved in beside it.
struct Opened {
    display: egl::Display,
    context: egl::Context,
    gl: Gl,
    name: String,
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn egl_error(error: egl::Error) -> Error {
    Error::Egl(error.into())
}

/// A device node's major and minor numbers.
fn node_numbers(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    Some((major, minor))
}

impl Device {
    /// Open the device that drives a given display node.
    ///
    /// **Matched on the node's own numbers, not on the path's spelling.** The
    /// interface reports the device file it drives and two paths can name one
    /// device, so the two are compared after the filesystem has resolved them.
    pub fn for_display(node: &Path) -> Result<Self, Error> {
        let wanted = node_numbers(node).ok_or(Error::NoDeviceForNode)?;
        Self::build(|egl, device| {
            // SAFETY: the device came from this interface's own enumeration.
            let file = unsafe { query_device_string(egl, device, DRM_DEVICE_FILE) };
            file.and_then(|file| node_numbers(Path::new(&file))) == Some(wanted)
        })
    }

    /// Open any device that can do this, for a test with no display attached.
    ///
    /// **Not for the product**, for the same reason the primary interface's
    /// equivalent is not: a conversion has to run where the frame already is.
    pub fn any() -> Result<Self, Error> {
        Self::build(|_, _| true)
    }

    /// Open the first enumerated device the predicate accepts.
    ///
    /// **The instance is built here and moved in at the end.** It owns the
    /// loaded library, so it cannot be handed to the steps below by value and
    /// cannot be copied; they borrow it and report what they built instead.
    fn build(
        wanted: impl Fn(&egl::DynamicInstance<egl::EGL1_5>, egl::Attrib) -> bool,
    ) -> Result<Self, Error> {
        // SAFETY: loads the display interface by its versioned name. The
        // instance owns the library for as long as it lives.
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .map_err(|_| Error::NoLoader)?;

        // SAFETY: enumeration is by the interface's own extension, resolved
        // against the client rather than a display, which is what makes it
        // callable before any display exists.
        let devices = unsafe { query_devices(&egl)? };
        let mut opened = None;
        for device in devices {
            if !wanted(&egl, device) {
                continue;
            }
            match Self::open(&egl, device) {
                Ok(parts) => {
                    opened = Some(parts);
                    break;
                }
                // A device that enumerates and cannot do this is not the one;
                // the next may be. The refusal survives only if none opens.
                Err(error) => {
                    lowlat_common::log_debug!("gl: a device refused, {error}");
                }
            }
        }
        let Opened {
            display,
            context,
            gl,
            name,
        } = opened.ok_or(Error::NoDeviceForNode)?;
        Ok(Self {
            egl,
            display,
            context,
            gl,
            name,
        })
    }

    /// Bring one enumerated device up, or say what it cannot do.
    fn open(egl: &egl::DynamicInstance<egl::EGL1_5>, device: egl::Attrib) -> Result<Opened, Error> {
        // SAFETY: the device came from this interface's enumeration, and no
        // native display handle is involved on this platform.
        let display = unsafe {
            egl.get_platform_display(PLATFORM_DEVICE, device as *mut c_void, &[egl::ATTRIB_NONE])
        }
        .map_err(egl_error)?;
        let opened = Self::configure(egl, display);
        if opened.is_err() {
            let _ = egl.terminate(display);
        }
        opened
    }

    /// Everything after the display exists, so a failure has one place to undo.
    fn configure(
        egl: &egl::DynamicInstance<egl::EGL1_5>,
        display: egl::Display,
    ) -> Result<Opened, Error> {
        egl.initialize(display).map_err(egl_error)?;

        let extensions = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .map_err(egl_error)?
            .to_string_lossy()
            .into_owned();
        for wanted in REQUIRED_DISPLAY {
            if !names(&extensions, wanted) {
                return Err(Error::Unsupported(wanted));
            }
        }

        // The drawing interface rather than the embedded one. Both would serve,
        // and the embedded one reaches more devices, but its shading language
        // spells the conversion's declarations differently and one source is
        // worth more here than one more device.
        egl.bind_api(egl::OPENGL_API).map_err(egl_error)?;

        // 4.3 is where a compute shader, an immutable texture and a shader
        // storage buffer all exist, and nothing here needs anything later.
        let attributes = [
            egl::CONTEXT_MAJOR_VERSION,
            4,
            egl::CONTEXT_MINOR_VERSION,
            3,
            egl::CONTEXT_OPENGL_PROFILE_MASK,
            egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ];
        // No configuration: nothing is drawn to a surface, so asking for one
        // would reject devices that can serve us perfectly well.
        let context = egl
            .create_context(
                display,
                unsafe { egl::Config::from_ptr(core::ptr::null_mut()) },
                None,
                &attributes,
            )
            .map_err(egl_error)?;

        let built = Self::finish(egl, display, context);
        if built.is_err() {
            let _ = egl.destroy_context(display, context);
        }
        built
    }

    /// Make the context current and check what it can do.
    fn finish(
        egl: &egl::DynamicInstance<egl::EGL1_5>,
        display: egl::Display,
        context: egl::Context,
    ) -> Result<Opened, Error> {
        egl.make_current(display, None, None, Some(context))
            .map_err(egl_error)?;
        let gl = Gl::load(egl)?;

        let mut count = 0_i32;
        // SAFETY: a context is current and the destination is one integer.
        unsafe { (gl.GetIntegerv)(raw::NUM_EXTENSIONS, &raw mut count) };
        gl.check()?;
        let mut available = String::new();
        for at in 0..count.max(0) {
            // SAFETY: the index is below the count the interface just gave,
            // and the pointer it returns is a NUL-terminated static string.
            let name = unsafe { (gl.GetStringi)(raw::EXTENSIONS, at.unsigned_abs()) };
            if name.is_null() {
                continue;
            }
            // SAFETY: non-null and NUL-terminated, as the interface specifies.
            let name = unsafe { CStr::from_ptr(name.cast()) };
            available.push_str(&name.to_string_lossy());
            available.push(' ');
        }
        for wanted in REQUIRED_DRAWING {
            if !names(&available, wanted) {
                return Err(Error::Unsupported(wanted));
            }
        }

        let name = egl
            .query_string(Some(display), egl::VENDOR)
            .map(|vendor| vendor.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "an unnamed driver".to_string());

        Ok(Opened {
            display,
            context,
            gl,
            name,
        })
    }

    /// What the driver calls itself, for a startup log line.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

/// One attribute value, widened to what the attribute list carries.
///
/// **Widened rather than cast.** Every value that reaches an attribute list
/// here is a size, an offset, a pitch, a descriptor or half a modifier, all of
/// which are unsigned and none of which can be negative; a cast would be the
/// lint the data path forbids and would hide a wrong one rather than refuse it.
fn attrib(value: u32) -> egl::Attrib {
    egl::Attrib::try_from(value).unwrap_or(0)
}

/// Whether a space-separated list names something exactly.
///
/// **Compared whole, not as a substring.** Every name here is a prefix of a
/// longer one that exists, so a substring test reports support for an
/// interface the driver never claimed.
fn names(list: &str, wanted: &str) -> bool {
    list.split_whitespace().any(|entry| entry == wanted)
}

/// Enumerate the devices this interface can see.
///
/// # Safety
///
/// The instance must be live for the duration of the call.
unsafe fn query_devices(
    egl: &egl::DynamicInstance<egl::EGL1_5>,
) -> Result<Vec<egl::Attrib>, Error> {
    type QueryDevices =
        unsafe extern "system" fn(egl::Int, *mut egl::Attrib, *mut egl::Int) -> egl::Boolean;
    let query: QueryDevices = entry(egl, "eglQueryDevicesEXT", "eglQueryDevicesEXT")?;

    let mut count = 0_i32;
    // SAFETY: asking for the count writes only the counter, as specified when
    // the destination is null.
    if unsafe { query(0, core::ptr::null_mut(), &raw mut count) } == egl::FALSE {
        return Err(Error::Unsupported("EGL_EXT_device_enumeration"));
    }
    let wanted = usize::try_from(count.max(0)).unwrap_or(0);
    let mut devices = vec![0_usize as egl::Attrib; wanted];
    // SAFETY: the destination holds `count` entries, which is what the count
    // query above reported and what is passed as the capacity.
    if unsafe { query(count, devices.as_mut_ptr(), &raw mut count) } == egl::FALSE {
        return Err(Error::Unsupported("EGL_EXT_device_enumeration"));
    }
    devices.truncate(usize::try_from(count.max(0)).unwrap_or(0));
    Ok(devices)
}

/// One string a device reports about itself, or nothing if it does not.
///
/// # Safety
///
/// `device` must have come from this instance's own enumeration.
unsafe fn query_device_string(
    egl: &egl::DynamicInstance<egl::EGL1_5>,
    device: egl::Attrib,
    name: egl::Int,
) -> Option<String> {
    type QueryDeviceString =
        unsafe extern "system" fn(egl::Attrib, egl::Int) -> *const core::ffi::c_char;
    let query: QueryDeviceString =
        entry(egl, "eglQueryDeviceStringEXT", "eglQueryDeviceStringEXT").ok()?;
    // SAFETY: the device came from this instance's enumeration, as the caller
    // guarantees; the result is NUL-terminated or null.
    let text = unsafe { query(device, name) };
    if text.is_null() {
        return None;
    }
    // SAFETY: non-null and NUL-terminated, as the interface specifies.
    Some(
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// A captured framebuffer, imported without a copy.
///
/// Holds the texture and, for a real capture, the image it was built from.
pub struct Imported {
    /// Absent for a frame built here from bytes, which has no image behind it.
    image: Option<egl::Image>,
    texture: u32,
    pub width: u32,
    pub height: u32,
}

impl core::fmt::Debug for Imported {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Imported")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// A converted frame: the two planes an encoder reads.
///
/// **Two textures and no allocation of our own**, which is where this differs
/// from the primary interface and why nothing here can be handed to an encoder
/// yet. That interface allocates the memory and lends the planes a view of it,
/// so one descriptor leaves for either encoder; this one has the driver
/// allocate and does not say where. Giving an encoder these bytes needs the
/// allocation to come from outside and be imported the way a capture is, which
/// is the next piece rather than a missing one.
pub struct Nv12 {
    luma: u32,
    chroma: u32,
    pub width: u32,
    pub height: u32,
}

impl core::fmt::Debug for Nv12 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Nv12")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Everything the conversion needs that outlives a single frame.
pub struct Converter {
    program: u32,
    /// Where the shader accumulates its summary.
    ///
    /// **One buffer, read back rather than mapped.** The read is what waits
    /// for the dispatch, so nothing here needs a fence of its own; this
    /// interface has no equivalent of one to reuse and no equivalent cost to
    /// avoid.
    summary: u32,
}

impl core::fmt::Debug for Converter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Converter").finish_non_exhaustive()
    }
}

impl Device {
    /// Import a captured framebuffer.
    ///
    /// **The descriptor stays the caller's.** This interface duplicates what it
    /// needs, which is the opposite of the primary one and the kind of
    /// difference that leaks a descriptor per frame if it is assumed either way.
    ///
    /// The tiling modifier and the per-plane pitches are handed over rather
    /// than inferred, for the same reason they are there: a scanout buffer read
    /// as though it were plain rows is garbage.
    pub fn import(&self, source: &Imports<'_>) -> Result<Imported, Error> {
        let planes = source.planes;
        if planes.len() > PLANE_ATTRIBUTES.len() {
            return Err(Error::TooManyPlanes);
        }

        let mut attributes: Vec<egl::Attrib> = vec![
            WIDTH,
            attrib(source.width),
            HEIGHT,
            attrib(source.height),
            LINUX_DRM_FOURCC,
            attrib(source.format as u32),
        ];
        for (plane, names) in planes.iter().zip(PLANE_ATTRIBUTES) {
            // **The modifier goes in as two halves, and both are given even
            // when it is zero.** Untiled is a modifier like any other here;
            // omitting it asks the driver to guess, and one of them guesses
            // wrong on a buffer that is genuinely untiled.
            attributes.extend_from_slice(&[
                names[0],
                attrib(source.fd.unsigned_abs()),
                names[1],
                attrib(plane.offset),
                names[2],
                attrib(plane.pitch),
                names[3],
                attrib(u32::try_from(source.modifier & 0xffff_ffff).unwrap_or(0)),
                names[4],
                attrib(u32::try_from(source.modifier >> 32).unwrap_or(0)),
            ]);
        }
        attributes.push(egl::ATTRIB_NONE);

        // SAFETY: no context and no client buffer, which is what this target
        // takes; everything describing the buffer is in the attributes.
        let image = self
            .egl
            .create_image(
                self.display,
                unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) },
                LINUX_DMA_BUF,
                unsafe { egl::ClientBuffer::from_ptr(core::ptr::null_mut()) },
                &attributes,
            )
            .map_err(egl_error)?;

        match self.texture_from(image) {
            Ok(texture) => Ok(Imported {
                image: Some(image),
                texture,
                width: source.width,
                height: source.height,
            }),
            Err(error) => {
                let _ = self.egl.destroy_image(self.display, image);
                Err(error)
            }
        }
    }

    /// Build an immutable texture over an imported image.
    ///
    /// **Immutable, which is the whole reason this needs an extension.** The
    /// long-standing way of taking an image as a texture produces a mutable one,
    /// and the conversion writes through image bindings, which a mutable
    /// texture cannot serve.
    fn texture_from(&self, image: egl::Image) -> Result<u32, Error> {
        let gl = &self.gl;
        let mut texture = 0_u32;
        // SAFETY: a context is current, and every destination is one name.
        unsafe {
            (gl.GenTextures)(1, &raw mut texture);
            (gl.BindTexture)(raw::TEXTURE_2D, texture);
            (gl.EGLImageTargetTexStorage)(raw::TEXTURE_2D, image.as_ptr(), core::ptr::null());
        }
        if let Err(error) = gl.check() {
            // SAFETY: created just above and bound to nothing else.
            unsafe { (gl.DeleteTextures)(1, &raw const texture) };
            return Err(error);
        }
        self.sample_exactly(texture);
        gl.check()?;
        Ok(texture)
    }

    /// Read the texture as stored, with nothing interpolated.
    ///
    /// The conversion fetches by coordinate and never samples between pixels,
    /// so filtering cannot change a result. It is set anyway because the
    /// default is not this, and a later reader that does sample would silently
    /// get a blurred picture rather than a refusal.
    fn sample_exactly(&self, texture: u32) {
        let gl = &self.gl;
        // SAFETY: a context is current and the texture is this device's.
        unsafe {
            (gl.BindTexture)(raw::TEXTURE_2D, texture);
            for (name, value) in [
                (raw::TEXTURE_MIN_FILTER, raw::NEAREST),
                (raw::TEXTURE_MAG_FILTER, raw::NEAREST),
                (raw::TEXTURE_WRAP_S, raw::CLAMP_TO_EDGE),
                (raw::TEXTURE_WRAP_T, raw::CLAMP_TO_EDGE),
            ] {
                (gl.TexParameteri)(raw::TEXTURE_2D, name, value);
            }
        }
    }

    /// Release an imported frame.
    pub fn release(&self, imported: Imported) {
        // SAFETY: the texture is this device's and nothing else refers to it.
        unsafe { (self.gl.DeleteTextures)(1, &raw const imported.texture) };
        if let Some(image) = imported.image {
            let _ = self.egl.destroy_image(self.display, image);
        }
    }

    /// Allocate a conversion target.
    pub fn allocate_nv12(&self, width: u32, height: u32) -> Result<Nv12, Error> {
        // Both dimensions round up to even, as on the primary interface: a
        // plane at half resolution has no meaning for an odd one, and the
        // shader's last block would write outside the colour plane.
        let width = width.next_multiple_of(2);
        let height = height.next_multiple_of(2);

        let luma = self.plane_texture(raw::R8, width, height)?;
        match self.plane_texture(raw::RG8, width / 2, height / 2) {
            Ok(chroma) => Ok(Nv12 {
                luma,
                chroma,
                width,
                height,
            }),
            Err(error) => {
                // SAFETY: created just above and bound to nothing else.
                unsafe { (self.gl.DeleteTextures)(1, &raw const luma) };
                Err(error)
            }
        }
    }

    /// One plane, immutable so it can be written through an image binding.
    fn plane_texture(&self, format: u32, width: u32, height: u32) -> Result<u32, Error> {
        let gl = &self.gl;
        let mut texture = 0_u32;
        // SAFETY: a context is current; the destination is one name and the
        // extents are the ones asked for.
        unsafe {
            (gl.GenTextures)(1, &raw mut texture);
            (gl.BindTexture)(raw::TEXTURE_2D, texture);
            (gl.TexStorage2D)(
                raw::TEXTURE_2D,
                1,
                format,
                i32::try_from(width).unwrap_or(i32::MAX),
                i32::try_from(height).unwrap_or(i32::MAX),
            );
        }
        if let Err(error) = gl.check() {
            // SAFETY: created just above.
            unsafe { (gl.DeleteTextures)(1, &raw const texture) };
            return Err(error);
        }
        Ok(texture)
    }

    /// Release a conversion target.
    pub fn release_nv12(&self, nv12: Nv12) {
        let names = [nv12.luma, nv12.chroma];
        // SAFETY: both are this device's and nothing else refers to them.
        unsafe { (self.gl.DeleteTextures)(2, names.as_ptr()) };
    }
}

impl Converter {
    /// Build the conversion pipeline.
    pub fn new(device: &Device) -> Result<Self, Error> {
        let gl = &device.gl;
        let program = Self::build(device)?;

        let mut summary = 0_u32;
        // SAFETY: a context is current; the destination is one name and the
        // buffer is the size the shader's own declaration comes to.
        unsafe {
            (gl.GenBuffers)(1, &raw mut summary);
            (gl.BindBuffer)(raw::SHADER_STORAGE_BUFFER, summary);
            (gl.BufferData)(
                raw::SHADER_STORAGE_BUFFER,
                DIGEST as isize,
                core::ptr::null(),
                raw::DYNAMIC_READ,
            );
        }
        if let Err(error) = gl.check() {
            // SAFETY: both were created above and nothing refers to them.
            unsafe {
                (gl.DeleteBuffers)(1, &raw const summary);
                (gl.DeleteProgram)(program);
            }
            return Err(error);
        }
        Ok(Self { program, summary })
    }

    /// Compile and link the shader, reporting its log if it will not build.
    fn build(device: &Device) -> Result<u32, Error> {
        let gl = &device.gl;
        // SAFETY: a context is current. The source is borrowed for the call
        // only, and its length is given rather than relying on a terminator,
        // which the embedded text does not have.
        let shader = unsafe {
            let shader = (gl.CreateShader)(raw::COMPUTE_SHADER);
            let text = CONVERT.as_ptr();
            let length = i32::try_from(CONVERT.len()).unwrap_or(i32::MAX);
            (gl.ShaderSource)(shader, 1, &raw const text, &raw const length);
            (gl.CompileShader)(shader);
            shader
        };
        let mut status = 0_i32;
        // SAFETY: the shader was created above; the destination is one integer.
        unsafe { (gl.GetShaderiv)(shader, raw::COMPILE_STATUS, &raw mut status) };
        if status == 0 {
            log_build_failure(gl, shader, false);
            // SAFETY: created above and attached to nothing.
            unsafe { (gl.DeleteShader)(shader) };
            return Err(Error::BadShader);
        }

        // SAFETY: the shader compiled, so it can be attached and linked.
        let program = unsafe {
            let program = (gl.CreateProgram)();
            (gl.AttachShader)(program, shader);
            (gl.LinkProgram)(program);
            (gl.DeleteShader)(shader);
            program
        };
        // SAFETY: the program was created above; the destination is one
        // integer.
        unsafe { (gl.GetProgramiv)(program, raw::LINK_STATUS, &raw mut status) };
        if status == 0 {
            log_build_failure(gl, program, true);
            // SAFETY: created above and used by nothing.
            unsafe { (gl.DeleteProgram)(program) };
            return Err(Error::BadShader);
        }
        Ok(program)
    }

    /// Convert one captured frame into the target.
    ///
    /// Blocks until the device has finished, like the primary interface's, and
    /// for the same reason: the read of the summary is what waits.
    pub fn run(
        &self,
        device: &Device,
        source: &Imported,
        target: &Nv12,
        dither: bool,
    ) -> Result<crate::convert::Digest, Error> {
        let gl = &device.gl;
        // One invocation per 2x2 block, rounded up so an odd edge is covered.
        let groups_x = source.width.div_ceil(2).div_ceil(GROUP);
        let groups_y = source.height.div_ceil(2).div_ceil(GROUP);
        let zeroed = [0_u8; DIGEST];
        let mut value = [0_u8; DIGEST];

        // SAFETY: a context is current and every name below is this device's.
        // The two reads write exactly the eight bytes their destinations hold,
        // which is the size the buffer was created with.
        unsafe {
            (gl.UseProgram)(self.program);

            // The source reaches the shader as a sampler on the first unit,
            // which is what its binding names.
            (gl.ActiveTexture)(raw::TEXTURE0);
            (gl.BindTexture)(raw::TEXTURE_2D, source.texture);

            // Each plane is written through its own binding. Writing the
            // two-plane format directly is what drops colour on more than one
            // driver, which is why there are two.
            (gl.BindImageTexture)(1, target.luma, 0, 0, 0, raw::WRITE_ONLY, raw::R8);
            (gl.BindImageTexture)(2, target.chroma, 0, 0, 0, raw::WRITE_ONLY, raw::RG8);

            // **Zeroed before the dispatch and not after.** The shader adds to
            // whatever it finds, so a summary left over from the previous frame
            // makes every frame differ from the one before it.
            (gl.BindBuffer)(raw::SHADER_STORAGE_BUFFER, self.summary);
            (gl.BufferSubData)(
                raw::SHADER_STORAGE_BUFFER,
                0,
                DIGEST as isize,
                zeroed.as_ptr().cast(),
            );
            (gl.BindBufferBase)(raw::SHADER_STORAGE_BUFFER, 3, self.summary);

            (gl.Uniform2i)(
                0,
                i32::try_from(source.width).unwrap_or(i32::MAX),
                i32::try_from(source.height).unwrap_or(i32::MAX),
            );
            (gl.Uniform1ui)(1, u32::from(dither));

            (gl.DispatchCompute)(groups_x, groups_y, 1);
            (gl.MemoryBarrier)(raw::AFTER_DISPATCH);
            (gl.GetBufferSubData)(
                raw::SHADER_STORAGE_BUFFER,
                0,
                DIGEST as isize,
                value.as_mut_ptr().cast(),
            );
        }
        gl.check()?;
        Ok(crate::convert::Digest(u64::from_le_bytes(value)))
    }

    /// Release the pipeline.
    pub fn destroy(self, device: &Device) {
        // SAFETY: both are this device's and nothing submitted still refers to
        // them, because every run here waits before it returns.
        unsafe {
            (device.gl.DeleteBuffers)(1, &raw const self.summary);
            (device.gl.DeleteProgram)(self.program);
        }
    }
}

/// Put a build failure's own account of itself into the log.
///
/// **The log is the whole diagnostic.** A shader that will not build says why
/// in a string the driver writes, and without it the refusal is one word.
fn log_build_failure(gl: &Gl, name: u32, linking: bool) {
    let mut length = 0_i32;
    // SAFETY: the name is a shader or a program as `linking` says, and the
    // destination is one integer.
    unsafe {
        if linking {
            (gl.GetProgramiv)(name, raw::INFO_LOG_LENGTH, &raw mut length);
        } else {
            (gl.GetShaderiv)(name, raw::INFO_LOG_LENGTH, &raw mut length);
        }
    }
    let wanted = usize::try_from(length.max(0)).unwrap_or(0);
    if wanted == 0 {
        lowlat_common::log_error!("gl: the conversion shader did not build, and said nothing");
        return;
    }
    let mut text = vec![0_u8; wanted];
    let mut written = 0_i32;
    // SAFETY: the destination holds `wanted` bytes, which is the length the
    // interface just reported and what is passed as the capacity.
    unsafe {
        if linking {
            (gl.GetProgramInfoLog)(name, length, &raw mut written, text.as_mut_ptr());
        } else {
            (gl.GetShaderInfoLog)(name, length, &raw mut written, text.as_mut_ptr());
        }
    }
    text.truncate(usize::try_from(written.max(0)).unwrap_or(0));
    lowlat_common::log_error!(
        "gl: the conversion shader did not build, {}",
        String::from_utf8_lossy(&text).trim()
    );
}

impl Device {
    /// Build a frame from bytes, for a test with a known answer.
    ///
    /// **Not a capture path**, exactly as on the primary interface: a captured
    /// frame is already on the device. This exists so the colour transform can
    /// be checked against a reference with no display attached, which is the
    /// only check of it that a desktop cannot fool.
    pub fn upload_rgba(&self, width: u32, height: u32, pixels: &[u8]) -> Result<Imported, Error> {
        let wanted = (width as usize) * (height as usize) * 4;
        if pixels.len() < wanted {
            return Err(Error::Unsupported("a frame shorter than its own extent"));
        }
        let gl = &self.gl;
        let texture = self.plane_texture(raw::RGBA8, width, height)?;
        // SAFETY: a context is current, the texture was created just above at
        // exactly this extent, and the source holds the bytes that covers.
        unsafe {
            (gl.BindTexture)(raw::TEXTURE_2D, texture);
            (gl.TexSubImage2D)(
                raw::TEXTURE_2D,
                0,
                0,
                0,
                i32::try_from(width).unwrap_or(i32::MAX),
                i32::try_from(height).unwrap_or(i32::MAX),
                raw::RGBA,
                raw::UNSIGNED_BYTE,
                pixels.as_ptr().cast(),
            );
        }
        if let Err(error) = gl.check() {
            // SAFETY: created above and bound to nothing else.
            unsafe { (gl.DeleteTextures)(1, &raw const texture) };
            return Err(error);
        }
        self.sample_exactly(texture);
        Ok(Imported {
            image: None,
            texture,
            width,
            height,
        })
    }

    /// Copy a converted frame out into ordinary memory.
    ///
    /// **A diagnostic.** The loop never does this. It exists because a colour
    /// transform that is wrong in the matrix or in the range still produces a
    /// picture, and only comparing one against its source says which.
    pub fn read_nv12(&self, nv12: &Nv12) -> Result<(Vec<u8>, Vec<u8>), Error> {
        let gl = &self.gl;
        let luma_bytes = (nv12.width as usize) * (nv12.height as usize);
        let mut luma = vec![0_u8; luma_bytes];
        let mut chroma = vec![0_u8; luma_bytes / 2];
        // SAFETY: a context is current; both destinations are the size their
        // plane's extent and component count come to, and the packing is set
        // to add no padding so the interface writes exactly that many bytes.
        unsafe {
            (gl.PixelStorei)(raw::PACK_ALIGNMENT, 1);
            (gl.BindTexture)(raw::TEXTURE_2D, nv12.luma);
            (gl.GetTexImage)(
                raw::TEXTURE_2D,
                0,
                raw::RED,
                raw::UNSIGNED_BYTE,
                luma.as_mut_ptr().cast(),
            );
            (gl.BindTexture)(raw::TEXTURE_2D, nv12.chroma);
            (gl.GetTexImage)(
                raw::TEXTURE_2D,
                0,
                raw::RG,
                raw::UNSIGNED_BYTE,
                chroma.as_mut_ptr().cast(),
            );
            (gl.Finish)();
        }
        gl.check()?;
        Ok((luma, chroma))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Colours that can tell one matrix from another, each filling a whole 2x2
    /// block so subsampling has nothing to average.
    const PATTERN: [[u8; 3]; 8] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [0, 255, 255],
        [255, 0, 255],
        [255, 255, 255],
        [0, 0, 0],
    ];

    /// The pattern as a frame, one block per colour along a row.
    fn frame() -> (u32, u32, Vec<u8>) {
        let width = u32::try_from(PATTERN.len()).unwrap_or(1) * 2;
        let height = 2;
        let mut pixels = vec![0_u8; (width as usize) * (height as usize) * 4];
        for (at, colour) in PATTERN.iter().enumerate() {
            for y in 0..2_usize {
                for x in 0..2_usize {
                    let base = ((y * (width as usize)) + at * 2 + x) * 4;
                    pixels[base] = colour[0];
                    pixels[base + 1] = colour[1];
                    pixels[base + 2] = colour[2];
                    pixels[base + 3] = 255;
                }
            }
        }
        (width, height, pixels)
    }

    /// **The two interfaces produce the same picture, to the byte.**
    ///
    /// This is the only check that says the fallback is a fallback rather than
    /// a second implementation with its own colour. The summary is what is
    /// compared, because that is what the two shaders compute over every pixel
    /// they wrote: agreeing here means agreeing on the whole frame, and a
    /// single sample landing on a different byte moves it.
    ///
    /// Both run against whatever device comes first, which on a machine with
    /// two vendors is not reliably the same one -- and that is the stronger
    /// test, not a weaker one.
    ///
    /// **Runs by default**, like its counterpart on the primary interface: it
    /// needs a driver and not a graphics card.
    #[test]
    fn the_two_interfaces_agree() {
        let (width, height, pixels) = frame();

        let gl_device = Device::any().expect("a device that can convert");
        let gl_converter = Converter::new(&gl_device).expect("a pipeline");
        let gl_source = gl_device
            .upload_rgba(width, height, &pixels)
            .expect("upload");
        let gl_target = gl_device.allocate_nv12(width, height).expect("a target");
        let theirs = gl_converter
            .run(&gl_device, &gl_source, &gl_target, false)
            .expect("convert");

        let vk_device = crate::vulkan::Device::any().expect("a device that can convert");
        let vk_converter = crate::convert::Converter::new(&vk_device).expect("a pipeline");
        let vk_source = vk_device
            .upload_rgba(width, height, &pixels)
            .expect("upload");
        let vk_target = vk_device.allocate_nv12(width, height).expect("a target");
        let ours = vk_converter
            .run(&vk_device, &vk_source, &vk_target, false)
            .expect("convert");

        assert_eq!(
            ours, theirs,
            "the two interfaces converted one picture differently"
        );

        gl_device.release(gl_source);
        gl_device.release_nv12(gl_target);
        gl_converter.destroy(&gl_device);
        vk_device.release(vk_source);
        vk_device.release_nv12(vk_target);
        vk_converter.destroy(&vk_device);
    }

    /// The summary answers the only question asked of it: is this picture the
    /// one before it. The same three properties its counterpart checks, because
    /// a fallback that suppresses the wrong frames is worse than none.
    #[test]
    fn the_summary_tells_one_picture_from_another() {
        let width = 64;
        let height = 64;
        let device = Device::any().expect("a device that can convert");
        let converter = Converter::new(&device).expect("a pipeline");
        let target = device.allocate_nv12(width, height).expect("a target");

        let digest_of = |pixels: &[u8]| {
            let source = device.upload_rgba(width, height, pixels).expect("upload");
            let got = converter
                .run(&device, &source, &target, false)
                .expect("convert");
            device.release(source);
            got
        };

        let mut plain = vec![0_u8; (width as usize) * (height as usize) * 4];
        for (at, byte) in plain.iter_mut().enumerate() {
            *byte = u8::try_from(at % 251).unwrap_or(0);
        }
        let mut changed = plain.clone();
        changed[4000] = changed[4000].wrapping_add(64);

        // The same pixels, one 2x2 block exchanged with another. A sum and an
        // exclusive-or are both blind to order, so this is what says position
        // reached the summary.
        let mut moved = plain.clone();
        let row = (width as usize) * 4;
        for y in 0..2_usize {
            for x in 0..8_usize {
                moved.swap(y * row + x, y * row + row / 2 + x);
            }
        }

        assert_eq!(digest_of(&plain), digest_of(&plain), "one picture, twice");
        assert_ne!(digest_of(&plain), digest_of(&changed), "one byte moved");
        assert_ne!(digest_of(&plain), digest_of(&moved), "two blocks exchanged");

        device.release_nv12(target);
        converter.destroy(&device);
    }
}
