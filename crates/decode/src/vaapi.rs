//! The open-stack backend: the device decodes what the readers read.
//!
//! One configuration and one context per decoder, a fixed pool of surfaces
//! the picture buffers index by slot, and each picture as it leaves the
//! buffer either read back or lent: its surface exported to whoever holds
//! the picture, and never decoded into while held. Every buffer the device
//! is handed is staged in storage allocated with the backend; nothing is
//! allocated per picture.

use core::ffi::{CStr, c_int, c_uint, c_void};
use core::sync::atomic::{AtomicU32, Ordering};
use std::os::fd::OwnedFd;

use lowlat_core::video::{Codec, VideoHeader};
use lowlat_drivers::ffi::va::{
    VA_FOURCC_444P, VA_FOURCC_AYUV, VA_FOURCC_NV12, VA_FOURCC_P010, VA_FOURCC_XYUV, VA_FOURCC_Y410,
    VA_INVALID_ID, VA_INVALID_SURFACE, VA_PICTURE_H264_BOTTOM_FIELD, VA_PICTURE_H264_INVALID,
    VA_PICTURE_H264_LONG_TERM_REFERENCE, VA_PICTURE_H264_SHORT_TERM_REFERENCE,
    VA_PICTURE_H264_TOP_FIELD, VA_PICTURE_HEVC_INVALID, VA_PICTURE_HEVC_LONG_TERM_REFERENCE,
    VA_PICTURE_HEVC_RPS_LT_CURR, VA_PICTURE_HEVC_RPS_ST_CURR_AFTER,
    VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE, VA_RT_FORMAT_YUV420, VA_RT_FORMAT_YUV420_10,
    VA_RT_FORMAT_YUV444, VA_RT_FORMAT_YUV444_10, VA_SLICE_DATA_FLAG_ALL,
    VA_SURFACE_ATTRIB_SETTABLE, VABufferID, VAConfigAttrib, VAConfigAttribRTFormat, VAConfigID,
    VAContextID, VAEntrypointVLD, VAGenericValue, VAGenericValueTypeInteger, VAIQMatrixBufferH264,
    VAIQMatrixBufferHEVC, VAIQMatrixBufferType, VAImage, VAPictureH264, VAPictureHEVC,
    VAPictureParameterBufferH264, VAPictureParameterBufferHEVC,
    VAPictureParameterBufferHEVCExtension, VAPictureParameterBufferType, VAProfile,
    VAProfileH264High, VAProfileHEVCMain, VAProfileHEVCMain10, VAProfileHEVCMain444,
    VAProfileHEVCMain444_10, VASliceDataBufferType, VASliceParameterBufferH264,
    VASliceParameterBufferHEVC, VASliceParameterBufferHEVCExtension, VASliceParameterBufferType,
    VASurfaceAttrib, VASurfaceAttribPixelFormat, VASurfaceID,
};
use lowlat_drivers::va;
pub use lowlat_drivers::va::{Display, Error as RuntimeError, Vaapi};

use crate::h264::dpb::{Parity, Structure};
use crate::packed::{unpack_vuyx_row, unpack_y410_row};
use crate::{Caps, Decoder, Fault, Fed, Format, Lease, Picture, Planes, h264, hevc};

/// Surfaces a context holds: what either picture buffer can index.
const SURFACES: usize = h264::dpb::MAX_FRAMES;
/// Surfaces made to stand in for lent ones, at most: one for each picture a
/// holder may keep lent at once. Each is made the first time a slot whose
/// surface is lent is decoded into again, and kept with the context, so a
/// session of planes makes none.
pub const SPARES: usize = 4;
/// Every surface a context may come to hold.
const POOL: usize = SURFACES + SPARES;
/// Contexts made in this process, so a lease names the context it is of.
static CONTEXTS: AtomicU32 = AtomicU32::new(0);
/// Buffers one picture may hand the device: the parameters, the matrix,
/// and a parameter and data buffer per slice.
const BUFFERS: usize = 2 + 2 * h264::MAX_SLICES;

/// Why a decoder could not be built or a picture could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The runtime or the render node.
    Runtime(va::Error),
    /// The device decodes no profile this stream needs.
    NoProfile,
    /// A call failed, with its status.
    Status(i32),
    /// A picture larger than the surfaces.
    TooLarge,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "{e}"),
            Self::NoProfile => f.write_str("device decodes no profile this stream needs"),
            Self::Status(s) => write!(f, "display runtime returned status {s}"),
            Self::TooLarge => f.write_str("picture larger than the surfaces"),
        }
    }
}

impl std::error::Error for Error {}

impl From<va::Error> for Error {
    fn from(e: va::Error) -> Self {
        Self::Runtime(e)
    }
}

type Result<T> = core::result::Result<T, Error>;

/// The stream's shape, which picks the profile, the surfaces' format and
/// the layout the pictures leave in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    ten_bit: bool,
    full_chroma: bool,
}

impl Shape {
    fn profile(self, codec: Codec) -> VAProfile {
        match (codec, self.ten_bit, self.full_chroma) {
            (Codec::H264, _, _) => VAProfileH264High,
            (Codec::H265, false, false) => VAProfileHEVCMain,
            (Codec::H265, true, false) => VAProfileHEVCMain10,
            (Codec::H265, false, true) => VAProfileHEVCMain444,
            (Codec::H265, true, true) => VAProfileHEVCMain444_10,
        }
    }

    fn rt_format(self) -> u32 {
        match (self.ten_bit, self.full_chroma) {
            (false, false) => VA_RT_FORMAT_YUV420,
            (true, false) => VA_RT_FORMAT_YUV420_10,
            (false, true) => VA_RT_FORMAT_YUV444,
            (true, true) => VA_RT_FORMAT_YUV444_10,
        }
    }

    fn format(self) -> Format {
        match (self.ten_bit, self.full_chroma) {
            (false, false) => Format::Nv12,
            (true, false) => Format::P010,
            (false, true) => Format::Yuv444,
            (true, true) => Format::Yuv444_16,
        }
    }
}

/// The surface layouts this backend reads back, in the order it asks for
/// them: planar first, since that is a plain copy, then the packed ones it
/// unpacks.
const FULL_CHROMA_FOURCCS: [u32; 3] = [VA_FOURCC_444P, VA_FOURCC_XYUV, VA_FOURCC_AYUV];
const FULL_CHROMA_TEN_FOURCCS: [u32; 1] = [VA_FOURCC_Y410];

/// The surface format the display would decode a full-chroma stream into
/// that this backend can read, if any: the first of the layouts it reads
/// among those the driver offers for the profile.
fn full_chroma_fourcc(display: &Display<'_>, shape: Shape) -> Option<u32> {
    let offered = display.pixel_formats(
        shape.profile(Codec::H265),
        VAEntrypointVLD,
        shape.rt_format(),
    );
    let readable: &[u32] = if shape.ten_bit {
        &FULL_CHROMA_TEN_FOURCCS
    } else {
        &FULL_CHROMA_FOURCCS
    };
    readable.iter().copied().find(|f| offered.contains(f))
}

/// Ask a display what it decodes. **Full chroma is reported only where the
/// driver offers, for the full-chroma profile, a surface layout this
/// backend reads back** -- planar, or the packed eight-bit and ten-bit
/// layouts it unpacks -- verified on a device that decodes into them; a
/// driver that lists the profile with a layout this backend does not know
/// is not asked for it.
pub fn caps(display: &Display<'_>) -> Result<Caps> {
    let profiles = display.profiles()?;
    let decodes = |profile: VAProfile| -> Result<bool> {
        if !profiles.contains(&profile) {
            return Ok(false);
        }
        Ok(display.entrypoints(profile)?.contains(&VAEntrypointVLD))
    };
    let full = |ten_bit: bool| -> Result<bool> {
        let shape = Shape {
            ten_bit,
            full_chroma: true,
        };
        Ok(decodes(shape.profile(Codec::H265))? && full_chroma_fourcc(display, shape).is_some())
    };
    Ok(Caps {
        h264: decodes(VAProfileH264High)?,
        hevc: decodes(VAProfileHEVCMain)?,
        hevc_10: decodes(VAProfileHEVCMain10)?,
        hevc_444: full(false)?,
        hevc_444_10: full(true)?,
    })
}

/// The largest coded picture a display decodes for a codec, as the driver
/// reports it; zero where it does not say.
pub fn limits(display: &Display<'_>, codec: Codec) -> (u32, u32) {
    let shape = Shape {
        ten_bit: false,
        full_chroma: false,
    };
    display.max_picture(shape.profile(codec), VAEntrypointVLD)
}

/// Open the runtime and a render node and ask what it decodes, and whether
/// it lends its surfaces: the probe a client makes at creation, so a machine
/// without a decoder is refused before anything connects.
pub fn probe(node: &CStr) -> Result<(Caps, bool)> {
    let va = Vaapi::load()?;
    let display = va.open(node)?;
    Ok((caps(&display)?, exports_surfaces(&display)))
}

/// Whether a display exports a surface of the half-chroma layouts as one
/// buffer, in the layout a lent picture is handed out in: one made, exported
/// and destroyed to ask.
pub fn exports_surfaces(display: &Display<'_>) -> bool {
    let va = display.va();
    let mut surface: VASurfaceID = VA_INVALID_SURFACE;
    // SAFETY: the output is a live local, one surface; no attributes.
    let status = unsafe {
        (va.create_surfaces)(
            display.raw(),
            VA_RT_FORMAT_YUV420,
            256,
            256,
            &raw mut surface,
            1,
            core::ptr::null_mut(),
            0,
        )
    };
    if va.check(status).is_err() {
        return false;
    }
    let exported = display.export(surface).is_ok();
    // SAFETY: made above, destroyed once; the export's descriptor, if any,
    // is closed already and keeps nothing of it.
    unsafe { (va.destroy_surfaces)(display.raw(), &raw mut surface, 1) };
    exported
}

/// A picture lent in its surface: the surface's export, which the holder
/// imports, for as long as it keeps the lease.
#[derive(Debug)]
pub struct Lent {
    /// A duplicate of the export's descriptor, the holder's to close.
    pub fd: OwnedFd,
    /// The buffer's whole size, which is what an import is told.
    pub size: u64,
    pub modifier: u64,
    /// The chroma's place in the buffer; the luma is at its start.
    pub chroma_offset: u32,
    /// Bytes a row: the luma's, then the chroma's.
    pub pitches: [u32; 2],
    /// The export's number, as the holder's counter gave it the first time
    /// this surface was lent: the same surface keeps it for its context's
    /// life, so a holder imports it once.
    pub allocation: u32,
    /// The render node's device number, major in the high half.
    pub device: u64,
    /// What the holder names back when it lets the surface go.
    pub lease: Lease,
}

/// Which surface each picture-buffer slot decodes into, and which surfaces
/// a holder keeps: the bookkeeping that keeps a lent surface from being
/// decoded into, apart from the device calls. A slot decoded into while its
/// own surface is lent is given another one; the picture buffers never see
/// the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pool {
    /// Each slot's surface, an index among the made ones.
    map: [usize; SURFACES],
    lent: [bool; POOL],
    /// Surfaces made: the context's own, then the spares.
    made: usize,
    /// The context the surfaces belong to; zero for none.
    context: u32,
}

/// What a slot about to be decoded into is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cover {
    /// Its own surface, which no holder keeps.
    Keep,
    /// A surface made already that nothing holds or decodes into.
    Spare(usize),
    /// A new surface, at this index.
    Make(usize),
}

impl Pool {
    /// `made` surfaces of `context`, each slot on its own one.
    fn fresh(made: usize, context: u32) -> Self {
        let mut map = [0; SURFACES];
        for (slot, surface) in map.iter_mut().enumerate() {
            *surface = slot;
        }
        Self {
            map,
            lent: [false; POOL],
            made,
            context,
        }
    }

    fn surface(&self, slot: usize) -> Option<usize> {
        self.map.get(slot).copied()
    }

    fn is_lent(&self, surface: usize) -> bool {
        self.lent.get(surface).copied().unwrap_or(false)
    }

    /// What `slot` is decoded into next: its own surface unless a holder
    /// keeps it, else one nothing holds or decodes into, made where none
    /// is. `None` past the spares, which holders within their bound never
    /// reach: a slot's surface is lent, so at most `SPARES - 1` others are,
    /// and each spare made stands in for one.
    fn cover(&self, slot: usize) -> Option<Cover> {
        let own = self.surface(slot)?;
        if !self.is_lent(own) {
            return Some(Cover::Keep);
        }
        if let Some(free) = (0..self.made).find(|&s| !self.is_lent(s) && !self.map.contains(&s)) {
            return Some(Cover::Spare(free));
        }
        (self.made < POOL).then_some(Cover::Make(self.made))
    }

    /// `slot` decodes into `surface` from now on.
    fn assign(&mut self, slot: usize, surface: usize) {
        if let Some(s) = self.map.get_mut(slot) {
            *s = surface;
        }
        self.made = self.made.max(surface + 1);
    }

    /// A holder keeps `surface` from now on.
    fn lend(&mut self, surface: usize) -> Option<Lease> {
        let buffer = u32::try_from(surface).ok()?;
        *self.lent.get_mut(surface)? = true;
        Some(Lease {
            context: self.context,
            buffer,
        })
    }

    /// A holder let go: the surface may be decoded into again. A lease of
    /// another context frees nothing.
    fn end(&mut self, lease: Lease) {
        if self.context == 0 || lease.context != self.context {
            return;
        }
        if let Some(lent) = usize::try_from(lease.buffer)
            .ok()
            .and_then(|b| self.lent.get_mut(b))
        {
            *lent = false;
        }
    }
}

/// The staged buffers for one picture.
struct Staging {
    h264_picture: VAPictureParameterBufferH264,
    h264_matrix: VAIQMatrixBufferH264,
    h264_slices: Box<[VASliceParameterBufferH264; h264::MAX_SLICES]>,
    /// The base parameters lead the extension structure, so one storage
    /// serves both: a base-sized buffer for a stream at Main or Main 10,
    /// the whole for one at a range-extension profile.
    hevc_picture: VAPictureParameterBufferHEVCExtension,
    hevc_matrix: VAIQMatrixBufferHEVC,
    hevc_slices: Box<[VASliceParameterBufferHEVCExtension; hevc::MAX_SLICES]>,
    ids: [VABufferID; BUFFERS],
    count: usize,
}

/// The decoder over one display.
pub struct Backend<'a> {
    display: &'a Display<'a>,
    /// The largest coded picture the caller's planes take.
    ceiling: (u32, u32),
    codec: Codec,
    /// The stream's shape: the declaration's until the first parameter
    /// set, then that set's.
    shape: Shape,
    /// The surface layout asked for, or zero for the driver's own choice.
    fourcc: u32,
    config: Option<VAConfigID>,
    context: Option<VAContextID>,
    /// The context's surfaces, then any spares, as many as `pool` says.
    surfaces: [VASurfaceID; POOL],
    pool: Pool,
    /// Each surface's export, made the first time it is lent and kept with
    /// the context, with the number its first holder gave it.
    exports: [Option<(va::Export, u32)>; POOL],
    /// Whether this context's pictures can be lent: its surfaces export in
    /// the layout handed out, which full chroma's packed ones do not.
    lends: bool,
    coded_width: u32,
    coded_height: u32,
    /// The slot of the last picture taken, for the mapping hook.
    last_taken: Option<usize>,
    h264: Box<h264::Stream>,
    hevc: Box<hevc::Stream>,
    staging: Box<Staging>,
    /// The last decode and read-back, in microseconds, for the log.
    pub decode_us: u32,
    pub readback_us: u32,
}

impl core::fmt::Debug for Backend<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Backend")
            .field("codec", &self.codec)
            .field("shape", &self.shape)
            .field("context", &self.context)
            .field("coded", &(self.coded_width, self.coded_height))
            .finish()
    }
}

// SAFETY: every field is plain data; the device's structures are filled
// whole before use, so zero is a valid starting state.
fn zeroed<T>() -> T {
    unsafe { core::mem::zeroed() }
}

impl<'a> Backend<'a> {
    pub fn new(display: &'a Display<'a>, ceiling: (u32, u32)) -> Self {
        Self {
            display,
            ceiling,
            codec: Codec::H264,
            shape: Shape {
                ten_bit: false,
                full_chroma: false,
            },
            fourcc: 0,
            config: None,
            context: None,
            surfaces: [VA_INVALID_SURFACE; POOL],
            pool: Pool::fresh(0, 0),
            exports: [const { None }; POOL],
            lends: false,
            coded_width: 0,
            coded_height: 0,
            last_taken: None,
            h264: Box::new(h264::Stream::new()),
            hevc: Box::new(hevc::Stream::new()),
            staging: Box::new(Staging {
                h264_picture: zeroed(),
                h264_matrix: zeroed(),
                h264_slices: Box::new([zeroed(); h264::MAX_SLICES]),
                hevc_picture: zeroed(),
                hevc_matrix: zeroed(),
                hevc_slices: Box::new([zeroed(); hevc::MAX_SLICES]),
                ids: [VA_INVALID_ID; BUFFERS],
                count: 0,
            }),
            decode_us: 0,
            readback_us: 0,
        }
    }

    fn va(&self) -> &'a Vaapi {
        self.display.va()
    }

    fn check(&self, status: i32) -> Result<()> {
        self.va().check(status).map_err(|_| Error::Status(status))
    }

    /// Let every waiting picture out, as at the end of a stream; a test's
    /// need, since a live stream never ends this way.
    pub fn drain(&mut self) {
        self.h264.drain();
        self.hevc.drain();
    }

    /// Map the surface of the last picture taken and hand `f` the mapping,
    /// for a measurement of the copy out of it; a test's need, since a
    /// stream's pictures leave through [`Decoder::take`]. Valid until the
    /// next unit is fed, which may reuse the surface.
    pub fn with_taken_mapped<R>(&mut self, f: impl FnOnce(&VAImage, *const u8) -> R) -> Result<R> {
        let Some(slot) = self.last_taken else {
            return Err(Error::Status(-1));
        };
        let surface = self.surface(slot);
        let mut image: VAImage = zeroed();
        // SAFETY: the image is a live local the driver fills.
        let status =
            unsafe { (self.va().derive_image)(self.display.raw(), surface, &raw mut image) };
        self.check(status)?;
        let mut mapped: *mut c_void = core::ptr::null_mut();
        // SAFETY: the image's buffer is the driver's; the mapping lives until
        // the unmap below.
        let status =
            unsafe { (self.va().map_buffer)(self.display.raw(), image.buf, &raw mut mapped) };
        if let Err(e) = self.check(status) {
            // SAFETY: derived above, destroyed once.
            unsafe { (self.va().destroy_image)(self.display.raw(), image.image_id) };
            return Err(e);
        }
        let result = f(&image, mapped.cast_const().cast::<u8>());
        // SAFETY: mapped above; unmapped once, then the derived image is
        // destroyed once.
        unsafe {
            (self.va().unmap_buffer)(self.display.raw(), image.buf);
            (self.va().destroy_image)(self.display.raw(), image.image_id);
        }
        Ok(result)
    }

    /// The layout pictures come back in.
    pub fn format(&self) -> Format {
        self.shape.format()
    }

    /// The size and layout the pictures [`Decoder::take`] hands out have,
    /// once the stream has said: the active parameter set's visible size.
    pub fn output(&self) -> Option<(u32, u32, Format)> {
        let (width, height) = match self.codec {
            Codec::H264 => self.h264.active_sps()?.visible(),
            Codec::H265 => self.hevc.active_sps()?.visible(),
        };
        (width > 0 && height > 0).then_some((width, height, self.format()))
    }

    /// The configuration for the stream's shape, and the surface layout to
    /// ask for with it: for full chroma the first the driver offers that
    /// this backend reads, or no configuration at all.
    fn create_config(&mut self) -> Result<()> {
        let profile = self.shape.profile(self.codec);
        self.fourcc = if self.shape.full_chroma {
            full_chroma_fourcc(self.display, self.shape).ok_or(Error::NoProfile)?
        } else {
            0
        };
        let mut attrib = VAConfigAttrib {
            type_: VAConfigAttribRTFormat,
            value: self.shape.rt_format(),
        };
        let mut config: VAConfigID = 0;
        // SAFETY: the attribute is a live local, and the output too.
        let status = unsafe {
            (self.va().create_config)(
                self.display.raw(),
                profile,
                VAEntrypointVLD,
                &raw mut attrib,
                1,
                &raw mut config,
            )
        };
        if status != 0 {
            return Err(Error::NoProfile);
        }
        self.config = Some(config);
        Ok(())
    }

    /// `count` surfaces at `width` x `height` from index `from` on, in the
    /// layout the configuration asked for.
    fn make_surfaces(&mut self, from: usize, count: usize, width: u32, height: u32) -> Result<()> {
        let va = self.va();
        let raw = self.display.raw();
        let rt_format = self.shape.rt_format();
        // The layout is named where one was chosen, so the read-back knows
        // what it will map; otherwise the driver's own choice.
        let mut attrib = VASurfaceAttrib {
            type_: VASurfaceAttribPixelFormat,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue {
                type_: VAGenericValueTypeInteger,
                value: lowlat_drivers::ffi::va::_VAGenericValue__bindgen_ty_1 {
                    // The four-character code's bits, whatever the sign.
                    i: i32::from_ne_bytes(self.fourcc.to_ne_bytes()),
                },
            },
        };
        let (attribs, named): (*mut VASurfaceAttrib, c_uint) = if self.fourcc == 0 {
            (core::ptr::null_mut(), 0)
        } else {
            (&raw mut attrib, 1)
        };
        let out = self
            .surfaces
            .get_mut(from..from.saturating_add(count))
            .ok_or(Error::TooLarge)?;
        // SAFETY: the slice is writable for the count passed; the attribute
        // is a live local for the call.
        let status = unsafe {
            (va.create_surfaces)(
                raw,
                rt_format,
                width,
                height,
                out.as_mut_ptr(),
                c_uint::try_from(count).unwrap_or(0),
                attribs.cast(),
                named,
            )
        };
        self.check(status)
    }

    /// The surfaces and the context, at the stream's coded size.
    fn create_context(&mut self, width: u32, height: u32) -> Result<()> {
        let Some(config) = self.config else {
            return Err(Error::NoProfile);
        };
        if width > self.ceiling.0 || height > self.ceiling.1 {
            return Err(Error::TooLarge);
        }
        self.make_surfaces(0, SURFACES, width, height)?;
        let mut context: VAContextID = 0;
        // SAFETY: the surfaces were just created and the output is live.
        let status = unsafe {
            (self.va().create_context)(
                self.display.raw(),
                config,
                c_int::try_from(width).unwrap_or(0),
                c_int::try_from(height).unwrap_or(0),
                0,
                self.surfaces.as_mut_ptr(),
                c_int::try_from(SURFACES).unwrap_or(0),
                &raw mut context,
            )
        };
        if let Err(e) = self.check(status) {
            self.pool = Pool::fresh(SURFACES, 0);
            self.destroy_surfaces();
            return Err(e);
        }
        self.context = Some(context);
        self.coded_width = width;
        self.coded_height = height;
        // A number of its own, so a lease of an earlier context frees
        // nothing of this one's. Relaxed: the count only has to be unique,
        // and nothing else is published through it.
        let number = CONTEXTS
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);
        self.pool = Pool::fresh(SURFACES, number);
        // Lent only where the surfaces export in the layout handed out,
        // asked of the first, whose export is kept for its first lending.
        self.lends = false;
        if !self.shape.full_chroma {
            let first = self.surface(0);
            if let (Ok(export), Some(entry)) =
                (self.display.export(first), self.exports.first_mut())
            {
                *entry = Some((export, 0));
                self.lends = true;
            }
        }
        Ok(())
    }

    fn destroy_surfaces(&mut self) {
        let made = self.pool.made.min(POOL);
        if made > 0 {
            // SAFETY: the surfaces were created by this backend and are
            // destroyed together, once.
            unsafe {
                (self.va().destroy_surfaces)(
                    self.display.raw(),
                    self.surfaces.as_mut_ptr(),
                    c_int::try_from(made).unwrap_or(0),
                );
            }
        }
        self.surfaces = [VA_INVALID_SURFACE; POOL];
        // The context's own descriptors close here; a holder's duplicate
        // keeps its buffer alive until the holder lets it go.
        self.exports = [const { None }; POOL];
        self.pool = Pool::fresh(0, 0);
        self.lends = false;
    }

    fn destroy_context(&mut self) {
        if let Some(context) = self.context.take() {
            // SAFETY: created by this backend, destroyed once.
            unsafe { (self.va().destroy_context)(self.display.raw(), context) };
            self.destroy_surfaces();
        }
        self.coded_width = 0;
        self.coded_height = 0;
    }

    fn destroy_config(&mut self) {
        if let Some(config) = self.config.take() {
            // SAFETY: created by this backend, destroyed once.
            unsafe { (self.va().destroy_config)(self.display.raw(), config) };
        }
    }

    /// The surface `slot` decodes into now.
    fn surface(&self, slot: usize) -> VASurfaceID {
        self.pool
            .surface(slot)
            .and_then(|i| self.surfaces.get(i).copied())
            .unwrap_or(VA_INVALID_SURFACE)
    }

    /// Give `slot` a surface no holder keeps before a picture is decoded
    /// into it: its own, or a spare in its place while a holder keeps that.
    fn cover(&mut self, slot: usize) -> Result<()> {
        match self.pool.cover(slot) {
            Some(Cover::Keep) => Ok(()),
            Some(Cover::Spare(surface)) => {
                self.pool.assign(slot, surface);
                Ok(())
            }
            Some(Cover::Make(surface)) => {
                self.make_surfaces(surface, 1, self.coded_width, self.coded_height)?;
                self.pool.assign(slot, surface);
                lowlat_common::log_info!(
                    "decode: spare surface made for a lent one, surfaces={}",
                    self.pool.made
                );
                Ok(())
            }
            // More lent at once than the spares cover.
            None => Err(Error::Status(-1)),
        }
    }

    fn buffer(
        &mut self,
        kind: lowlat_drivers::ffi::va::VABufferType,
        data: *mut c_void,
        size: usize,
    ) -> Result<()> {
        let Some(context) = self.context else {
            return Err(Error::NoProfile);
        };
        let mut id: VABufferID = 0;
        // SAFETY: `data` points at `size` readable bytes the caller owns for
        // the call; the driver copies them.
        let status = unsafe {
            (self.va().create_buffer)(
                self.display.raw(),
                context,
                kind,
                c_uint::try_from(size).unwrap_or(0),
                1,
                data,
                &raw mut id,
            )
        };
        self.check(status)?;
        let slot = self
            .staging
            .ids
            .get_mut(self.staging.count)
            .ok_or(Error::TooLarge)?;
        *slot = id;
        self.staging.count += 1;
        Ok(())
    }

    fn destroy_buffers(&mut self) {
        for i in 0..self.staging.count {
            if let Some(&id) = self.staging.ids.get(i) {
                // SAFETY: created above, destroyed once.
                unsafe { (self.va().destroy_buffer)(self.display.raw(), id) };
            }
        }
        self.staging.count = 0;
    }

    /// Submit the staged buffers for the picture on `slot`.
    fn submit(&mut self, slot: usize) -> Result<()> {
        let Some(context) = self.context else {
            return Err(Error::NoProfile);
        };
        let surface = self.surface(slot);
        // SAFETY: the context and surface are this backend's.
        let status = unsafe { (self.va().begin_picture)(self.display.raw(), context, surface) };
        self.check(status)?;
        // SAFETY: the ids are live buffers, `count` of them.
        let status = unsafe {
            (self.va().render_picture)(
                self.display.raw(),
                context,
                self.staging.ids.as_mut_ptr(),
                c_int::try_from(self.staging.count).unwrap_or(0),
            )
        };
        self.check(status)?;
        // SAFETY: as above.
        let status = unsafe { (self.va().end_picture)(self.display.raw(), context) };
        self.check(status)
    }

    fn decode_h264(&mut self, unit: &[u8]) -> Result<Fed> {
        match self.h264.read(unit).map_err(|_| Error::Status(-1))? {
            h264::Read::Nothing => return Ok(self.pending_fed_h264()),
            h264::Read::FormatChanged => return Ok(Fed::FormatChanged),
            h264::Read::Picture => {}
        }
        let job = self.h264.job().ok_or(Error::Status(-1))?;
        let (width, height) = (job.sps.coded_width(), job.sps.coded_height());
        if self.context.is_none() {
            self.create_context(width, height)?;
        } else if width != self.coded_width || height != self.coded_height {
            self.h264.abandon();
            return Ok(Fed::FormatChanged);
        }
        let slot = self
            .h264
            .job()
            .map(|j| j.current.slot)
            .ok_or(Error::Status(-1))?;
        if let Err(e) = self.cover(slot) {
            self.h264.abandon();
            return Err(e);
        }
        self.stage_h264(unit)?;
        let submitted = self.submit(slot);
        self.destroy_buffers();
        if let Err(e) = submitted {
            self.h264.abandon();
            return Err(e);
        }
        self.h264.finish().map_err(|_| Error::Status(-1))?;
        Ok(self.pending_fed_h264())
    }

    fn pending_fed_h264(&self) -> Fed {
        if self.h264.dpb.has_output() {
            Fed::Picture
        } else {
            Fed::NeedMoreData
        }
    }

    fn stage_h264(&mut self, unit: &[u8]) -> Result<()> {
        let (surfaces, pool) = (self.surfaces, self.pool);
        let surface_of = |slot: usize| {
            pool.surface(slot)
                .and_then(|i| surfaces.get(i).copied())
                .unwrap_or(VA_INVALID_SURFACE)
        };
        let job = self.h264.job().ok_or(Error::Status(-1))?;
        let sps = &job.sps;
        let pps = &job.pps;
        let current = &job.current;

        let picture = &mut self.staging.h264_picture;
        *picture = zeroed();
        picture.CurrPic = va_picture_h264_current(current, surface_of(current.slot));
        for (i, slot) in picture.ReferenceFrames.iter_mut().enumerate() {
            *slot = match job.references.as_slice().get(i).copied().flatten() {
                Some(r) => va_picture_h264(&r, surface_of(r.slot)),
                None => va_picture_h264_invalid(),
            };
        }
        picture.picture_width_in_mbs_minus1 = sps.pic_width_in_mbs_minus1;
        picture.picture_height_in_mbs_minus1 =
            u16::try_from(sps.frame_height_in_mbs().saturating_sub(1)).unwrap_or(0);
        picture.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
        picture.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
        picture.num_ref_frames = sps.max_num_ref_frames;
        // SAFETY: the bitfield view of a zeroed word.
        let seq = unsafe { &mut picture.seq_fields.bits };
        seq.set_chroma_format_idc(u32::from(sps.chroma_format_idc));
        seq.set_residual_colour_transform_flag(u32::from(sps.separate_colour_plane));
        seq.set_gaps_in_frame_num_value_allowed_flag(u32::from(sps.gaps_in_frame_num_allowed));
        seq.set_frame_mbs_only_flag(u32::from(sps.frame_mbs_only));
        seq.set_mb_adaptive_frame_field_flag(u32::from(sps.mb_adaptive_frame_field));
        seq.set_direct_8x8_inference_flag(u32::from(sps.direct_8x8_inference));
        seq.set_MinLumaBiPredSize8x8(u32::from(sps.level_idc >= 31));
        seq.set_log2_max_frame_num_minus4(u32::from(sps.log2_max_frame_num_minus4));
        seq.set_pic_order_cnt_type(u32::from(sps.pic_order_cnt_type));
        seq.set_log2_max_pic_order_cnt_lsb_minus4(u32::from(sps.log2_max_pic_order_cnt_lsb_minus4));
        seq.set_delta_pic_order_always_zero_flag(u32::from(sps.delta_pic_order_always_zero));
        picture.num_slice_groups_minus1 = 0;
        picture.slice_group_map_type = 0;
        picture.slice_group_change_rate_minus1 = 0;
        picture.pic_init_qp_minus26 = pps.pic_init_qp_minus26;
        picture.pic_init_qs_minus26 = pps.pic_init_qs_minus26;
        picture.chroma_qp_index_offset = pps.chroma_qp_index_offset;
        picture.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset;
        let first = job
            .slices
            .items
            .first()
            .copied()
            .flatten()
            .ok_or(Error::Status(-1))?;
        // SAFETY: as above.
        let pic = unsafe { &mut picture.pic_fields.bits };
        pic.set_entropy_coding_mode_flag(u32::from(pps.entropy_coding_mode));
        pic.set_weighted_pred_flag(u32::from(pps.weighted_pred));
        pic.set_weighted_bipred_idc(u32::from(pps.weighted_bipred_idc));
        pic.set_transform_8x8_mode_flag(u32::from(pps.transform_8x8_mode));
        pic.set_field_pic_flag(u32::from(first.header.field_pic));
        pic.set_constrained_intra_pred_flag(u32::from(pps.constrained_intra_pred));
        pic.set_pic_order_present_flag(u32::from(pps.bottom_field_pic_order_in_frame_present));
        pic.set_deblocking_filter_control_present_flag(u32::from(
            pps.deblocking_filter_control_present,
        ));
        pic.set_redundant_pic_cnt_present_flag(u32::from(pps.redundant_pic_cnt_present));
        pic.set_reference_pic_flag(u32::from(first.header.nal_ref_idc != 0));
        picture.frame_num = u16::try_from(current.frame_num).unwrap_or(0);

        let matrix = &mut self.staging.h264_matrix;
        matrix.ScalingList4x4 = pps.scaling.list_4x4;
        matrix.ScalingList8x8 = pps.scaling.list_8x8;

        let slices = job.slices.len;
        for (i, slice) in job.slices.as_slice().iter().flatten().enumerate() {
            let Some(out) = self.staging.h264_slices.get_mut(i) else {
                break;
            };
            *out = zeroed();
            let h = &slice.header;
            out.slice_data_size = u32::try_from(slice.len).unwrap_or(0);
            out.slice_data_offset = 0;
            out.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
            out.slice_data_bit_offset = u16::try_from(8 + h.header_bits).unwrap_or(u16::MAX);
            out.first_mb_in_slice = u16::try_from(h.first_mb_in_slice).unwrap_or(u16::MAX);
            out.slice_type = h.slice_type.code();
            out.direct_spatial_mv_pred_flag = u8::from(h.direct_spatial_mv_pred);
            out.num_ref_idx_l0_active_minus1 = if h.slice_type.is_intra() {
                0
            } else {
                h.num_ref_idx_l0_active_minus1
            };
            out.num_ref_idx_l1_active_minus1 = if h.slice_type.is_b() {
                h.num_ref_idx_l1_active_minus1
            } else {
                0
            };
            out.cabac_init_idc = h.cabac_init_idc;
            out.slice_qp_delta = h.slice_qp_delta;
            out.disable_deblocking_filter_idc = h.disable_deblocking_filter_idc;
            out.slice_alpha_c0_offset_div2 = h.slice_alpha_c0_offset_div2;
            out.slice_beta_offset_div2 = h.slice_beta_offset_div2;
            for (n, entry) in out.RefPicList0.iter_mut().enumerate() {
                *entry = match slice.l0.as_slice().get(n).copied().flatten() {
                    Some(r) => va_picture_h264(&r, surface_of(r.slot)),
                    None => va_picture_h264_invalid(),
                };
            }
            for (n, entry) in out.RefPicList1.iter_mut().enumerate() {
                *entry = match slice.l1.as_slice().get(n).copied().flatten() {
                    Some(r) => va_picture_h264(&r, surface_of(r.slot)),
                    None => va_picture_h264_invalid(),
                };
            }
            if let Some(w) = &h.weights {
                out.luma_log2_weight_denom = w.luma_log2_denom;
                out.chroma_log2_weight_denom = w.chroma_log2_denom;
                out.luma_weight_l0_flag = u8::from(w.l0.luma_flag);
                out.luma_weight_l0 = w.l0.luma_weight;
                out.luma_offset_l0 = w.l0.luma_offset;
                out.chroma_weight_l0_flag = u8::from(w.l0.chroma_flag);
                out.chroma_weight_l0 = w.l0.chroma_weight;
                out.chroma_offset_l0 = w.l0.chroma_offset;
                out.luma_weight_l1_flag = u8::from(w.l1.luma_flag);
                out.luma_weight_l1 = w.l1.luma_weight;
                out.luma_offset_l1 = w.l1.luma_offset;
                out.chroma_weight_l1_flag = u8::from(w.l1.chroma_flag);
                out.chroma_weight_l1 = w.l1.chroma_weight;
                out.chroma_offset_l1 = w.l1.chroma_offset;
            }
        }

        // Hand everything over. The slice data is the unit's own bytes.
        self.staging.count = 0;
        let picture_ptr: *mut c_void = (&raw mut self.staging.h264_picture).cast();
        self.buffer(
            VAPictureParameterBufferType,
            picture_ptr,
            size_of::<VAPictureParameterBufferH264>(),
        )?;
        let matrix_ptr: *mut c_void = (&raw mut self.staging.h264_matrix).cast();
        self.buffer(
            VAIQMatrixBufferType,
            matrix_ptr,
            size_of::<VAIQMatrixBufferH264>(),
        )?;
        for i in 0..slices {
            let (offset, len) = self
                .h264
                .job()
                .and_then(|j| j.slices.as_slice().get(i).copied().flatten())
                .map(|s| (s.offset, s.len))
                .ok_or(Error::Status(-1))?;
            let param_ptr: *mut c_void = self
                .staging
                .h264_slices
                .get_mut(i)
                .map(|s| (s as *mut VASliceParameterBufferH264).cast())
                .ok_or(Error::TooLarge)?;
            self.buffer(
                VASliceParameterBufferType,
                param_ptr,
                size_of::<VASliceParameterBufferH264>(),
            )?;
            let data = unit.get(offset..offset + len).ok_or(Error::Status(-1))?;
            self.buffer(
                VASliceDataBufferType,
                data.as_ptr().cast_mut().cast(),
                data.len(),
            )?;
        }
        Ok(())
    }

    fn decode_hevc(&mut self, unit: &[u8]) -> Result<Fed> {
        match self.hevc.read(unit).map_err(|_| Error::Status(-1))? {
            hevc::Read::Nothing => return Ok(self.pending_fed_hevc()),
            hevc::Read::FormatChanged => return Ok(Fed::FormatChanged),
            hevc::Read::Picture => {}
        }
        let job = self.hevc.job().ok_or(Error::Status(-1))?;
        let (width, height) = (job.sps.width, job.sps.height);
        // The parameter set says what the stream is. Full chroma at eight
        // or ten bits is staged through the range-extension structures and
        // decoded into a layout the read-back knows; anything else the
        // range extensions allow -- half chroma, or the extension tools on
        // a 4:2:0 stream -- has no profile here, and the base structures
        // would decode such a stream wrongly without an error, so it is
        // refused outright.
        let shape = Shape {
            ten_bit: job.sps.bit_depth_luma_minus8 > 0,
            full_chroma: job.sps.chroma_format_idc == 3,
        };
        if job.sps.chroma_format_idc == 2 || (!shape.full_chroma && job.sps.is_range_extended()) {
            self.hevc.abandon();
            return Err(Error::NoProfile);
        }
        if self.context.is_none() {
            if shape != self.shape {
                // The declaration guessed the shape; the stream corrects it
                // before anything is built on the guess.
                self.destroy_config();
                self.shape = shape;
                self.create_config()?;
            }
            self.create_context(width, height)?;
        } else if width != self.coded_width || height != self.coded_height || shape != self.shape {
            self.hevc.abandon();
            return Ok(Fed::FormatChanged);
        }
        let slot = self
            .hevc
            .job()
            .map(|j| j.current.slot)
            .ok_or(Error::Status(-1))?;
        if let Err(e) = self.cover(slot) {
            self.hevc.abandon();
            return Err(e);
        }
        self.stage_hevc(unit)?;
        let submitted = self.submit(slot);
        self.destroy_buffers();
        if let Err(e) = submitted {
            self.hevc.abandon();
            return Err(e);
        }
        self.hevc.finish().map_err(|_| Error::Status(-1))?;
        Ok(self.pending_fed_hevc())
    }

    fn pending_fed_hevc(&self) -> Fed {
        if self.hevc.dpb.has_output() {
            Fed::Picture
        } else {
            Fed::NeedMoreData
        }
    }

    fn stage_hevc(&mut self, unit: &[u8]) -> Result<()> {
        let (surfaces, pool) = (self.surfaces, self.pool);
        let surface_of = |slot: usize| {
            pool.surface(slot)
                .and_then(|i| surfaces.get(i).copied())
                .unwrap_or(VA_INVALID_SURFACE)
        };
        let job = self.hevc.job().ok_or(Error::Status(-1))?;
        let sps = &job.sps;
        let pps = &job.pps;
        let current = &job.current;
        let first = job
            .slices
            .items
            .first()
            .copied()
            .flatten()
            .ok_or(Error::Status(-1))?;

        let extension = &mut self.staging.hevc_picture;
        *extension = zeroed();
        let picture = &mut extension.base;
        picture.CurrPic = VAPictureHEVC {
            picture_id: surface_of(current.slot),
            pic_order_cnt: current.poc,
            flags: 0,
            va_reserved: [0; 4],
        };
        for (i, slot) in picture.ReferenceFrames.iter_mut().enumerate() {
            *slot = match job.references.as_slice().get(i).copied().flatten() {
                Some(r) => {
                    let mut flags = 0;
                    if r.long_term {
                        flags |= VA_PICTURE_HEVC_LONG_TERM_REFERENCE;
                    }
                    flags |= match r.set {
                        hevc::dpb::Set::StCurrBefore => VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
                        hevc::dpb::Set::StCurrAfter => VA_PICTURE_HEVC_RPS_ST_CURR_AFTER,
                        hevc::dpb::Set::LtCurr => VA_PICTURE_HEVC_RPS_LT_CURR,
                        hevc::dpb::Set::Foll => 0,
                    };
                    VAPictureHEVC {
                        picture_id: surface_of(r.slot),
                        pic_order_cnt: r.poc,
                        flags,
                        va_reserved: [0; 4],
                    }
                }
                None => VAPictureHEVC {
                    picture_id: VA_INVALID_SURFACE,
                    pic_order_cnt: 0,
                    flags: VA_PICTURE_HEVC_INVALID,
                    va_reserved: [0; 4],
                },
            };
        }
        picture.pic_width_in_luma_samples = u16::try_from(sps.width).unwrap_or(u16::MAX);
        picture.pic_height_in_luma_samples = u16::try_from(sps.height).unwrap_or(u16::MAX);
        let no_bipred = job
            .slices
            .as_slice()
            .iter()
            .flatten()
            .all(|s| !s.header.slice_type.is_b());
        let intra_only = job
            .slices
            .as_slice()
            .iter()
            .flatten()
            .all(|s| s.header.slice_type.is_intra());
        // SAFETY: the bitfield view of a zeroed word.
        let pf = unsafe { &mut picture.pic_fields.bits };
        pf.set_chroma_format_idc(u32::from(sps.chroma_format_idc));
        pf.set_separate_colour_plane_flag(u32::from(sps.separate_colour_plane));
        pf.set_pcm_enabled_flag(u32::from(sps.pcm_enabled));
        pf.set_scaling_list_enabled_flag(u32::from(sps.scaling_list_enabled));
        pf.set_transform_skip_enabled_flag(u32::from(pps.transform_skip_enabled));
        pf.set_amp_enabled_flag(u32::from(sps.amp_enabled));
        pf.set_strong_intra_smoothing_enabled_flag(u32::from(sps.strong_intra_smoothing_enabled));
        pf.set_sign_data_hiding_enabled_flag(u32::from(pps.sign_data_hiding_enabled));
        pf.set_constrained_intra_pred_flag(u32::from(pps.constrained_intra_pred));
        pf.set_cu_qp_delta_enabled_flag(u32::from(pps.cu_qp_delta_enabled));
        pf.set_weighted_pred_flag(u32::from(pps.weighted_pred));
        pf.set_weighted_bipred_flag(u32::from(pps.weighted_bipred));
        pf.set_transquant_bypass_enabled_flag(u32::from(pps.transquant_bypass_enabled));
        pf.set_tiles_enabled_flag(u32::from(pps.tiles_enabled));
        pf.set_entropy_coding_sync_enabled_flag(u32::from(pps.entropy_coding_sync_enabled));
        pf.set_pps_loop_filter_across_slices_enabled_flag(u32::from(
            pps.loop_filter_across_slices_enabled,
        ));
        pf.set_loop_filter_across_tiles_enabled_flag(u32::from(
            pps.loop_filter_across_tiles_enabled,
        ));
        pf.set_pcm_loop_filter_disabled_flag(u32::from(sps.pcm_loop_filter_disabled));
        pf.set_NoPicReorderingFlag(u32::from(sps.max_num_reorder_pics == 0));
        pf.set_NoBiPredFlag(u32::from(no_bipred));
        picture.sps_max_dec_pic_buffering_minus1 = sps.max_dec_pic_buffering_minus1;
        picture.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
        picture.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
        picture.pcm_sample_bit_depth_luma_minus1 = sps.pcm_sample_bit_depth_luma_minus1;
        picture.pcm_sample_bit_depth_chroma_minus1 = sps.pcm_sample_bit_depth_chroma_minus1;
        picture.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3;
        picture.log2_diff_max_min_luma_coding_block_size =
            sps.log2_diff_max_min_luma_coding_block_size;
        picture.log2_min_transform_block_size_minus2 =
            sps.log2_min_luma_transform_block_size_minus2;
        picture.log2_diff_max_min_transform_block_size =
            sps.log2_diff_max_min_luma_transform_block_size;
        picture.log2_min_pcm_luma_coding_block_size_minus3 =
            sps.log2_min_pcm_luma_coding_block_size_minus3;
        picture.log2_diff_max_min_pcm_luma_coding_block_size =
            sps.log2_diff_max_min_pcm_luma_coding_block_size;
        picture.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
        picture.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
        picture.init_qp_minus26 = pps.init_qp_minus26;
        picture.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth;
        picture.pps_cb_qp_offset = pps.cb_qp_offset;
        picture.pps_cr_qp_offset = pps.cr_qp_offset;
        picture.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2;
        picture.num_tile_columns_minus1 = pps.num_tile_columns_minus1;
        picture.num_tile_rows_minus1 = pps.num_tile_rows_minus1;
        let (ctb_w, ctb_h) = sps.ctbs();
        let columns = u32::from(pps.num_tile_columns_minus1) + 1;
        let rows = u32::from(pps.num_tile_rows_minus1) + 1;
        for (i, w) in picture.column_width_minus1.iter_mut().enumerate() {
            let i32 = u32::try_from(i).unwrap_or(0);
            *w = if pps.uniform_spacing {
                if i32 < columns {
                    u16::try_from(
                        ((i32 + 1) * ctb_w / columns - i32 * ctb_w / columns).saturating_sub(1),
                    )
                    .unwrap_or(0)
                } else {
                    0
                }
            } else {
                pps.column_width_minus1.get(i).copied().unwrap_or(0)
            };
        }
        for (i, h) in picture.row_height_minus1.iter_mut().enumerate() {
            let i32 = u32::try_from(i).unwrap_or(0);
            *h = if pps.uniform_spacing {
                if i32 < rows {
                    u16::try_from(((i32 + 1) * ctb_h / rows - i32 * ctb_h / rows).saturating_sub(1))
                        .unwrap_or(0)
                } else {
                    0
                }
            } else {
                pps.row_height_minus1.get(i).copied().unwrap_or(0)
            };
        }
        // SAFETY: as above.
        let sf = unsafe { &mut picture.slice_parsing_fields.bits };
        sf.set_lists_modification_present_flag(u32::from(pps.lists_modification_present));
        sf.set_long_term_ref_pics_present_flag(u32::from(sps.long_term_ref_pics_present));
        sf.set_sps_temporal_mvp_enabled_flag(u32::from(sps.temporal_mvp_enabled));
        sf.set_cabac_init_present_flag(u32::from(pps.cabac_init_present));
        sf.set_output_flag_present_flag(u32::from(pps.output_flag_present));
        sf.set_dependent_slice_segments_enabled_flag(u32::from(
            pps.dependent_slice_segments_enabled,
        ));
        sf.set_pps_slice_chroma_qp_offsets_present_flag(u32::from(
            pps.slice_chroma_qp_offsets_present,
        ));
        sf.set_sample_adaptive_offset_enabled_flag(u32::from(sps.sample_adaptive_offset_enabled));
        sf.set_deblocking_filter_override_enabled_flag(u32::from(
            pps.deblocking_filter_override_enabled,
        ));
        sf.set_pps_disable_deblocking_filter_flag(u32::from(pps.disable_deblocking_filter));
        sf.set_slice_segment_header_extension_present_flag(u32::from(
            pps.slice_segment_header_extension_present,
        ));
        sf.set_RapPicFlag(u32::from(first.header.is_irap()));
        sf.set_IdrPicFlag(u32::from(first.header.is_idr()));
        sf.set_IntraPicFlag(u32::from(intra_only));
        picture.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
        picture.num_short_term_ref_pic_sets = sps.num_short_term_ref_pic_sets;
        picture.num_long_term_ref_pic_sps = sps.num_long_term_ref_pics_sps;
        picture.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        picture.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        picture.pps_beta_offset_div2 = pps.beta_offset_div2;
        picture.pps_tc_offset_div2 = pps.tc_offset_div2;
        picture.num_extra_slice_header_bits = pps.num_extra_slice_header_bits;
        picture.st_rps_bits = if first.header.st_rps_from_sps {
            0
        } else {
            first.header.st_rps_bits
        };
        let full_chroma = self.shape.full_chroma;
        if full_chroma {
            // The range-extension picture fields, both parameter sets'.
            let rext = &mut extension.rext;
            // SAFETY: the bitfield view of a zeroed word.
            let rf = unsafe { &mut rext.range_extension_pic_fields.bits };
            let range = &sps.range;
            rf.set_transform_skip_rotation_enabled_flag(u32::from(
                range.transform_skip_rotation_enabled,
            ));
            rf.set_transform_skip_context_enabled_flag(u32::from(
                range.transform_skip_context_enabled,
            ));
            rf.set_implicit_rdpcm_enabled_flag(u32::from(range.implicit_rdpcm_enabled));
            rf.set_explicit_rdpcm_enabled_flag(u32::from(range.explicit_rdpcm_enabled));
            rf.set_extended_precision_processing_flag(u32::from(
                range.extended_precision_processing,
            ));
            rf.set_intra_smoothing_disabled_flag(u32::from(range.intra_smoothing_disabled));
            rf.set_high_precision_offsets_enabled_flag(u32::from(
                range.high_precision_offsets_enabled,
            ));
            rf.set_persistent_rice_adaptation_enabled_flag(u32::from(
                range.persistent_rice_adaptation_enabled,
            ));
            rf.set_cabac_bypass_alignment_enabled_flag(u32::from(
                range.cabac_bypass_alignment_enabled,
            ));
            let prange = &pps.range;
            rf.set_cross_component_prediction_enabled_flag(u32::from(
                prange.cross_component_prediction_enabled,
            ));
            rf.set_chroma_qp_offset_list_enabled_flag(u32::from(
                prange.chroma_qp_offset_list_enabled,
            ));
            rext.diff_cu_chroma_qp_offset_depth = prange.diff_cu_chroma_qp_offset_depth;
            rext.chroma_qp_offset_list_len_minus1 = prange.chroma_qp_offset_list_len_minus1;
            rext.log2_sao_offset_scale_luma = prange.log2_sao_offset_scale_luma;
            rext.log2_sao_offset_scale_chroma = prange.log2_sao_offset_scale_chroma;
            rext.log2_max_transform_skip_block_size_minus2 =
                prange.log2_max_transform_skip_block_size_minus2;
            rext.cb_qp_offset_list = prange.cb_qp_offset_list;
            rext.cr_qp_offset_list = prange.cr_qp_offset_list;
        }

        let matrix = &mut self.staging.hevc_matrix;
        matrix.ScalingList4x4 = pps.scaling.list_4x4;
        matrix.ScalingList8x8 = pps.scaling.list_8x8;
        matrix.ScalingList16x16 = pps.scaling.list_16x16;
        matrix.ScalingList32x32 = pps.scaling.list_32x32;
        matrix.ScalingListDC16x16 = pps.scaling.dc_16x16;
        matrix.ScalingListDC32x32 = pps.scaling.dc_32x32;

        let slices = job.slices.len;
        let scaling_enabled = sps.scaling_list_enabled;
        for (i, slice) in job.slices.as_slice().iter().flatten().enumerate() {
            let Some(ext) = self.staging.hevc_slices.get_mut(i) else {
                break;
            };
            *ext = zeroed();
            let out = &mut ext.base;
            let h = &slice.header;
            out.slice_data_size = u32::try_from(slice.len).unwrap_or(0);
            out.slice_data_offset = 0;
            out.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
            out.slice_data_byte_offset = h.header_bytes;
            out.slice_segment_address = h.slice_segment_address;
            for list in 0..2 {
                let source = if list == 0 { &slice.l0 } else { &slice.l1 };
                if let Some(entries) = out.RefPicList.get_mut(list) {
                    for (n, e) in entries.iter_mut().enumerate() {
                        *e = source.as_slice().get(n).copied().flatten().unwrap_or(0xFF);
                    }
                }
            }
            // SAFETY: the bitfield view of a zeroed word.
            let lf = unsafe { &mut out.LongSliceFlags.fields };
            lf.set_LastSliceOfPic(u32::from(i + 1 == slices));
            lf.set_dependent_slice_segment_flag(u32::from(h.dependent_slice_segment));
            lf.set_slice_type(u32::from(h.slice_type.code()));
            lf.set_color_plane_id(u32::from(h.colour_plane_id));
            lf.set_slice_sao_luma_flag(u32::from(h.sao_luma));
            lf.set_slice_sao_chroma_flag(u32::from(h.sao_chroma));
            lf.set_mvd_l1_zero_flag(u32::from(h.mvd_l1_zero));
            lf.set_cabac_init_flag(u32::from(h.cabac_init));
            lf.set_slice_temporal_mvp_enabled_flag(u32::from(h.temporal_mvp_enabled));
            lf.set_slice_deblocking_filter_disabled_flag(u32::from(h.deblocking_filter_disabled));
            lf.set_collocated_from_l0_flag(u32::from(h.collocated_from_l0));
            lf.set_slice_loop_filter_across_slices_enabled_flag(u32::from(
                h.loop_filter_across_slices_enabled,
            ));
            out.collocated_ref_idx = h.collocated_ref_idx;
            out.num_ref_idx_l0_active_minus1 = if h.slice_type.is_intra() {
                0
            } else {
                h.num_ref_idx_l0_active_minus1
            };
            out.num_ref_idx_l1_active_minus1 = if h.slice_type.is_b() {
                h.num_ref_idx_l1_active_minus1
            } else {
                0
            };
            out.slice_qp_delta = h.slice_qp_delta;
            out.slice_cb_qp_offset = h.slice_cb_qp_offset;
            out.slice_cr_qp_offset = h.slice_cr_qp_offset;
            out.slice_beta_offset_div2 = h.beta_offset_div2;
            out.slice_tc_offset_div2 = h.tc_offset_div2;
            if let Some(w) = &h.weights {
                out.luma_log2_weight_denom = w.luma_log2_denom;
                out.delta_chroma_log2_weight_denom = w.delta_chroma_log2_denom;
                // Eight bits in the base parameters, the range every stream
                // without the extensions stays within; the extension carries
                // the offsets whole for a stream that may exceed it.
                out.delta_luma_weight_l0 = w.l0.delta_luma_weight;
                out.luma_offset_l0 = w.l0.luma_offset.map(narrow);
                out.delta_chroma_weight_l0 = w.l0.delta_chroma_weight;
                out.ChromaOffsetL0 = w.l0.chroma_offset.map(|pair| pair.map(narrow));
                out.delta_luma_weight_l1 = w.l1.delta_luma_weight;
                out.luma_offset_l1 = w.l1.luma_offset.map(narrow);
                out.delta_chroma_weight_l1 = w.l1.delta_chroma_weight;
                out.ChromaOffsetL1 = w.l1.chroma_offset.map(|pair| pair.map(narrow));
                if full_chroma {
                    ext.rext.luma_offset_l0 = w.l0.luma_offset;
                    ext.rext.ChromaOffsetL0 = w.l0.chroma_offset;
                    ext.rext.luma_offset_l1 = w.l1.luma_offset;
                    ext.rext.ChromaOffsetL1 = w.l1.chroma_offset;
                }
            }
            out.five_minus_max_num_merge_cand = h.five_minus_max_num_merge_cand;
            out.num_entry_point_offsets =
                u16::try_from(h.num_entry_point_offsets).unwrap_or(u16::MAX);
            out.entry_offset_to_subset_array = 0;
            out.slice_data_num_emu_prevn_bytes =
                u16::try_from(h.header_escapes).unwrap_or(u16::MAX);
            if full_chroma {
                // SAFETY: the bitfield view of a zeroed word.
                let ef = unsafe { &mut ext.rext.slice_ext_flags.bits };
                ef.set_cu_chroma_qp_offset_enabled_flag(u32::from(h.cu_chroma_qp_offset_enabled));
            }
        }

        self.staging.count = 0;
        // A range-extension profile takes the whole extension structure,
        // the base profiles the base alone, from the same storage.
        let (picture_size, slice_size) = if full_chroma {
            (
                size_of::<VAPictureParameterBufferHEVCExtension>(),
                size_of::<VASliceParameterBufferHEVCExtension>(),
            )
        } else {
            (
                size_of::<VAPictureParameterBufferHEVC>(),
                size_of::<VASliceParameterBufferHEVC>(),
            )
        };
        let picture_ptr: *mut c_void = (&raw mut self.staging.hevc_picture).cast();
        self.buffer(VAPictureParameterBufferType, picture_ptr, picture_size)?;
        if scaling_enabled {
            let matrix_ptr: *mut c_void = (&raw mut self.staging.hevc_matrix).cast();
            self.buffer(
                VAIQMatrixBufferType,
                matrix_ptr,
                size_of::<VAIQMatrixBufferHEVC>(),
            )?;
        }
        for i in 0..slices {
            let (offset, len) = self
                .hevc
                .job()
                .and_then(|j| j.slices.as_slice().get(i).copied().flatten())
                .map(|s| (s.offset, s.len))
                .ok_or(Error::Status(-1))?;
            let param_ptr: *mut c_void = self
                .staging
                .hevc_slices
                .get_mut(i)
                .map(|s| (s as *mut VASliceParameterBufferHEVCExtension).cast())
                .ok_or(Error::TooLarge)?;
            self.buffer(VASliceParameterBufferType, param_ptr, slice_size)?;
            let data = unit.get(offset..offset + len).ok_or(Error::Status(-1))?;
            self.buffer(
                VASliceDataBufferType,
                data.as_ptr().cast_mut().cast(),
                data.len(),
            )?;
        }
        Ok(())
    }

    /// Read `slot`'s picture into the planes.
    fn read_back(&mut self, slot: usize, out: &mut Planes<'_>) -> Result<()> {
        self.read_surface(self.surface(slot), out)
    }

    /// Read a surface's picture into the planes.
    fn read_surface(&mut self, surface: VASurfaceID, out: &mut Planes<'_>) -> Result<()> {
        let started = lowlat_common::clock::Time::now();
        // SAFETY: the surface is this backend's; the wait returns when the
        // device has finished writing it.
        let status = unsafe { (self.va().sync_surface)(self.display.raw(), surface) };
        self.check(status)?;
        let synced = lowlat_common::clock::Time::now();
        // The surface's own storage is mapped rather than copied into a
        // linear image first: measured at 720p, the driver's copy costs more
        // than reading the mapping does (0.86 to 1.7 ms against 0.77 to 1.0).
        let mut image: VAImage = zeroed();
        // SAFETY: the image is a live local the driver fills.
        let status =
            unsafe { (self.va().derive_image)(self.display.raw(), surface, &raw mut image) };
        self.check(status)?;
        let mut mapped: *mut c_void = core::ptr::null_mut();
        // SAFETY: the image's buffer is the driver's; the mapping lives until
        // the unmap below.
        let status =
            unsafe { (self.va().map_buffer)(self.display.raw(), image.buf, &raw mut mapped) };
        if let Err(e) = self.check(status) {
            // SAFETY: derived above, destroyed once.
            unsafe { (self.va().destroy_image)(self.display.raw(), image.image_id) };
            return Err(e);
        }
        let result = copy_planes(&image, mapped, self.format(), out);
        // SAFETY: mapped above; unmapped once, then the derived image is
        // destroyed once.
        unsafe {
            (self.va().unmap_buffer)(self.display.raw(), image.buf);
            (self.va().destroy_image)(self.display.raw(), image.image_id);
        }
        let done = lowlat_common::clock::Time::now();
        self.decode_us = micros(lowlat_common::clock::diff_ms(started, synced));
        self.readback_us = micros(lowlat_common::clock::diff_ms(synced, done));
        result
    }
}

/// A weighted-prediction offset at eight bits: what the base parameters
/// carry, and the whole range of every stream this backend admits.
fn narrow(offset: i16) -> i8 {
    i8::try_from(offset).unwrap_or(0)
}

/// Whole microseconds from a millisecond figure, saturated.
fn micros(ms: f64) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative duration in whole microseconds, saturated"
    )]
    let us = (ms * 1000.0).max(0.0).min(f64::from(u32::MAX)) as u32;
    us
}

/// Copy the visible planes out of a mapped image, in the layout the
/// driver derived it to: the two-plane layouts as they are; the planar
/// full-chroma layout as three copies; the packed ones unpacked, the
/// ten-bit samples moved to the high bits as every other backend hands
/// them out.
fn copy_planes(
    image: &VAImage,
    mapped: *mut c_void,
    format: Format,
    out: &mut Planes<'_>,
) -> Result<()> {
    if mapped.is_null() {
        return Err(Error::Status(-1));
    }
    let width = usize::from(image.width);
    let height = usize::from(image.height);
    let total = usize::try_from(image.data_size).unwrap_or(0);
    // SAFETY: the driver mapped `data_size` bytes at `mapped`.
    let source = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), total) };
    let plane = |index: usize| -> (usize, usize) {
        (
            usize::try_from(image.offsets.get(index).copied().unwrap_or(0)).unwrap_or(0),
            usize::try_from(image.pitches.get(index).copied().unwrap_or(0)).unwrap_or(0),
        )
    };
    let fourcc = image.format.fourcc;
    match (format, fourcc, image.num_planes) {
        (Format::Nv12 | Format::P010, VA_FOURCC_NV12 | VA_FOURCC_P010, 2) => {
            let row_bytes = width * format.sample();
            let (y_offset, y_pitch) = plane(0);
            let (uv_offset, uv_pitch) = plane(1);
            copy_rows(
                source,
                y_offset,
                y_pitch,
                out.y,
                out.y_pitch,
                row_bytes,
                height,
            )?;
            copy_rows(
                source,
                uv_offset,
                uv_pitch,
                out.uv,
                out.uv_pitch,
                row_bytes,
                height / 2,
            )
        }
        (Format::Yuv444, VA_FOURCC_444P, 3) => {
            let (y_offset, y_pitch) = plane(0);
            let (u_offset, u_pitch) = plane(1);
            let (v_offset, v_pitch) = plane(2);
            copy_rows(source, y_offset, y_pitch, out.y, out.y_pitch, width, height)?;
            copy_rows(
                source,
                u_offset,
                u_pitch,
                out.uv,
                out.uv_pitch,
                width,
                height,
            )?;
            copy_rows(source, v_offset, v_pitch, out.v, out.v_pitch, width, height)
        }
        (Format::Yuv444, VA_FOURCC_XYUV | VA_FOURCC_AYUV, 1) => {
            let (offset, pitch) = plane(0);
            let rows = height.min(out.y.len() / out.y_pitch.max(1));
            for row in 0..rows {
                let from = source
                    .get(offset + row * pitch..offset + row * pitch + 4 * width)
                    .ok_or(Error::Status(-1))?;
                let y = out
                    .y
                    .get_mut(row * out.y_pitch..row * out.y_pitch + width)
                    .ok_or(Error::TooLarge)?;
                let u = out
                    .uv
                    .get_mut(row * out.uv_pitch..row * out.uv_pitch + width)
                    .ok_or(Error::TooLarge)?;
                let v = out
                    .v
                    .get_mut(row * out.v_pitch..row * out.v_pitch + width)
                    .ok_or(Error::TooLarge)?;
                unpack_vuyx_row(from, y, u, v);
            }
            Ok(())
        }
        (Format::Yuv444_16, VA_FOURCC_Y410, 1) => {
            let (offset, pitch) = plane(0);
            let rows = height.min(out.y.len() / out.y_pitch.max(1));
            for row in 0..rows {
                let from = source
                    .get(offset + row * pitch..offset + row * pitch + 4 * width)
                    .ok_or(Error::Status(-1))?;
                let y = out
                    .y
                    .get_mut(row * out.y_pitch..row * out.y_pitch + 2 * width)
                    .ok_or(Error::TooLarge)?;
                let u = out
                    .uv
                    .get_mut(row * out.uv_pitch..row * out.uv_pitch + 2 * width)
                    .ok_or(Error::TooLarge)?;
                let v = out
                    .v
                    .get_mut(row * out.v_pitch..row * out.v_pitch + 2 * width)
                    .ok_or(Error::TooLarge)?;
                unpack_y410_row(from, y, u, v);
            }
            Ok(())
        }
        _ => Err(Error::Status(-1)),
    }
}

/// `rows` rows of `row_bytes` from a plane of the mapping into a plane of
/// the caller's, each at its own pitch; short output takes what fits.
fn copy_rows(
    source: &[u8],
    offset: usize,
    pitch: usize,
    to: &mut [u8],
    to_pitch: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<()> {
    let rows = rows.min(to.len() / to_pitch.max(1));
    for row in 0..rows {
        let from = source
            .get(offset + row * pitch..offset + row * pitch + row_bytes)
            .ok_or(Error::Status(-1))?;
        let dst = to
            .get_mut(row * to_pitch..row * to_pitch + row_bytes)
            .ok_or(Error::TooLarge)?;
        dst.copy_from_slice(from);
    }
    Ok(())
}

fn va_picture_h264_invalid() -> VAPictureH264 {
    VAPictureH264 {
        picture_id: VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    }
}

fn va_picture_h264(r: &h264::dpb::RefPic, surface: VASurfaceID) -> VAPictureH264 {
    let mut flags = if r.long_term {
        VA_PICTURE_H264_LONG_TERM_REFERENCE
    } else {
        VA_PICTURE_H264_SHORT_TERM_REFERENCE
    };
    match r.parity {
        Some(Parity::Top) => flags |= VA_PICTURE_H264_TOP_FIELD,
        Some(Parity::Bottom) => flags |= VA_PICTURE_H264_BOTTOM_FIELD,
        None => {}
    }
    VAPictureH264 {
        picture_id: surface,
        frame_idx: r.frame_idx,
        flags,
        TopFieldOrderCnt: r.top_poc,
        BottomFieldOrderCnt: r.bottom_poc,
        va_reserved: [0; 4],
    }
}

fn va_picture_h264_current(current: &h264::dpb::Current, surface: VASurfaceID) -> VAPictureH264 {
    let flags = match current.structure {
        Structure::Frame => 0,
        Structure::Field(Parity::Top) => VA_PICTURE_H264_TOP_FIELD,
        Structure::Field(Parity::Bottom) => VA_PICTURE_H264_BOTTOM_FIELD,
    };
    VAPictureH264 {
        picture_id: surface,
        frame_idx: current.frame_num,
        flags,
        TopFieldOrderCnt: current.top_poc,
        BottomFieldOrderCnt: current.bottom_poc,
        va_reserved: [0; 4],
    }
}

impl Decoder for Backend<'_> {
    fn build(&mut self, header: &VideoHeader) -> core::result::Result<(), Fault> {
        self.codec = header.codec;
        // The declaration's guess at the shape; the first parameter set
        // corrects it before anything is built on it.
        self.shape = Shape {
            ten_bit: header.ten_bit,
            full_chroma: false,
        };
        self.h264.reset();
        self.hevc.reset();
        self.create_config().map_err(|_| Fault::Fatal)
    }

    fn feed(&mut self, unit: &[u8]) -> core::result::Result<Fed, Fault> {
        let result = match self.codec {
            Codec::H264 => self.decode_h264(unit),
            Codec::H265 => self.decode_hevc(unit),
        };
        match result {
            Ok(fed) => Ok(fed),
            // A picture the planes cannot take would be refused again on
            // every keyframe asked for; nothing to ask.
            Err(Error::NoProfile | Error::Runtime(_) | Error::TooLarge) => Err(Fault::Fatal),
            Err(Error::Status(_)) => Err(Fault::Unrecoverable),
        }
    }

    fn take(&mut self, out: &mut Planes<'_>) -> core::result::Result<Option<Picture>, Fault> {
        let Some((slot, order)) = self.next_output() else {
            return Ok(None);
        };
        let read = self.read_back(slot, out);
        self.last_taken = Some(slot);
        self.taken(slot);
        read.map_err(|_| Fault::Unrecoverable)?;
        Ok(Some(self.picture(order)))
    }

    fn destroy(&mut self) {
        self.destroy_context();
        self.destroy_config();
    }
}

impl Drop for Backend<'_> {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl Backend<'_> {
    /// Whether this context's pictures can be lent rather than read back.
    pub fn lends(&self) -> bool {
        self.lends && self.context.is_some()
    }

    /// The next picture in output order, lent rather than read back: its
    /// decode waited for and its surface handed out as its export, which
    /// is never decoded into until [`lease_ended`](Self::lease_ended) names
    /// it. `allocation` numbers a surface's export the first time the
    /// surface is lent.
    pub fn take_lent(
        &mut self,
        allocation: impl FnOnce() -> u32,
    ) -> core::result::Result<Option<(Picture, Lent)>, Fault> {
        let Some((slot, order)) = self.next_output() else {
            return Ok(None);
        };
        let lent = self.lend(slot, allocation);
        self.last_taken = Some(slot);
        self.taken(slot);
        let lent = lent.map_err(|_| Fault::Unrecoverable)?;
        Ok(Some((self.picture(order), lent)))
    }

    /// A holder let a lent surface go: it may be decoded into again. A
    /// lease of an earlier context, or another decoder's, frees nothing.
    pub fn lease_ended(&mut self, lease: Lease) {
        self.pool.end(lease);
    }

    /// Read a lent surface back as its holder would see it; a test's need,
    /// since a lent picture is read by its holder's own device.
    pub fn read_lent(&mut self, lease: Lease, out: &mut Planes<'_>) -> Result<()> {
        if lease.context != self.pool.context || !self.pool.is_lent(lease.buffer as usize) {
            return Err(Error::Status(-1));
        }
        let surface = self
            .surfaces
            .get(lease.buffer as usize)
            .copied()
            .ok_or(Error::Status(-1))?;
        self.read_surface(surface, out)
    }

    /// Spare surfaces made for the context, at most [`SPARES`].
    pub fn spares(&self) -> usize {
        self.pool.made.saturating_sub(SURFACES)
    }

    fn lend(&mut self, slot: usize, allocation: impl FnOnce() -> u32) -> Result<Lent> {
        if !self.lends() {
            return Err(Error::Status(-1));
        }
        let index = self.pool.surface(slot).ok_or(Error::Status(-1))?;
        let surface = self.surface(slot);
        let started = lowlat_common::clock::Time::now();
        // SAFETY: the surface is this backend's; the wait returns when the
        // device has finished writing it.
        let status = unsafe { (self.va().sync_surface)(self.display.raw(), surface) };
        self.check(status)?;
        let synced = lowlat_common::clock::Time::now();
        let display = self.display;
        let entry = self.exports.get_mut(index).ok_or(Error::Status(-1))?;
        if entry.is_none() {
            *entry = Some((display.export(surface)?, 0));
        }
        let (export, number) = entry.as_mut().ok_or(Error::Status(-1))?;
        if *number == 0 {
            *number = allocation();
        }
        let fd = export.fd.try_clone().map_err(|_| Error::Status(-1))?;
        let (size, modifier, chroma_offset, pitches, allocation) = (
            export.size,
            export.modifier,
            export.chroma_offset,
            export.pitches,
            *number,
        );
        let lease = self.pool.lend(index).ok_or(Error::Status(-1))?;
        let done = lowlat_common::clock::Time::now();
        self.decode_us = micros(lowlat_common::clock::diff_ms(started, synced));
        self.readback_us = micros(lowlat_common::clock::diff_ms(synced, done));
        Ok(Lent {
            fd,
            size,
            modifier,
            chroma_offset,
            pitches,
            allocation,
            device: display.device(),
            lease,
        })
    }

    /// The next picture to leave the picture buffer: its slot and order.
    fn next_output(&mut self) -> Option<(usize, i32)> {
        match self.codec {
            Codec::H264 => self.h264.next_output().map(|o| (o.slot, o.poc)),
            Codec::H265 => self.hevc.next_output().map(|o| (o.slot, o.poc)),
        }
    }

    /// The picture on `slot` has left: the picture buffer may reuse the slot
    /// once nothing references it.
    fn taken(&mut self, slot: usize) {
        match self.codec {
            Codec::H264 => self.h264.dpb.taken(slot),
            Codec::H265 => self.hevc.dpb.taken(slot),
        }
    }

    /// What a picture leaving now is, from the active parameter set.
    fn picture(&self, order: i32) -> Picture {
        let ((width, height), full_range) = match self.codec {
            Codec::H264 => self
                .h264
                .active_sps()
                .map_or(((0, 0), false), |s| (s.visible(), s.vui.video_full_range)),
            Codec::H265 => self
                .hevc
                .active_sps()
                .map_or(((0, 0), false), |s| (s.visible(), s.video_full_range)),
        };
        Picture {
            format: self.format(),
            width,
            height,
            order,
            full_range,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape picks the profile, the render-target format and the layout
    /// out, and the base profiles are what a declaration guesses.
    #[test]
    fn the_shape_names_its_profile_and_layout() {
        let cases = [
            (
                false,
                false,
                VAProfileHEVCMain,
                VA_RT_FORMAT_YUV420,
                Format::Nv12,
            ),
            (
                true,
                false,
                VAProfileHEVCMain10,
                VA_RT_FORMAT_YUV420_10,
                Format::P010,
            ),
            (
                false,
                true,
                VAProfileHEVCMain444,
                VA_RT_FORMAT_YUV444,
                Format::Yuv444,
            ),
            (
                true,
                true,
                VAProfileHEVCMain444_10,
                VA_RT_FORMAT_YUV444_10,
                Format::Yuv444_16,
            ),
        ];
        for (ten_bit, full_chroma, profile, rt, format) in cases {
            let shape = Shape {
                ten_bit,
                full_chroma,
            };
            assert_eq!(shape.profile(Codec::H265), profile);
            assert_eq!(shape.profile(Codec::H264), VAProfileH264High);
            assert_eq!(shape.rt_format(), rt);
            assert_eq!(shape.format(), format);
        }
    }

    /// A stream decoded into `slots` in turn, each picture lent as it leaves
    /// and held by a ring of `holders`, each holder letting its last picture
    /// go when it takes the next: every picture is decoded into a surface no
    /// holder keeps. The surfaces made at the end, or `None` where a slot
    /// could not be covered.
    fn run(pool: &mut Pool, slots: &[usize], holders: usize, pictures: usize) -> Option<usize> {
        let mut held: Vec<Option<Lease>> = vec![None; holders];
        for n in 0..pictures {
            let slot = slots[n % slots.len()];
            match pool.cover(slot)? {
                Cover::Keep => {}
                Cover::Spare(s) | Cover::Make(s) => pool.assign(slot, s),
            }
            let surface = pool.surface(slot).unwrap();
            assert!(
                !pool.is_lent(surface),
                "picture {n} decoded into lent surface {surface}"
            );
            if let Some(old) = held[n % holders].take() {
                pool.end(old);
            }
            held[n % holders] = pool.lend(surface);
            assert!(held[n % holders].is_some());
        }
        Some(pool.made)
    }

    /// **A lent surface is never decoded into**, whatever order the picture
    /// buffer reuses its slots in, and the spares stay within the pool: a
    /// stream of one reference, of none (every picture on one slot, the most
    /// spares it takes), of eight slots in turn, and of the whole buffer.
    #[test]
    fn a_lent_surface_is_never_decoded_into() {
        let all: Vec<usize> = (0..SURFACES).collect();
        for slots in [&[0, 1][..], &[0], &[0, 1, 2, 3, 4, 5, 6, 7], &all] {
            let mut pool = Pool::fresh(SURFACES, 1);
            let made = run(&mut pool, slots, SPARES, 1000).expect("every slot covered");
            assert!(made <= POOL, "{slots:?}: {made} surfaces");
        }
        // One slot for every picture lends four at once and needs every
        // spare; the whole buffer in turn reuses a slot after its holder let
        // go, and needs none.
        let mut pool = Pool::fresh(SURFACES, 1);
        assert_eq!(run(&mut pool, &[0], SPARES, 1000), Some(POOL));
        let mut pool = Pool::fresh(SURFACES, 1);
        assert_eq!(run(&mut pool, &all, SPARES, 1000), Some(SURFACES));
    }

    /// The bound is the holders': one holder more than the spares cover
    /// runs out of surfaces, and is refused rather than decoded into a lent
    /// one.
    #[test]
    fn a_holder_past_the_spares_is_refused() {
        let mut pool = Pool::fresh(SURFACES, 1);
        assert_eq!(run(&mut pool, &[0], SPARES + 1, 100), None);
    }

    /// A lease names its context: one ended after the context was made
    /// again frees nothing of the new one's, and one of the same context
    /// frees its surface.
    #[test]
    fn a_lease_of_an_earlier_context_frees_nothing() {
        let mut old = Pool::fresh(SURFACES, 7);
        let lease = old.lend(3).unwrap();
        let mut new = Pool::fresh(SURFACES, 8);
        new.lend(3).unwrap();
        new.end(lease);
        assert!(new.is_lent(3), "an old context's lease freed a surface");
        old.end(lease);
        assert!(!old.is_lent(3));
        // No context: nothing is lent and nothing is freed.
        let mut none = Pool::fresh(0, 0);
        none.end(Lease {
            context: 0,
            buffer: 0,
        });
        assert!(!none.is_lent(0));
    }
}
