//! The open-stack backend: the device decodes what the readers read.
//!
//! One configuration and one context per decoder, a fixed pool of surfaces
//! the picture buffers index by slot, and a read-back of each picture as it
//! leaves the buffer. Every buffer the device is handed is staged in
//! storage allocated with the backend; nothing is allocated per picture.

use core::ffi::{CStr, c_int, c_uint, c_void};

use lowlat_core::video::{Codec, VideoHeader};
use lowlat_drivers::ffi::va::{
    VA_INVALID_ID, VA_INVALID_SURFACE, VA_PICTURE_H264_BOTTOM_FIELD, VA_PICTURE_H264_INVALID,
    VA_PICTURE_H264_LONG_TERM_REFERENCE, VA_PICTURE_H264_SHORT_TERM_REFERENCE,
    VA_PICTURE_H264_TOP_FIELD, VA_PICTURE_HEVC_INVALID, VA_PICTURE_HEVC_LONG_TERM_REFERENCE,
    VA_PICTURE_HEVC_RPS_LT_CURR, VA_PICTURE_HEVC_RPS_ST_CURR_AFTER,
    VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE, VA_RT_FORMAT_YUV420, VA_RT_FORMAT_YUV420_10,
    VA_SLICE_DATA_FLAG_ALL, VABufferID, VAConfigAttrib, VAConfigAttribRTFormat, VAConfigID,
    VAContextID, VAEntrypointVLD, VAIQMatrixBufferH264, VAIQMatrixBufferHEVC, VAIQMatrixBufferType,
    VAImage, VAPictureH264, VAPictureHEVC, VAPictureParameterBufferH264,
    VAPictureParameterBufferHEVC, VAPictureParameterBufferType, VAProfile, VAProfileH264High,
    VAProfileHEVCMain, VAProfileHEVCMain10, VASliceDataBufferType, VASliceParameterBufferH264,
    VASliceParameterBufferHEVC, VASliceParameterBufferType, VASurfaceID,
};
use lowlat_drivers::va;
pub use lowlat_drivers::va::{Display, Error as RuntimeError, Vaapi};

use crate::h264::dpb::{Parity, Structure};
use crate::{Caps, Decoder, Fault, Fed, Format, Picture, Planes, h264, hevc};

/// Surfaces a context holds: what either picture buffer can index.
const SURFACES: usize = h264::dpb::MAX_FRAMES;
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

fn profile_for(codec: Codec, ten_bit: bool) -> VAProfile {
    match (codec, ten_bit) {
        (Codec::H264, _) => VAProfileH264High,
        (Codec::H265, false) => VAProfileHEVCMain,
        (Codec::H265, true) => VAProfileHEVCMain10,
    }
}

/// Ask a display what it decodes. **Full chroma is never reported here**:
/// this backend's full-chroma surfaces are a layout no device it was built
/// on can verify, so the profile is not asked for and such a stream is
/// refused, whatever the device lists.
pub fn caps(display: &Display<'_>) -> Result<Caps> {
    let profiles = display.profiles()?;
    let decodes = |profile: VAProfile| -> Result<bool> {
        if !profiles.contains(&profile) {
            return Ok(false);
        }
        Ok(display.entrypoints(profile)?.contains(&VAEntrypointVLD))
    };
    Ok(Caps {
        h264: decodes(VAProfileH264High)?,
        hevc: decodes(VAProfileHEVCMain)?,
        hevc_10: decodes(VAProfileHEVCMain10)?,
        hevc_444: false,
        hevc_444_10: false,
    })
}

/// Open the runtime and a render node and ask what it decodes: the probe a
/// client makes at creation, so a machine without a decoder is refused
/// before anything connects.
pub fn probe(node: &CStr) -> Result<Caps> {
    let va = Vaapi::load()?;
    let display = va.open(node)?;
    caps(&display)
}

/// The staged buffers for one picture.
struct Staging {
    h264_picture: VAPictureParameterBufferH264,
    h264_matrix: VAIQMatrixBufferH264,
    h264_slices: Box<[VASliceParameterBufferH264; h264::MAX_SLICES]>,
    hevc_picture: VAPictureParameterBufferHEVC,
    hevc_matrix: VAIQMatrixBufferHEVC,
    hevc_slices: Box<[VASliceParameterBufferHEVC; hevc::MAX_SLICES]>,
    ids: [VABufferID; BUFFERS],
    count: usize,
}

/// The decoder over one display.
pub struct Backend<'a> {
    display: &'a Display<'a>,
    /// The largest coded picture the caller's planes take.
    ceiling: (u32, u32),
    codec: Codec,
    ten_bit: bool,
    config: Option<VAConfigID>,
    context: Option<VAContextID>,
    surfaces: [VASurfaceID; SURFACES],
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
            .field("ten_bit", &self.ten_bit)
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
            ten_bit: false,
            config: None,
            context: None,
            surfaces: [VA_INVALID_SURFACE; SURFACES],
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
        if self.ten_bit {
            Format::P010
        } else {
            Format::Nv12
        }
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

    fn create_config(&mut self) -> Result<()> {
        let profile = profile_for(self.codec, self.ten_bit);
        let mut attrib = VAConfigAttrib {
            type_: VAConfigAttribRTFormat,
            value: if self.ten_bit {
                VA_RT_FORMAT_YUV420_10
            } else {
                VA_RT_FORMAT_YUV420
            },
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

    /// The surfaces and the context, at the stream's coded size.
    fn create_context(&mut self, width: u32, height: u32) -> Result<()> {
        let Some(config) = self.config else {
            return Err(Error::NoProfile);
        };
        if width > self.ceiling.0 || height > self.ceiling.1 {
            return Err(Error::TooLarge);
        }
        let rt_format = if self.ten_bit {
            VA_RT_FORMAT_YUV420_10
        } else {
            VA_RT_FORMAT_YUV420
        };
        // SAFETY: the array is writable for the count passed; no attributes.
        let status = unsafe {
            (self.va().create_surfaces)(
                self.display.raw(),
                rt_format,
                width,
                height,
                self.surfaces.as_mut_ptr(),
                c_uint::try_from(SURFACES).unwrap_or(0),
                core::ptr::null_mut(),
                0,
            )
        };
        self.check(status)?;
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
            self.destroy_surfaces();
            return Err(e);
        }
        self.context = Some(context);
        self.coded_width = width;
        self.coded_height = height;
        Ok(())
    }

    fn destroy_surfaces(&mut self) {
        // SAFETY: the surfaces were created together and are destroyed
        // together, once.
        unsafe {
            (self.va().destroy_surfaces)(
                self.display.raw(),
                self.surfaces.as_mut_ptr(),
                c_int::try_from(SURFACES).unwrap_or(0),
            );
        }
        self.surfaces = [VA_INVALID_SURFACE; SURFACES];
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

    fn surface(&self, slot: usize) -> VASurfaceID {
        self.surfaces
            .get(slot)
            .copied()
            .unwrap_or(VA_INVALID_SURFACE)
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
        self.stage_h264(unit)?;
        let slot = self
            .h264
            .job()
            .map(|j| j.current.slot)
            .ok_or(Error::Status(-1))?;
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
        let surfaces = self.surfaces;
        let surface_of = |slot: usize| surfaces.get(slot).copied().unwrap_or(VA_INVALID_SURFACE);
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
        // Full chroma and the range-extension tools are staged through
        // parameter structures of their own, which no device this backend
        // was verified on decodes; the base structures would decode such a
        // stream wrongly without an error, so it is refused outright.
        if job.sps.is_range_extended() {
            self.hevc.abandon();
            return Err(Error::NoProfile);
        }
        if self.context.is_none() {
            self.create_context(width, height)?;
        } else if width != self.coded_width || height != self.coded_height {
            self.hevc.abandon();
            return Ok(Fed::FormatChanged);
        }
        self.stage_hevc(unit)?;
        let slot = self
            .hevc
            .job()
            .map(|j| j.current.slot)
            .ok_or(Error::Status(-1))?;
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
        let surfaces = self.surfaces;
        let surface_of = |slot: usize| surfaces.get(slot).copied().unwrap_or(VA_INVALID_SURFACE);
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

        let picture = &mut self.staging.hevc_picture;
        *picture = zeroed();
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
            let Some(out) = self.staging.hevc_slices.get_mut(i) else {
                break;
            };
            *out = zeroed();
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
                // Eight bits: the base parameters, which is all this backend
                // stages, take the offsets at the range every stream it
                // admits stays within.
                out.delta_luma_weight_l0 = w.l0.delta_luma_weight;
                out.luma_offset_l0 = w.l0.luma_offset.map(narrow);
                out.delta_chroma_weight_l0 = w.l0.delta_chroma_weight;
                out.ChromaOffsetL0 = w.l0.chroma_offset.map(|pair| pair.map(narrow));
                out.delta_luma_weight_l1 = w.l1.delta_luma_weight;
                out.luma_offset_l1 = w.l1.luma_offset.map(narrow);
                out.delta_chroma_weight_l1 = w.l1.delta_chroma_weight;
                out.ChromaOffsetL1 = w.l1.chroma_offset.map(|pair| pair.map(narrow));
            }
            out.five_minus_max_num_merge_cand = h.five_minus_max_num_merge_cand;
            out.num_entry_point_offsets =
                u16::try_from(h.num_entry_point_offsets).unwrap_or(u16::MAX);
            out.entry_offset_to_subset_array = 0;
            out.slice_data_num_emu_prevn_bytes =
                u16::try_from(h.header_escapes).unwrap_or(u16::MAX);
        }

        self.staging.count = 0;
        let picture_ptr: *mut c_void = (&raw mut self.staging.hevc_picture).cast();
        self.buffer(
            VAPictureParameterBufferType,
            picture_ptr,
            size_of::<VAPictureParameterBufferHEVC>(),
        )?;
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
                .map(|s| (s as *mut VASliceParameterBufferHEVC).cast())
                .ok_or(Error::TooLarge)?;
            self.buffer(
                VASliceParameterBufferType,
                param_ptr,
                size_of::<VASliceParameterBufferHEVC>(),
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

    /// Read `slot`'s picture into the planes.
    fn read_back(&mut self, slot: usize, out: &mut Planes<'_>) -> Result<()> {
        let surface = self.surface(slot);
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

/// Copy the visible planes out of a mapped image.
fn copy_planes(
    image: &VAImage,
    mapped: *mut c_void,
    format: Format,
    out: &mut Planes<'_>,
) -> Result<()> {
    if image.num_planes < 2 || mapped.is_null() {
        return Err(Error::Status(-1));
    }
    let sample = format.sample();
    let width = usize::from(image.width);
    let height = usize::from(image.height);
    let row_bytes = width * sample;
    let y_pitch = usize::try_from(image.pitches[0]).unwrap_or(0);
    let uv_pitch = usize::try_from(image.pitches[1]).unwrap_or(0);
    let y_offset = usize::try_from(image.offsets[0]).unwrap_or(0);
    let uv_offset = usize::try_from(image.offsets[1]).unwrap_or(0);
    let total = usize::try_from(image.data_size).unwrap_or(0);
    // SAFETY: the driver mapped `data_size` bytes at `mapped`.
    let source = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), total) };
    let rows_y = height.min(out.y.len() / out.y_pitch.max(1));
    for row in 0..rows_y {
        let from = source
            .get(y_offset + row * y_pitch..y_offset + row * y_pitch + row_bytes)
            .ok_or(Error::Status(-1))?;
        let to = out
            .y
            .get_mut(row * out.y_pitch..row * out.y_pitch + row_bytes)
            .ok_or(Error::TooLarge)?;
        to.copy_from_slice(from);
    }
    let rows_uv = (height / 2).min(out.uv.len() / out.uv_pitch.max(1));
    for row in 0..rows_uv {
        let from = source
            .get(uv_offset + row * uv_pitch..uv_offset + row * uv_pitch + row_bytes)
            .ok_or(Error::Status(-1))?;
        let to = out
            .uv
            .get_mut(row * out.uv_pitch..row * out.uv_pitch + row_bytes)
            .ok_or(Error::TooLarge)?;
        to.copy_from_slice(from);
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
        self.ten_bit = header.ten_bit;
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
        let (slot, order) = match self.codec {
            Codec::H264 => match self.h264.next_output() {
                Some(o) => (o.slot, o.poc),
                None => return Ok(None),
            },
            Codec::H265 => match self.hevc.next_output() {
                Some(o) => (o.slot, o.poc),
                None => return Ok(None),
            },
        };
        let read = self.read_back(slot, out);
        self.last_taken = Some(slot);
        match self.codec {
            Codec::H264 => self.h264.dpb.taken(slot),
            Codec::H265 => self.hevc.dpb.taken(slot),
        }
        read.map_err(|_| Fault::Unrecoverable)?;
        let (width, height) = match self.codec {
            Codec::H264 => self.h264.active_sps().map_or((0, 0), |s| s.visible()),
            Codec::H265 => self.hevc.active_sps().map_or((0, 0), |s| s.visible()),
        };
        Ok(Some(Picture {
            format: self.format(),
            width,
            height,
            order,
        }))
    }

    fn destroy(&mut self) {
        self.destroy_context();
        if let Some(config) = self.config.take() {
            // SAFETY: created by this backend, destroyed once.
            unsafe { (self.va().destroy_config)(self.display.raw(), config) };
        }
    }
}

impl Drop for Backend<'_> {
    fn drop(&mut self) {
        self.destroy();
    }
}
