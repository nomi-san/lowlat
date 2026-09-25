//! The vendor backend: the same readers, the vendor's decode interface.
//!
//! One decoder per stream, created at the first picture from what the
//! parameter sets say (the coded size, the chroma and the depth), and a
//! surface per picture-buffer slot so the readers' slots index the device's
//! surfaces directly. The picture and slice parameters the interface takes
//! are filled from the same jobs the open-stack backend stages; the slice
//! data is handed over with a start code ahead of each slice, in a buffer
//! sized once. A decoded picture is mapped, copied out to the caller's
//! planes -- in host memory, or on the device itself -- and unmapped.
//!
//! **The interface's own parser is not used.** It would be a second reader
//! and a second picture buffer beside the ones every clip is checked
//! against, with a reordering rule of its own and no view into what it
//! decided.

use core::ffi::c_int;

use lowlat_core::video::{Codec, VideoHeader};
use lowlat_drivers::cuda::{self, Cuda, Event, Stream};
use lowlat_drivers::cuvid::{self, Cuvid};
use lowlat_drivers::ffi::cuvid::{
    CUVIDDECODECAPS, CUVIDDECODECREATEINFO, CUVIDH264DPBENTRY, CUVIDPICPARAMS, CUVIDPROCPARAMS,
    cudaVideoChromaFormat_420, cudaVideoChromaFormat_444, cudaVideoCodec_H264, cudaVideoCodec_HEVC,
    cudaVideoCreate_PreferCUVID, cudaVideoDeinterlaceMode_Weave, cudaVideoSurfaceFormat_NV12,
    cudaVideoSurfaceFormat_P016, cudaVideoSurfaceFormat_YUV444,
    cudaVideoSurfaceFormat_YUV444_16Bit,
};

use crate::h264::dpb::{Parity, Structure};
use crate::hevc::sps::{DIAG_4X4, DIAG_8X8};
use crate::{Caps, Decoder, Fault, Fed, Format, Picture, Planes, h264, hevc};

/// Surfaces a decoder holds: what either picture buffer can index.
const SURFACES: usize = h264::dpb::MAX_FRAMES;
/// Slices one picture may hand the device.
const MAX_SLICES: usize = h264::MAX_SLICES;
/// The start code put ahead of every slice in the buffer the device reads.
const START_CODE: [u8; 3] = [0, 0, 1];

/// Why a decoder could not be built or a picture could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The runtimes, or the device.
    Runtime(cuvid::Error),
    /// The device decodes no combination this stream needs.
    NoProfile,
    /// A call failed, with its status; zero for a reader's refusal.
    Status(u32),
    /// A picture larger than the surfaces, or a unit larger than the
    /// bitstream buffer.
    TooLarge,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "{e}"),
            Self::NoProfile => f.write_str("device decodes no profile this stream needs"),
            Self::Status(s) => write!(f, "decode interface returned status {s}"),
            Self::TooLarge => f.write_str("picture larger than the surfaces"),
        }
    }
}

impl std::error::Error for Error {}

impl From<cuvid::Error> for Error {
    fn from(e: cuvid::Error) -> Self {
        Self::Runtime(e)
    }
}

impl From<cuda::Error> for Error {
    fn from(e: cuda::Error) -> Self {
        Self::Runtime(cuvid::Error::Cuda(e))
    }
}

type Result<T> = core::result::Result<T, Error>;

// SAFETY: every field is plain data; the device's structures are filled
// whole before use, so zero is a valid starting state.
fn zeroed<T>() -> T {
    unsafe { core::mem::zeroed() }
}

/// Planes on the device, for a picture copied there rather than read
/// back: the shape of [`Planes`] with device addresses in place of slices.
/// The caller's allocation covers each plane's rows at its pitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevicePlanes {
    pub y: u64,
    pub y_pitch: usize,
    /// The interleaved chroma plane, or the first of two.
    pub uv: u64,
    pub uv_pitch: usize,
    /// The second chroma plane at full chroma; zero otherwise.
    pub v: u64,
    pub v_pitch: usize,
}

/// What a decoder is created for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    codec: Codec,
    ten_bit: bool,
    full_chroma: bool,
    width: u32,
    height: u32,
}

impl Shape {
    fn format(&self) -> Format {
        Format::of(self.ten_bit, self.full_chroma)
    }

    fn fill(&self, info: &mut CUVIDDECODECREATEINFO) {
        info.CodecType = match self.codec {
            Codec::H264 => cudaVideoCodec_H264,
            Codec::H265 => cudaVideoCodec_HEVC,
        };
        info.ChromaFormat = if self.full_chroma {
            cudaVideoChromaFormat_444
        } else {
            cudaVideoChromaFormat_420
        };
        info.OutputFormat = match (self.full_chroma, self.ten_bit) {
            (false, false) => cudaVideoSurfaceFormat_NV12,
            (false, true) => cudaVideoSurfaceFormat_P016,
            (true, false) => cudaVideoSurfaceFormat_YUV444,
            (true, true) => cudaVideoSurfaceFormat_YUV444_16Bit,
        };
        info.bitDepthMinus8 = if self.ten_bit { 2 } else { 0 };
        info.ulWidth = u64::from(self.width);
        info.ulHeight = u64::from(self.height);
        info.ulMaxWidth = u64::from(self.width);
        info.ulMaxHeight = u64::from(self.height);
        info.ulTargetWidth = u64::from(self.width);
        info.ulTargetHeight = u64::from(self.height);
        info.display_area.right = i16::try_from(self.width).unwrap_or(i16::MAX);
        info.display_area.bottom = i16::try_from(self.height).unwrap_or(i16::MAX);
        info.target_rect.right = info.display_area.right;
        info.target_rect.bottom = info.display_area.bottom;
        info.ulNumDecodeSurfaces = u64::try_from(SURFACES).unwrap_or(0);
        // One mapped at a time: a picture is copied out and unmapped before
        // the next is taken.
        info.ulNumOutputSurfaces = 1;
        info.ulCreationFlags = u64::from(cudaVideoCreate_PreferCUVID);
        info.DeinterlaceMode = cudaVideoDeinterlaceMode_Weave;
    }
}

/// Ask what the device decodes, by building a real decoder per combination
/// and destroying it: the probe a client makes at creation. The capability
/// query alone has said yes to a combination the device then failed to
/// create, and a failure here lands in the probe rather than mid-stream.
pub fn caps(cuvid: &Cuvid) -> Caps {
    let decodes = |codec: Codec, ten_bit: bool, full_chroma: bool| -> bool {
        let shape = Shape {
            codec,
            ten_bit,
            full_chroma,
            width: 256,
            height: 256,
        };
        let mut query: CUVIDDECODECAPS = zeroed();
        let mut info: CUVIDDECODECREATEINFO = zeroed();
        shape.fill(&mut info);
        query.eCodecType = info.CodecType;
        query.eChromaFormat = info.ChromaFormat;
        query.nBitDepthMinus8 = if ten_bit { 2 } else { 0 };
        // The query gates the attempt, so a combination the device has never
        // heard of is not tried; the attempt decides.
        if cuvid.caps(&mut query).is_err() || query.bIsSupported == 0 {
            return false;
        }
        if query.nOutputFormatMask & (1 << info.OutputFormat) == 0 {
            return false;
        }
        cuvid.create(&mut info).is_ok()
    };
    Caps {
        h264: decodes(Codec::H264, false, false),
        hevc: decodes(Codec::H265, false, false),
        hevc_10: decodes(Codec::H265, true, false),
        hevc_444: decodes(Codec::H265, false, true),
        hevc_444_10: decodes(Codec::H265, true, true),
    }
}

/// The largest coded picture the device decodes for a codec, from the
/// capability query, which is trusted for the size limits and nothing
/// else; zero where it does not say.
pub fn limits(cuvid: &Cuvid, codec: Codec) -> (u32, u32) {
    let mut query: CUVIDDECODECAPS = zeroed();
    query.eCodecType = match codec {
        Codec::H264 => cudaVideoCodec_H264,
        Codec::H265 => cudaVideoCodec_HEVC,
    };
    query.eChromaFormat = cudaVideoChromaFormat_420;
    if cuvid.caps(&mut query).is_err() || query.bIsSupported == 0 {
        return (0, 0);
    }
    (query.nMaxWidth, query.nMaxHeight)
}

/// The decoder over one device.
pub struct Backend<'a> {
    cuda: &'a Cuda,
    cuvid: &'a Cuvid,
    /// The largest coded picture the caller's planes take.
    ceiling: (u32, u32),
    codec: Codec,
    ten_bit: bool,
    decoder: Option<cuvid::Decoder<'a>>,
    shape: Option<Shape>,
    h264: Box<h264::Stream>,
    hevc: Box<hevc::Stream>,
    params: Box<CUVIDPICPARAMS>,
    /// The slice data the device reads: each slice with a start code ahead
    /// of it, and where each begins.
    bitstream: Vec<u8>,
    offsets: Box<[u32; MAX_SLICES]>,
    /// The stream the device copies run on, and the event recorded behind
    /// them that the copy's end is waited on by, made at the first.
    stream: Option<(Stream, Event)>,
    /// The last decode and read-back, in microseconds, for the log.
    pub decode_us: u32,
    pub readback_us: u32,
}

impl core::fmt::Debug for Backend<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Backend")
            .field("codec", &self.codec)
            .field("ten_bit", &self.ten_bit)
            .field("shape", &self.shape)
            .finish()
    }
}

impl<'a> Backend<'a> {
    /// A backend over the loaded runtimes, for pictures up to `ceiling` and
    /// access units up to `max_unit` bytes. The device's context must be
    /// current on the thread that drives this.
    pub fn new(cuda: &'a Cuda, cuvid: &'a Cuvid, ceiling: (u32, u32), max_unit: usize) -> Self {
        Self {
            cuda,
            cuvid,
            ceiling,
            codec: Codec::H264,
            ten_bit: false,
            decoder: None,
            shape: None,
            h264: Box::new(h264::Stream::new()),
            hevc: Box::new(hevc::Stream::new()),
            params: Box::new(zeroed()),
            // Every slice gains a three-byte start code; a unit is at most
            // this many slices.
            bitstream: vec![0u8; max_unit + MAX_SLICES * START_CODE.len()],
            offsets: Box::new([0; MAX_SLICES]),
            stream: None,
            decode_us: 0,
            readback_us: 0,
        }
    }

    /// Let every waiting picture out, as at the end of a stream; a test's
    /// need, since a live stream never ends this way.
    pub fn drain(&mut self) {
        self.h264.drain();
        self.hevc.drain();
    }

    /// The layout pictures come back in.
    pub fn format(&self) -> Format {
        self.shape
            .map_or(Format::of(self.ten_bit, false), |s| s.format())
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

    /// The decoder for `shape`, created if none exists; a shape that
    /// differs from the one created is the caller's format change.
    fn ensure_decoder(&mut self, shape: Shape) -> Result<bool> {
        if let Some(existing) = self.shape {
            return Ok(existing == shape);
        }
        if shape.width > self.ceiling.0 || shape.height > self.ceiling.1 {
            return Err(Error::TooLarge);
        }
        let mut info: CUVIDDECODECREATEINFO = zeroed();
        shape.fill(&mut info);
        let decoder = self.cuvid.create(&mut info).map_err(|e| match e {
            cuvid::Error::Status(_) => Error::NoProfile,
            other => Error::Runtime(other),
        })?;
        self.decoder = Some(decoder);
        self.shape = Some(shape);
        Ok(true)
    }

    /// Gather the slices of a unit into the bitstream buffer, a start code
    /// ahead of each, and record where each begins.
    fn gather(&mut self, unit: &[u8], slices: &[(usize, usize)]) -> Result<usize> {
        let mut at = 0usize;
        for (i, (offset, len)) in slices.iter().enumerate() {
            let data = unit.get(*offset..offset + len).ok_or(Error::Status(0))?;
            let end = at + START_CODE.len() + data.len();
            let room = self.bitstream.get_mut(at..end).ok_or(Error::TooLarge)?;
            let (code, body) = room.split_at_mut(START_CODE.len());
            code.copy_from_slice(&START_CODE);
            body.copy_from_slice(data);
            *self.offsets.get_mut(i).ok_or(Error::TooLarge)? =
                u32::try_from(at).map_err(|_| Error::TooLarge)?;
            at = end;
        }
        Ok(at)
    }

    fn submit(&mut self, len: usize, slices: usize) -> Result<()> {
        let params = &mut *self.params;
        params.nBitstreamDataLen = u32::try_from(len).map_err(|_| Error::TooLarge)?;
        params.pBitstreamData = self.bitstream.as_ptr();
        params.nNumSlices = u32::try_from(slices).map_err(|_| Error::TooLarge)?;
        params.pSliceDataOffsets = self.offsets.as_ptr();
        let decoder = self.decoder.as_ref().ok_or(Error::NoProfile)?;
        decoder.decode(params).map_err(|e| match e {
            cuvid::Error::Status(s) => Error::Status(s),
            other => Error::Runtime(other),
        })
    }

    fn decode_h264(&mut self, unit: &[u8]) -> Result<Fed> {
        match self.h264.read(unit).map_err(|_| Error::Status(0))? {
            h264::Read::Nothing => return Ok(self.pending_fed_h264()),
            h264::Read::FormatChanged => return Ok(Fed::FormatChanged),
            h264::Read::Picture => {}
        }
        let job = self.h264.job().ok_or(Error::Status(0))?;
        let shape = Shape {
            codec: Codec::H264,
            ten_bit: false,
            full_chroma: false,
            width: job.sps.coded_width(),
            height: job.sps.coded_height(),
        };
        if !self.ensure_decoder(shape)? {
            self.h264.abandon();
            return Ok(Fed::FormatChanged);
        }
        let (len, slices) = self.stage_h264(unit)?;
        let submitted = self.submit(len, slices);
        if let Err(e) = submitted {
            self.h264.abandon();
            return Err(e);
        }
        self.h264.finish().map_err(|_| Error::Status(0))?;
        Ok(self.pending_fed_h264())
    }

    fn pending_fed_h264(&self) -> Fed {
        if self.h264.dpb.has_output() {
            Fed::Picture
        } else {
            Fed::NeedMoreData
        }
    }

    /// Fill the picture parameters from the H.264 job; returns the gathered
    /// bitstream's length and the slice count.
    fn stage_h264(&mut self, unit: &[u8]) -> Result<(usize, usize)> {
        let job = self.h264.job().ok_or(Error::Status(0))?;
        let sps = &job.sps;
        let pps = &job.pps;
        let current = &job.current;
        let first = job
            .slices
            .items
            .first()
            .copied()
            .flatten()
            .ok_or(Error::Status(0))?;
        let intra_only = job
            .slices
            .as_slice()
            .iter()
            .flatten()
            .all(|s| s.header.slice_type.is_intra());
        let mut ranges: [(usize, usize); MAX_SLICES] = [(0, 0); MAX_SLICES];
        let slices = job.slices.len;
        for (i, slice) in job.slices.as_slice().iter().flatten().enumerate() {
            if let Some(r) = ranges.get_mut(i) {
                *r = (slice.offset, slice.len);
            }
        }

        let params = &mut *self.params;
        *params = zeroed();
        params.PicWidthInMbs = c_int::from(sps.pic_width_in_mbs_minus1) + 1;
        params.FrameHeightInMbs = c_int::try_from(sps.frame_height_in_mbs()).unwrap_or(0);
        params.CurrPicIdx = c_int::try_from(current.slot).unwrap_or(0);
        params.field_pic_flag = c_int::from(first.header.field_pic);
        params.bottom_field_flag = c_int::from(first.header.bottom_field);
        params.second_field = c_int::from(current.pair_of.is_some());
        params.ref_pic_flag = c_int::from(first.header.nal_ref_idc != 0);
        params.intra_pic_flag = c_int::from(intra_only);

        // SAFETY: the union's H.264 member of a zeroed structure.
        let h = unsafe { &mut params.CodecSpecific.h264 };
        h.log2_max_frame_num_minus4 = c_int::from(sps.log2_max_frame_num_minus4);
        h.pic_order_cnt_type = c_int::from(sps.pic_order_cnt_type);
        h.log2_max_pic_order_cnt_lsb_minus4 = c_int::from(sps.log2_max_pic_order_cnt_lsb_minus4);
        h.delta_pic_order_always_zero_flag = c_int::from(sps.delta_pic_order_always_zero);
        h.frame_mbs_only_flag = c_int::from(sps.frame_mbs_only);
        h.direct_8x8_inference_flag = c_int::from(sps.direct_8x8_inference);
        h.num_ref_frames = c_int::from(sps.max_num_ref_frames);
        h.residual_colour_transform_flag = u8::from(sps.separate_colour_plane);
        h.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
        h.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
        h.qpprime_y_zero_transform_bypass_flag = u8::from(sps.qpprime_y_zero_transform_bypass);
        h.entropy_coding_mode_flag = c_int::from(pps.entropy_coding_mode);
        h.pic_order_present_flag = c_int::from(pps.bottom_field_pic_order_in_frame_present);
        h.num_ref_idx_l0_active_minus1 = c_int::from(pps.num_ref_idx_l0_default_active_minus1);
        h.num_ref_idx_l1_active_minus1 = c_int::from(pps.num_ref_idx_l1_default_active_minus1);
        h.weighted_pred_flag = c_int::from(pps.weighted_pred);
        h.weighted_bipred_idc = c_int::from(pps.weighted_bipred_idc);
        h.pic_init_qp_minus26 = c_int::from(pps.pic_init_qp_minus26);
        h.deblocking_filter_control_present_flag =
            c_int::from(pps.deblocking_filter_control_present);
        h.redundant_pic_cnt_present_flag = c_int::from(pps.redundant_pic_cnt_present);
        h.transform_8x8_mode_flag = c_int::from(pps.transform_8x8_mode);
        h.MbaffFrameFlag = c_int::from(sps.mb_adaptive_frame_field && !first.header.field_pic);
        h.constrained_intra_pred_flag = c_int::from(pps.constrained_intra_pred);
        h.chroma_qp_index_offset = c_int::from(pps.chroma_qp_index_offset);
        h.second_chroma_qp_index_offset = c_int::from(pps.second_chroma_qp_index_offset);
        h.ref_pic_flag = c_int::from(first.header.nal_ref_idc != 0);
        h.frame_num = c_int::try_from(current.frame_num).unwrap_or(0);
        h.CurrFieldOrderCnt = [current.top_poc, current.bottom_poc];
        match current.structure {
            Structure::Field(Parity::Top) => h.CurrFieldOrderCnt[1] = current.top_poc,
            Structure::Field(Parity::Bottom) => h.CurrFieldOrderCnt[0] = current.bottom_poc,
            Structure::Frame => {}
        }
        for (i, entry) in h.dpb.iter_mut().enumerate() {
            *entry = match job.references.as_slice().get(i).copied().flatten() {
                Some(r) => CUVIDH264DPBENTRY {
                    PicIdx: c_int::try_from(r.slot).unwrap_or(-1),
                    FrameIdx: c_int::try_from(r.frame_idx).unwrap_or(0),
                    is_long_term: c_int::from(r.long_term),
                    not_existing: 0,
                    used_for_reference: match r.parity {
                        None => 3,
                        Some(Parity::Top) => 1,
                        Some(Parity::Bottom) => 2,
                    },
                    FieldOrderCnt: [r.top_poc, r.bottom_poc],
                },
                None => CUVIDH264DPBENTRY {
                    PicIdx: -1,
                    FrameIdx: 0,
                    is_long_term: 0,
                    not_existing: 0,
                    used_for_reference: 0,
                    FieldOrderCnt: [0, 0],
                },
            };
        }
        // Raster order, as the lists are kept.
        h.WeightScale4x4 = pps.scaling.list_4x4;
        h.WeightScale8x8 = pps.scaling.list_8x8;

        let len = self.gather(unit, ranges.get(..slices).unwrap_or(&[]))?;
        Ok((len, slices))
    }

    fn decode_hevc(&mut self, unit: &[u8]) -> Result<Fed> {
        match self.hevc.read(unit).map_err(|_| Error::Status(0))? {
            hevc::Read::Nothing => return Ok(self.pending_fed_hevc()),
            hevc::Read::FormatChanged => return Ok(Fed::FormatChanged),
            hevc::Read::Picture => {}
        }
        let job = self.hevc.job().ok_or(Error::Status(0))?;
        let shape = Shape {
            codec: Codec::H265,
            ten_bit: job.sps.bit_depth_luma_minus8 > 0,
            full_chroma: job.sps.chroma_format_idc == 3,
            width: job.sps.width,
            height: job.sps.height,
        };
        if !self.ensure_decoder(shape)? {
            self.hevc.abandon();
            return Ok(Fed::FormatChanged);
        }
        let (len, slices) = self.stage_hevc(unit)?;
        let submitted = self.submit(len, slices);
        if let Err(e) = submitted {
            self.hevc.abandon();
            return Err(e);
        }
        self.hevc.finish().map_err(|_| Error::Status(0))?;
        Ok(self.pending_fed_hevc())
    }

    fn pending_fed_hevc(&self) -> Fed {
        if self.hevc.dpb.has_output() {
            Fed::Picture
        } else {
            Fed::NeedMoreData
        }
    }

    /// Fill the picture parameters from the HEVC job; returns the gathered
    /// bitstream's length and the slice count.
    fn stage_hevc(&mut self, unit: &[u8]) -> Result<(usize, usize)> {
        let job = self.hevc.job().ok_or(Error::Status(0))?;
        let sps = &job.sps;
        let pps = &job.pps;
        let current = &job.current;
        let first = job
            .slices
            .items
            .first()
            .copied()
            .flatten()
            .ok_or(Error::Status(0))?;
        let intra_only = job
            .slices
            .as_slice()
            .iter()
            .flatten()
            .all(|s| s.header.slice_type.is_intra());
        let mut ranges: [(usize, usize); MAX_SLICES] = [(0, 0); MAX_SLICES];
        let slices = job.slices.len;
        for (i, slice) in job.slices.as_slice().iter().flatten().enumerate() {
            if let Some(r) = ranges.get_mut(i) {
                *r = (slice.offset, slice.len);
            }
        }

        let params = &mut *self.params;
        *params = zeroed();
        params.PicWidthInMbs = c_int::try_from(sps.width.div_ceil(16)).unwrap_or(0);
        params.FrameHeightInMbs = c_int::try_from(sps.height.div_ceil(16)).unwrap_or(0);
        params.CurrPicIdx = c_int::try_from(current.slot).unwrap_or(0);
        params.ref_pic_flag = 1;
        params.intra_pic_flag = c_int::from(intra_only);

        // SAFETY: the union's HEVC member of a zeroed structure.
        let h = unsafe { &mut params.CodecSpecific.hevc };
        h.pic_width_in_luma_samples = c_int::try_from(sps.width).unwrap_or(0);
        h.pic_height_in_luma_samples = c_int::try_from(sps.height).unwrap_or(0);
        h.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3;
        h.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size;
        h.log2_min_transform_block_size_minus2 = sps.log2_min_luma_transform_block_size_minus2;
        h.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_luma_transform_block_size;
        h.pcm_enabled_flag = u8::from(sps.pcm_enabled);
        h.log2_min_pcm_luma_coding_block_size_minus3 =
            sps.log2_min_pcm_luma_coding_block_size_minus3;
        h.log2_diff_max_min_pcm_luma_coding_block_size =
            sps.log2_diff_max_min_pcm_luma_coding_block_size;
        h.pcm_sample_bit_depth_luma_minus1 = sps.pcm_sample_bit_depth_luma_minus1;
        h.pcm_sample_bit_depth_chroma_minus1 = sps.pcm_sample_bit_depth_chroma_minus1;
        h.pcm_loop_filter_disabled_flag = u8::from(sps.pcm_loop_filter_disabled);
        h.strong_intra_smoothing_enabled_flag = u8::from(sps.strong_intra_smoothing_enabled);
        h.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
        h.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
        h.amp_enabled_flag = u8::from(sps.amp_enabled);
        h.separate_colour_plane_flag = u8::from(sps.separate_colour_plane);
        h.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
        h.num_short_term_ref_pic_sets = sps.num_short_term_ref_pic_sets;
        h.long_term_ref_pics_present_flag = u8::from(sps.long_term_ref_pics_present);
        h.num_long_term_ref_pics_sps = sps.num_long_term_ref_pics_sps;
        h.sps_temporal_mvp_enabled_flag = u8::from(sps.temporal_mvp_enabled);
        h.sample_adaptive_offset_enabled_flag = u8::from(sps.sample_adaptive_offset_enabled);
        h.scaling_list_enable_flag = u8::from(sps.scaling_list_enabled);
        h.IrapPicFlag = u8::from(first.header.is_irap());
        h.IdrPicFlag = u8::from(first.header.is_idr());
        h.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
        h.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
        h.log2_max_transform_skip_block_size_minus2 =
            pps.range.log2_max_transform_skip_block_size_minus2;
        h.log2_sao_offset_scale_luma = pps.range.log2_sao_offset_scale_luma;
        h.log2_sao_offset_scale_chroma = pps.range.log2_sao_offset_scale_chroma;
        h.high_precision_offsets_enabled_flag = u8::from(sps.range.high_precision_offsets_enabled);

        h.dependent_slice_segments_enabled_flag = u8::from(pps.dependent_slice_segments_enabled);
        h.slice_segment_header_extension_present_flag =
            u8::from(pps.slice_segment_header_extension_present);
        h.sign_data_hiding_enabled_flag = u8::from(pps.sign_data_hiding_enabled);
        h.cu_qp_delta_enabled_flag = u8::from(pps.cu_qp_delta_enabled);
        h.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth;
        h.init_qp_minus26 = pps.init_qp_minus26;
        h.pps_cb_qp_offset = pps.cb_qp_offset;
        h.pps_cr_qp_offset = pps.cr_qp_offset;
        h.constrained_intra_pred_flag = u8::from(pps.constrained_intra_pred);
        h.weighted_pred_flag = u8::from(pps.weighted_pred);
        h.weighted_bipred_flag = u8::from(pps.weighted_bipred);
        h.transform_skip_enabled_flag = u8::from(pps.transform_skip_enabled);
        h.transquant_bypass_enabled_flag = u8::from(pps.transquant_bypass_enabled);
        h.entropy_coding_sync_enabled_flag = u8::from(pps.entropy_coding_sync_enabled);
        h.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2;
        h.num_extra_slice_header_bits = pps.num_extra_slice_header_bits;
        h.loop_filter_across_tiles_enabled_flag = u8::from(pps.loop_filter_across_tiles_enabled);
        h.loop_filter_across_slices_enabled_flag = u8::from(pps.loop_filter_across_slices_enabled);
        h.output_flag_present_flag = u8::from(pps.output_flag_present);
        h.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        h.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
        h.lists_modification_present_flag = u8::from(pps.lists_modification_present);
        h.cabac_init_present_flag = u8::from(pps.cabac_init_present);
        h.pps_slice_chroma_qp_offsets_present_flag = u8::from(pps.slice_chroma_qp_offsets_present);
        h.deblocking_filter_override_enabled_flag =
            u8::from(pps.deblocking_filter_override_enabled);
        h.pps_deblocking_filter_disabled_flag = u8::from(pps.disable_deblocking_filter);
        h.pps_beta_offset_div2 = pps.beta_offset_div2;
        h.pps_tc_offset_div2 = pps.tc_offset_div2;
        h.tiles_enabled_flag = u8::from(pps.tiles_enabled);
        h.uniform_spacing_flag = u8::from(pps.uniform_spacing);
        h.num_tile_columns_minus1 = pps.num_tile_columns_minus1;
        h.num_tile_rows_minus1 = pps.num_tile_rows_minus1;
        // Explicit column and row sizes; under uniform spacing the device
        // derives them, as the standard does.
        for (dst, src) in h
            .column_width_minus1
            .iter_mut()
            .zip(pps.column_width_minus1.iter())
        {
            *dst = *src;
        }
        for (dst, src) in h
            .row_height_minus1
            .iter_mut()
            .zip(pps.row_height_minus1.iter())
        {
            *dst = *src;
        }

        h.sps_range_extension_flag = u8::from(sps.range.any());
        h.transform_skip_rotation_enabled_flag =
            u8::from(sps.range.transform_skip_rotation_enabled);
        h.transform_skip_context_enabled_flag = u8::from(sps.range.transform_skip_context_enabled);
        h.implicit_rdpcm_enabled_flag = u8::from(sps.range.implicit_rdpcm_enabled);
        h.explicit_rdpcm_enabled_flag = u8::from(sps.range.explicit_rdpcm_enabled);
        h.extended_precision_processing_flag = u8::from(sps.range.extended_precision_processing);
        h.intra_smoothing_disabled_flag = u8::from(sps.range.intra_smoothing_disabled);
        h.persistent_rice_adaptation_enabled_flag =
            u8::from(sps.range.persistent_rice_adaptation_enabled);
        h.cabac_bypass_alignment_enabled_flag = u8::from(sps.range.cabac_bypass_alignment_enabled);
        h.pps_range_extension_flag = u8::from(pps.range != hevc::pps::RangeExtension::default());
        h.cross_component_prediction_enabled_flag =
            u8::from(pps.range.cross_component_prediction_enabled);
        h.chroma_qp_offset_list_enabled_flag = u8::from(pps.range.chroma_qp_offset_list_enabled);
        h.diff_cu_chroma_qp_offset_depth = pps.range.diff_cu_chroma_qp_offset_depth;
        h.chroma_qp_offset_list_len_minus1 = pps.range.chroma_qp_offset_list_len_minus1;
        h.cb_qp_offset_list = pps.range.cb_qp_offset_list;
        h.cr_qp_offset_list = pps.range.cr_qp_offset_list;

        // The reference sets: every reference the buffer holds, then which
        // of them each set names, in the set's own order, which is the
        // order the buffer listed them in.
        h.NumBitsForShortTermRPSInSlice = if first.header.st_rps_from_sps {
            0
        } else {
            c_int::try_from(first.header.st_rps_bits).unwrap_or(0)
        };
        h.NumDeltaPocsOfRefRpsIdx = c_int::from(first.header.st_rps.predicted_from_deltas);
        h.CurrPicOrderCntVal = current.poc;
        for (i, (idx, poc)) in h
            .RefPicIdx
            .iter_mut()
            .zip(h.PicOrderCntVal.iter_mut())
            .enumerate()
        {
            match job.references.as_slice().get(i).copied().flatten() {
                Some(r) => {
                    *idx = c_int::try_from(r.slot).unwrap_or(-1);
                    *poc = r.poc;
                    if let Some(lt) = h.IsLongTerm.get_mut(i) {
                        *lt = u8::from(r.long_term);
                    }
                }
                None => {
                    *idx = -1;
                    *poc = 0;
                }
            }
        }
        let (mut before, mut after, mut long) = (0usize, 0usize, 0usize);
        for (i, r) in job.references.as_slice().iter().enumerate() {
            let Some(r) = r else {
                continue;
            };
            let Ok(i8) = u8::try_from(i) else {
                continue;
            };
            match r.set {
                hevc::dpb::Set::StCurrBefore => {
                    if let Some(s) = h.RefPicSetStCurrBefore.get_mut(before) {
                        *s = i8;
                        before += 1;
                    }
                }
                hevc::dpb::Set::StCurrAfter => {
                    if let Some(s) = h.RefPicSetStCurrAfter.get_mut(after) {
                        *s = i8;
                        after += 1;
                    }
                }
                hevc::dpb::Set::LtCurr => {
                    if let Some(s) = h.RefPicSetLtCurr.get_mut(long) {
                        *s = i8;
                        long += 1;
                    }
                }
                hevc::dpb::Set::Foll => {}
            }
        }
        h.NumPocStCurrBefore = c_int::try_from(before).unwrap_or(0);
        h.NumPocStCurrAfter = c_int::try_from(after).unwrap_or(0);
        h.NumPocLtCurr = c_int::try_from(long).unwrap_or(0);
        h.NumPocTotalCurr = c_int::try_from(before + after + long).unwrap_or(0);

        // The lists are kept in raster order; this device takes the coded
        // (up-right diagonal) order, so each is read back through the scan.
        for (dst, src) in h.ScalingList4x4.iter_mut().zip(pps.scaling.list_4x4.iter()) {
            for (i, at) in DIAG_4X4.iter().enumerate() {
                if let (Some(d), Some(s)) = (dst.get_mut(i), src.get(*at)) {
                    *d = *s;
                }
            }
        }
        for (dst, src) in h.ScalingList8x8.iter_mut().zip(pps.scaling.list_8x8.iter()) {
            for (i, at) in DIAG_8X8.iter().enumerate() {
                if let (Some(d), Some(s)) = (dst.get_mut(i), src.get(*at)) {
                    *d = *s;
                }
            }
        }
        for (dst, src) in h
            .ScalingList16x16
            .iter_mut()
            .zip(pps.scaling.list_16x16.iter())
        {
            for (i, at) in DIAG_8X8.iter().enumerate() {
                if let (Some(d), Some(s)) = (dst.get_mut(i), src.get(*at)) {
                    *d = *s;
                }
            }
        }
        for (dst, src) in h
            .ScalingList32x32
            .iter_mut()
            .zip(pps.scaling.list_32x32.iter())
        {
            for (i, at) in DIAG_8X8.iter().enumerate() {
                if let (Some(d), Some(s)) = (dst.get_mut(i), src.get(*at)) {
                    *d = *s;
                }
            }
        }
        h.ScalingListDCCoeff16x16 = pps.scaling.dc_16x16;
        h.ScalingListDCCoeff32x32 = pps.scaling.dc_32x32;

        let len = self.gather(unit, ranges.get(..slices).unwrap_or(&[]))?;
        Ok((len, slices))
    }

    /// Map `slot`'s picture, copy its planes out, and unmap it.
    fn read_back(&mut self, slot: usize, out: &mut Planes<'_>) -> Result<()> {
        let shape = self.shape.ok_or(Error::NoProfile)?;
        let decoder = self.decoder.as_ref().ok_or(Error::NoProfile)?;
        let format = shape.format();
        let started = lowlat_common::clock::Time::now();
        let mut proc_params: CUVIDPROCPARAMS = zeroed();
        proc_params.progressive_frame = 1;
        let picture = c_int::try_from(slot).map_err(|_| Error::TooLarge)?;
        let (ptr, pitch) = decoder.map(picture, &mut proc_params)?;
        let synced = lowlat_common::clock::Time::now();
        let result = self.copy_planes(ptr, pitch, shape, format, out);
        let unmapped = decoder.unmap(ptr);
        let done = lowlat_common::clock::Time::now();
        self.decode_us = micros(lowlat_common::clock::diff_ms(started, synced));
        self.readback_us = micros(lowlat_common::clock::diff_ms(synced, done));
        result?;
        unmapped?;
        Ok(())
    }

    /// As [`Self::read_back`], to device memory: the mapped picture is
    /// copied plane by plane on this backend's stream and the copies are
    /// waited for, asleep, before the picture is unmapped, so the bytes are
    /// in `out` when this returns and whatever imports that memory may read
    /// them with no fence of its own.
    fn copy_to_device(&mut self, slot: usize, out: &DevicePlanes) -> Result<()> {
        let shape = self.shape.ok_or(Error::NoProfile)?;
        let format = shape.format();
        if self.stream.is_none() {
            self.stream = Some((
                self.cuda.create_stream()?,
                self.cuda.create_waitable_event()?,
            ));
        }
        let (stream, copied) = self.stream.as_ref().ok_or(Error::NoProfile)?;
        let decoder = self.decoder.as_ref().ok_or(Error::NoProfile)?;
        let started = lowlat_common::clock::Time::now();
        let mut proc_params: CUVIDPROCPARAMS = zeroed();
        proc_params.progressive_frame = 1;
        // The map's own work runs on this stream too, so the copies queued
        // behind it are ordered after it.
        proc_params.output_stream = stream.raw();
        let picture = c_int::try_from(slot).map_err(|_| Error::TooLarge)?;
        let (ptr, pitch) = decoder.map(picture, &mut proc_params)?;
        let synced = lowlat_common::clock::Time::now();
        let width = usize::try_from(shape.width).unwrap_or(0);
        let coded_height = usize::try_from(shape.height).unwrap_or(0);
        let row_bytes = width * format.sample();
        let plane = u64::try_from(pitch * coded_height).unwrap_or(0);
        let planes: [(u64, u64, usize, usize); 3] = [
            (ptr, out.y, out.y_pitch, coded_height),
            (
                ptr + plane,
                out.uv,
                out.uv_pitch,
                format.chroma_rows(coded_height),
            ),
            (ptr + 2 * plane, out.v, out.v_pitch, coded_height),
        ];
        let count = if format.full_chroma() { 3 } else { 2 };
        let mut result = Ok(());
        for (src, dst, dst_pitch, rows) in planes.into_iter().take(count) {
            // SAFETY: the mapped picture is `pitch` x the coded height per
            // plane and stays mapped until the unmap below, after the
            // stream is waited for; the destination is the caller's.
            result = unsafe {
                self.cuda
                    .copy_rows_async(src, pitch, dst, dst_pitch, row_bytes, rows, stream)
            };
            if result.is_err() {
                break;
            }
        }
        let waited = copied.record(stream).and_then(|()| copied.wait());
        let unmapped = decoder.unmap(ptr);
        let done = lowlat_common::clock::Time::now();
        self.decode_us = micros(lowlat_common::clock::diff_ms(started, synced));
        self.readback_us = micros(lowlat_common::clock::diff_ms(synced, done));
        result?;
        waited?;
        unmapped?;
        Ok(())
    }

    /// The planes of a mapped picture: luma, then the chroma plane or the
    /// two chroma planes, each `pitch` x the coded height apart.
    fn copy_planes(
        &self,
        ptr: u64,
        pitch: usize,
        shape: Shape,
        format: Format,
        out: &mut Planes<'_>,
    ) -> Result<()> {
        let width = usize::try_from(shape.width).unwrap_or(0);
        let coded_height = usize::try_from(shape.height).unwrap_or(0);
        let row_bytes = width * format.sample();
        let rows_y = coded_height.min(out.y.len() / out.y_pitch.max(1));
        let plane = u64::try_from(pitch * coded_height).map_err(|_| Error::TooLarge)?;
        // SAFETY: the mapped picture is `pitch` x the coded height per
        // plane, live until the unmap after this returns.
        unsafe {
            self.cuda
                .read_rows(ptr, pitch, out.y, out.y_pitch, row_bytes, rows_y)?;
        }
        let chroma_rows = format
            .chroma_rows(coded_height)
            .min(out.uv.len() / out.uv_pitch.max(1));
        // SAFETY: as above; the chroma plane follows the luma plane.
        unsafe {
            self.cuda.read_rows(
                ptr + plane,
                pitch,
                out.uv,
                out.uv_pitch,
                row_bytes,
                chroma_rows,
            )?;
        }
        if format.full_chroma() {
            let rows_v = coded_height.min(out.v.len() / out.v_pitch.max(1));
            // SAFETY: as above; the second chroma plane follows the first.
            unsafe {
                self.cuda.read_rows(
                    ptr + 2 * plane,
                    pitch,
                    out.v,
                    out.v_pitch,
                    row_bytes,
                    rows_v,
                )?;
            }
        }
        Ok(())
    }
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

impl Decoder for Backend<'_> {
    fn build(&mut self, header: &VideoHeader) -> core::result::Result<(), Fault> {
        self.codec = header.codec;
        self.ten_bit = header.ten_bit;
        self.h264.reset();
        self.hevc.reset();
        // The decoder itself waits for the first parameter set, which says
        // the size and the chroma; the header says only the depth.
        Ok(())
    }

    fn feed(&mut self, unit: &[u8]) -> core::result::Result<Fed, Fault> {
        let result = match self.codec {
            Codec::H264 => self.decode_h264(unit),
            Codec::H265 => self.decode_hevc(unit),
        };
        match result {
            Ok(fed) => Ok(fed),
            Err(Error::NoProfile | Error::Runtime(_) | Error::TooLarge) => Err(Fault::Fatal),
            Err(Error::Status(_)) => Err(Fault::Unrecoverable),
        }
    }

    fn take(&mut self, out: &mut Planes<'_>) -> core::result::Result<Option<Picture>, Fault> {
        self.take_with(|this, slot| this.read_back(slot, out))
    }

    fn destroy(&mut self) {
        self.decoder = None;
        self.shape = None;
    }
}

impl Backend<'_> {
    /// As [`Decoder::take`], into device memory the caller allocated:
    /// one device copy in place of the read-back, complete when this
    /// returns.
    pub fn take_to_device(
        &mut self,
        out: &DevicePlanes,
    ) -> core::result::Result<Option<Picture>, Fault> {
        self.take_with(|this, slot| this.copy_to_device(slot, out))
    }

    /// The next picture in output order, moved out of its slot by `copy`
    /// and the slot given back to the picture buffer either way.
    fn take_with(
        &mut self,
        copy: impl FnOnce(&mut Self, usize) -> Result<()>,
    ) -> core::result::Result<Option<Picture>, Fault> {
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
        let copied = copy(self, slot);
        match self.codec {
            Codec::H264 => self.h264.dpb.taken(slot),
            Codec::H265 => self.hevc.dpb.taken(slot),
        }
        copied.map_err(|_| Fault::Unrecoverable)?;
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
        Ok(Some(Picture {
            format: self.format(),
            width,
            height,
            order,
            full_range,
        }))
    }
}
