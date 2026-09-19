//! The HEVC sequence parameter set, and the pieces of syntax the slice
//! header shares with it: the short-term reference picture set and the
//! scaling lists.

use crate::ParseError;
use crate::bits::BitReader;

type Result<T> = core::result::Result<T, ParseError>;

/// Reference picture sets a sequence may define, plus the one a slice may
/// code itself.
pub const MAX_ST_RPS: usize = 65;
/// Pictures a set may name in either direction.
pub const MAX_DELTA_POCS: usize = 16;
const MAX_DELTA_POCS_U32: u32 = 16;
/// Long-term reference candidates a sequence may list.
pub const MAX_LT_SPS: usize = 32;
/// Sub-layers.
const MAX_SUB_LAYERS: usize = 7;

/// A short-term reference picture set, derived: the negative and positive
/// order-count deltas and whether each is used by the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StRps {
    pub num_negative: u8,
    pub num_positive: u8,
    pub delta_s0: [i32; MAX_DELTA_POCS],
    pub used_s0: [bool; MAX_DELTA_POCS],
    pub delta_s1: [i32; MAX_DELTA_POCS],
    pub used_s1: [bool; MAX_DELTA_POCS],
}

impl StRps {
    pub fn num_delta_pocs(&self) -> usize {
        usize::from(self.num_negative) + usize::from(self.num_positive)
    }
}

/// Parse one `st_ref_pic_set(idx)`; `sets` holds those already parsed for
/// inter prediction, and `count` is `num_short_term_ref_pic_sets`.
pub fn st_ref_pic_set(
    r: &mut BitReader<'_>,
    idx: usize,
    count: usize,
    sets: &[StRps],
) -> Result<StRps> {
    let mut out = StRps::default();
    let inter = if idx != 0 { r.flag()? } else { false };
    if inter {
        let delta_idx_minus1 = if idx == count {
            r.ue_max(u32::try_from(idx).unwrap_or(1) - 1)?
        } else {
            0
        };
        let delta_rps_sign = r.flag()?;
        let abs_delta_rps_minus1 = r.ue_max((1 << 15) - 1)?;
        let ref_idx = idx
            .checked_sub(usize::try_from(delta_idx_minus1).unwrap_or(0) + 1)
            .ok_or(ParseError::OutOfRange)?;
        let reference = *sets.get(ref_idx).ok_or(ParseError::OutOfRange)?;
        let delta_rps = (1 - 2 * i32::from(delta_rps_sign))
            * (i32::try_from(abs_delta_rps_minus1).unwrap_or(0) + 1);
        let total = reference.num_delta_pocs();
        let mut used = [false; MAX_DELTA_POCS * 2 + 1];
        let mut use_delta = [true; MAX_DELTA_POCS * 2 + 1];
        for j in 0..=total {
            let u = r.flag()?;
            let d = if u { true } else { r.flag()? };
            if let Some(slot) = used.get_mut(j) {
                *slot = u;
            }
            if let Some(slot) = use_delta.get_mut(j) {
                *slot = d;
            }
        }
        let neg = usize::from(reference.num_negative);
        let pos = usize::from(reference.num_positive);
        let mut i = 0usize;
        let mut push_s0 = |out: &mut StRps, d: i32, u: bool| -> Result<()> {
            *out.delta_s0.get_mut(i).ok_or(ParseError::TooMany)? = d;
            *out.used_s0.get_mut(i).ok_or(ParseError::TooMany)? = u;
            i += 1;
            Ok(())
        };
        for j in (0..pos).rev() {
            let d = reference.delta_s1.get(j).copied().unwrap_or(0) + delta_rps;
            if d < 0 && use_delta.get(neg + j).copied().unwrap_or(false) {
                push_s0(&mut out, d, used.get(neg + j).copied().unwrap_or(false))?;
            }
        }
        if delta_rps < 0 && use_delta.get(total).copied().unwrap_or(false) {
            push_s0(
                &mut out,
                delta_rps,
                used.get(total).copied().unwrap_or(false),
            )?;
        }
        for j in 0..neg {
            let d = reference.delta_s0.get(j).copied().unwrap_or(0) + delta_rps;
            if d < 0 && use_delta.get(j).copied().unwrap_or(false) {
                push_s0(&mut out, d, used.get(j).copied().unwrap_or(false))?;
            }
        }
        out.num_negative = u8::try_from(i).map_err(|_| ParseError::TooMany)?;
        let mut i = 0usize;
        let mut push_s1 = |out: &mut StRps, d: i32, u: bool| -> Result<()> {
            *out.delta_s1.get_mut(i).ok_or(ParseError::TooMany)? = d;
            *out.used_s1.get_mut(i).ok_or(ParseError::TooMany)? = u;
            i += 1;
            Ok(())
        };
        for j in (0..neg).rev() {
            let d = reference.delta_s0.get(j).copied().unwrap_or(0) + delta_rps;
            if d > 0 && use_delta.get(j).copied().unwrap_or(false) {
                push_s1(&mut out, d, used.get(j).copied().unwrap_or(false))?;
            }
        }
        if delta_rps > 0 && use_delta.get(total).copied().unwrap_or(false) {
            push_s1(
                &mut out,
                delta_rps,
                used.get(total).copied().unwrap_or(false),
            )?;
        }
        for j in 0..pos {
            let d = reference.delta_s1.get(j).copied().unwrap_or(0) + delta_rps;
            if d > 0 && use_delta.get(neg + j).copied().unwrap_or(false) {
                push_s1(&mut out, d, used.get(neg + j).copied().unwrap_or(false))?;
            }
        }
        out.num_positive = u8::try_from(i).map_err(|_| ParseError::TooMany)?;
    } else {
        let num_negative = r.ue_max(MAX_DELTA_POCS_U32)?;
        let num_positive = r.ue_max(MAX_DELTA_POCS_U32)?;
        if num_negative + num_positive > MAX_DELTA_POCS_U32 {
            return Err(ParseError::TooMany);
        }
        let mut poc: i32 = 0;
        for i in 0..usize::try_from(num_negative).unwrap_or(0) {
            let delta = r.ue_max((1 << 15) - 1)?;
            poc -= i32::try_from(delta).unwrap_or(0) + 1;
            *out.delta_s0.get_mut(i).ok_or(ParseError::TooMany)? = poc;
            *out.used_s0.get_mut(i).ok_or(ParseError::TooMany)? = r.flag()?;
        }
        let mut poc: i32 = 0;
        for i in 0..usize::try_from(num_positive).unwrap_or(0) {
            let delta = r.ue_max((1 << 15) - 1)?;
            poc += i32::try_from(delta).unwrap_or(0) + 1;
            *out.delta_s1.get_mut(i).ok_or(ParseError::TooMany)? = poc;
            *out.used_s1.get_mut(i).ok_or(ParseError::TooMany)? = r.flag()?;
        }
        out.num_negative = u8::try_from(num_negative).map_err(|_| ParseError::TooMany)?;
        out.num_positive = u8::try_from(num_positive).map_err(|_| ParseError::TooMany)?;
    }
    Ok(out)
}

/// Scaling lists in raster order, which is the order the device takes them
/// in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalingLists {
    pub list_4x4: [[u8; 16]; 6],
    pub list_8x8: [[u8; 64]; 6],
    pub list_16x16: [[u8; 64]; 6],
    pub list_32x32: [[u8; 64]; 2],
    pub dc_16x16: [u8; 6],
    pub dc_32x32: [u8; 2],
}

/// The up-right diagonal scans a coded list arrives in (6.5.3), to raster:
/// the device takes raster.
const DIAG_4X4: [usize; 16] = [0, 4, 1, 8, 5, 2, 12, 9, 6, 3, 13, 10, 7, 14, 11, 15];
const DIAG_8X8: [usize; 64] = [
    0, 8, 1, 16, 9, 2, 24, 17, 10, 3, 32, 25, 18, 11, 4, 40, 33, 26, 19, 12, 5, 48, 41, 34, 27, 20,
    13, 6, 56, 49, 42, 35, 28, 21, 14, 7, 57, 50, 43, 36, 29, 22, 15, 58, 51, 44, 37, 30, 23, 59,
    52, 45, 38, 31, 60, 53, 46, 39, 61, 54, 47, 62, 55, 63,
];

/// Table 7-6, intra then inter, for every size above 4x4, in raster order
/// (the standard lists them in the coded order).
const DEFAULT_INTRA: [u8; 64] = [
    16, 16, 16, 16, 17, 18, 21, 24, 16, 16, 16, 16, 17, 19, 22, 25, 16, 16, 17, 18, 20, 22, 25, 29,
    16, 16, 18, 21, 24, 27, 31, 36, 17, 17, 20, 24, 30, 35, 41, 47, 18, 19, 22, 27, 35, 44, 54, 65,
    21, 22, 25, 31, 41, 54, 70, 88, 24, 25, 29, 36, 47, 65, 88, 115,
];
const DEFAULT_INTER: [u8; 64] = [
    16, 16, 16, 16, 17, 18, 20, 24, 16, 16, 16, 17, 18, 20, 24, 25, 16, 16, 17, 18, 20, 24, 25, 28,
    16, 17, 18, 20, 24, 25, 28, 33, 17, 18, 20, 24, 25, 28, 33, 41, 18, 20, 24, 25, 28, 33, 41, 54,
    20, 24, 25, 28, 33, 41, 54, 71, 24, 25, 28, 33, 41, 54, 71, 91,
];

impl ScalingLists {
    /// The lists the standard uses when a sequence enables scaling but codes
    /// none.
    pub const DEFAULT: Self = Self {
        list_4x4: [[16; 16]; 6],
        list_8x8: [
            DEFAULT_INTRA,
            DEFAULT_INTRA,
            DEFAULT_INTRA,
            DEFAULT_INTER,
            DEFAULT_INTER,
            DEFAULT_INTER,
        ],
        list_16x16: [
            DEFAULT_INTRA,
            DEFAULT_INTRA,
            DEFAULT_INTRA,
            DEFAULT_INTER,
            DEFAULT_INTER,
            DEFAULT_INTER,
        ],
        list_32x32: [DEFAULT_INTRA, DEFAULT_INTER],
        dc_16x16: [16; 6],
        dc_32x32: [16; 2],
    };

    fn list_mut(&mut self, size: usize, matrix: usize) -> Option<&mut [u8]> {
        match size {
            0 => self.list_4x4.get_mut(matrix).map(|l| &mut l[..]),
            1 => self.list_8x8.get_mut(matrix).map(|l| &mut l[..]),
            2 => self.list_16x16.get_mut(matrix).map(|l| &mut l[..]),
            _ => self.list_32x32.get_mut(matrix / 3).map(|l| &mut l[..]),
        }
    }

    fn list(&self, size: usize, matrix: usize) -> Option<[u8; 64]> {
        let mut out = [0u8; 64];
        let source: &[u8] = match size {
            0 => self.list_4x4.get(matrix)?,
            1 => self.list_8x8.get(matrix)?,
            2 => self.list_16x16.get(matrix)?,
            _ => self.list_32x32.get(matrix / 3)?,
        };
        for (o, s) in out.iter_mut().zip(source.iter()) {
            *o = *s;
        }
        Some(out)
    }

    fn set_dc(&mut self, size: usize, matrix: usize, dc: u8) {
        match size {
            2 => {
                if let Some(slot) = self.dc_16x16.get_mut(matrix) {
                    *slot = dc;
                }
            }
            3 => {
                if let Some(slot) = self.dc_32x32.get_mut(matrix / 3) {
                    *slot = dc;
                }
            }
            _ => {}
        }
    }

    fn dc(&self, size: usize, matrix: usize) -> u8 {
        match size {
            2 => self.dc_16x16.get(matrix).copied().unwrap_or(16),
            3 => self.dc_32x32.get(matrix / 3).copied().unwrap_or(16),
            _ => 16,
        }
    }
}

/// Parse `scaling_list_data()`.
pub fn scaling_list_data(r: &mut BitReader<'_>) -> Result<ScalingLists> {
    let mut lists = ScalingLists::DEFAULT;
    for size in 0..4usize {
        let step = if size == 3 { 3 } else { 1 };
        let mut matrix = 0usize;
        while matrix < 6 {
            let pred_mode = r.flag()?;
            if !pred_mode {
                let delta = r.ue_max(if size == 3 { 1 } else { 5 })? as usize;
                if delta == 0 {
                    // The default list for this size and kind.
                    let default: [u8; 64] = if size == 0 {
                        [16; 64]
                    } else if matrix < 3 {
                        DEFAULT_INTRA
                    } else {
                        DEFAULT_INTER
                    };
                    if let Some(list) = lists.list_mut(size, matrix) {
                        for (o, s) in list.iter_mut().zip(default.iter()) {
                            *o = *s;
                        }
                    }
                    lists.set_dc(size, matrix, 16);
                } else {
                    let reference = matrix
                        .checked_sub(delta * step)
                        .ok_or(ParseError::OutOfRange)?;
                    let source = lists.list(size, reference).ok_or(ParseError::OutOfRange)?;
                    let dc = lists.dc(size, reference);
                    if let Some(list) = lists.list_mut(size, matrix) {
                        for (o, s) in list.iter_mut().zip(source.iter()) {
                            *o = *s;
                        }
                    }
                    lists.set_dc(size, matrix, dc);
                }
            } else {
                let mut next: i32 = 8;
                let count = 64usize.min(1 << (4 + (size << 1)));
                if size > 1 {
                    let dc = r.se()?;
                    if !(-7..=247).contains(&dc) {
                        return Err(ParseError::OutOfRange);
                    }
                    next = dc + 8;
                    lists.set_dc(size, matrix, u8::try_from(next).unwrap_or(16));
                }
                for i in 0..count {
                    let delta = r.se()?;
                    if !(-128..=127).contains(&delta) {
                        return Err(ParseError::OutOfRange);
                    }
                    next = (next + delta + 256).rem_euclid(256);
                    let at = if size == 0 {
                        DIAG_4X4.get(i).copied().unwrap_or(0)
                    } else {
                        DIAG_8X8.get(i).copied().unwrap_or(0)
                    };
                    if let Some(slot) = lists.list_mut(size, matrix).and_then(|l| l.get_mut(at)) {
                        *slot = u8::try_from(next).unwrap_or(16);
                    }
                }
            }
            matrix += step;
        }
    }
    Ok(lists)
}

/// `profile_tier_level`, read for the profile and otherwise skipped.
fn profile_tier_level(r: &mut BitReader<'_>, max_sub_layers_minus1: u32) -> Result<(u8, u32)> {
    r.skip(2)?; // general_profile_space
    r.flag()?; // general_tier_flag
    let profile_idc = r.u8(5)?;
    let compatibility = r.bits(32)?;
    r.skip(4)?; // progressive, interlaced, non_packed, frame_only
    r.skip(32)?; // 43 reserved bits plus the inbld flag, in two halves
    r.skip(12)?;
    r.skip(8)?; // general_level_idc
    let mut profile_present = [false; MAX_SUB_LAYERS];
    let mut level_present = [false; MAX_SUB_LAYERS];
    let sub_layers = usize::try_from(max_sub_layers_minus1)
        .unwrap_or(0)
        .min(MAX_SUB_LAYERS);
    for (profile, level) in profile_present
        .iter_mut()
        .zip(level_present.iter_mut())
        .take(sub_layers)
    {
        *profile = r.flag()?;
        *level = r.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2)?;
        }
    }
    for (profile, level) in profile_present
        .iter()
        .zip(level_present.iter())
        .take(sub_layers)
    {
        if *profile {
            r.skip(32)?;
            r.skip(32)?;
            r.skip(24)?;
        }
        if *level {
            r.skip(8)?;
        }
    }
    Ok((profile_idc, compatibility))
}

/// `sps_range_extension()`: the coding tools of the range-extensions
/// profiles, every one of which the device is told about. All clear for a
/// sequence that carries none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RangeExtension {
    pub transform_skip_rotation_enabled: bool,
    pub transform_skip_context_enabled: bool,
    pub implicit_rdpcm_enabled: bool,
    pub explicit_rdpcm_enabled: bool,
    pub extended_precision_processing: bool,
    pub intra_smoothing_disabled: bool,
    pub high_precision_offsets_enabled: bool,
    pub persistent_rice_adaptation_enabled: bool,
    pub cabac_bypass_alignment_enabled: bool,
}

impl RangeExtension {
    pub fn any(&self) -> bool {
        self.transform_skip_rotation_enabled
            || self.transform_skip_context_enabled
            || self.implicit_rdpcm_enabled
            || self.explicit_rdpcm_enabled
            || self.extended_precision_processing
            || self.intra_smoothing_disabled
            || self.high_precision_offsets_enabled
            || self.persistent_rice_adaptation_enabled
            || self.cabac_bypass_alignment_enabled
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sps {
    pub id: u8,
    pub vps_id: u8,
    pub max_sub_layers_minus1: u8,
    pub profile_idc: u8,
    pub profile_compatibility: u32,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub width: u32,
    pub height: u32,
    pub conformance_window: Option<[u32; 4]>,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    /// Per sub-layer, the highest is what the decoder uses.
    pub max_dec_pic_buffering_minus1: u8,
    pub max_num_reorder_pics: u8,
    pub max_latency_increase_plus1: u32,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_luma_transform_block_size_minus2: u8,
    pub log2_diff_max_min_luma_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled: bool,
    /// The sequence's lists when enabled: coded, or the defaults.
    pub scaling: ScalingLists,
    pub amp_enabled: bool,
    pub sample_adaptive_offset_enabled: bool,
    pub pcm_enabled: bool,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub pcm_loop_filter_disabled: bool,
    pub num_short_term_ref_pic_sets: u8,
    pub st_rps: [StRps; MAX_ST_RPS],
    pub long_term_ref_pics_present: bool,
    pub num_long_term_ref_pics_sps: u8,
    pub lt_ref_pic_poc_lsb_sps: [u32; MAX_LT_SPS],
    pub used_by_curr_pic_lt_sps: [bool; MAX_LT_SPS],
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing_enabled: bool,
    pub range: RangeExtension,
}

impl Sps {
    /// Whether the sequence is of the range-extensions profile in any way a
    /// device has to be told about: full chroma, or any of its tools.
    pub fn is_range_extended(&self) -> bool {
        self.chroma_format_idc == 3 || self.range.any()
    }

    pub fn max_pic_order_cnt_lsb(&self) -> u32 {
        1 << (u32::from(self.log2_max_pic_order_cnt_lsb_minus4) + 4)
    }

    /// The visible picture after the conformance window.
    pub fn visible(&self) -> (u32, u32) {
        let Some([left, right, top, bottom]) = self.conformance_window else {
            return (self.width, self.height);
        };
        let (sub_w, sub_h) = match self.chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let w = self
            .width
            .saturating_sub((left + right).saturating_mul(sub_w));
        let h = self
            .height
            .saturating_sub((top + bottom).saturating_mul(sub_h));
        (w.max(1), h.max(1))
    }

    pub fn ctb_log2_size(&self) -> u32 {
        u32::from(self.log2_min_luma_coding_block_size_minus3)
            + 3
            + u32::from(self.log2_diff_max_min_luma_coding_block_size)
    }

    /// Coding tree blocks across and down.
    pub fn ctbs(&self) -> (u32, u32) {
        let size = 1u32 << self.ctb_log2_size();
        (self.width.div_ceil(size), self.height.div_ceil(size))
    }

    pub fn pic_size_in_ctbs(&self) -> u32 {
        let (w, h) = self.ctbs();
        w * h
    }
}

fn small(value: u32, max: u32) -> Result<u8> {
    if value > max {
        return Err(ParseError::OutOfRange);
    }
    u8::try_from(value).map_err(|_| ParseError::OutOfRange)
}

/// Parse a sequence parameter set from its payload (after the two header
/// bytes).
pub fn parse(payload: &[u8]) -> Result<Sps> {
    let mut r = BitReader::new(payload);
    let vps_id = r.u8(4)?;
    let max_sub_layers_minus1 = r.u8(3)?;
    if max_sub_layers_minus1 > 6 {
        return Err(ParseError::OutOfRange);
    }
    r.flag()?; // sps_temporal_id_nesting_flag
    let (profile_idc, profile_compatibility) =
        profile_tier_level(&mut r, u32::from(max_sub_layers_minus1))?;
    let id = small(r.ue()?, 15)?;
    let chroma_format_idc = small(r.ue()?, 3)?;
    let separate_colour_plane = if chroma_format_idc == 3 {
        r.flag()?
    } else {
        false
    };
    let width = r.ue_max(16888)?;
    let height = r.ue_max(16888)?;
    if width == 0 || height == 0 {
        return Err(ParseError::OutOfRange);
    }
    let conformance_window = if r.flag()? {
        Some([r.ue()?, r.ue()?, r.ue()?, r.ue()?])
    } else {
        None
    };
    let bit_depth_luma_minus8 = small(r.ue()?, 8)?;
    let bit_depth_chroma_minus8 = small(r.ue()?, 8)?;
    let log2_max_pic_order_cnt_lsb_minus4 = small(r.ue()?, 12)?;
    let sub_layer_ordering_info_present = r.flag()?;
    let mut max_dec_pic_buffering_minus1 = 0u8;
    let mut max_num_reorder_pics = 0u8;
    let mut max_latency_increase_plus1 = 0u32;
    let first = if sub_layer_ordering_info_present {
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in first..=max_sub_layers_minus1 {
        // The highest sub-layer's values are read last and win.
        max_dec_pic_buffering_minus1 = small(r.ue()?, 15)?;
        max_num_reorder_pics = small(r.ue()?, u32::from(max_dec_pic_buffering_minus1))?;
        max_latency_increase_plus1 = r.ue()?;
    }
    let log2_min_luma_coding_block_size_minus3 = small(r.ue()?, 3)?;
    let log2_diff_max_min_luma_coding_block_size = small(r.ue()?, 3)?;
    let log2_min_luma_transform_block_size_minus2 = small(r.ue()?, 3)?;
    let log2_diff_max_min_luma_transform_block_size = small(r.ue()?, 3)?;
    let max_transform_hierarchy_depth_inter = small(r.ue()?, 4)?;
    let max_transform_hierarchy_depth_intra = small(r.ue()?, 4)?;
    let scaling_list_enabled = r.flag()?;
    let mut scaling = ScalingLists::DEFAULT;
    if scaling_list_enabled && r.flag()? {
        scaling = scaling_list_data(&mut r)?;
    }
    let amp_enabled = r.flag()?;
    let sample_adaptive_offset_enabled = r.flag()?;
    let pcm_enabled = r.flag()?;
    let mut pcm_sample_bit_depth_luma_minus1 = 0;
    let mut pcm_sample_bit_depth_chroma_minus1 = 0;
    let mut log2_min_pcm_luma_coding_block_size_minus3 = 0;
    let mut log2_diff_max_min_pcm_luma_coding_block_size = 0;
    let mut pcm_loop_filter_disabled = false;
    if pcm_enabled {
        pcm_sample_bit_depth_luma_minus1 = r.u8(4)?;
        pcm_sample_bit_depth_chroma_minus1 = r.u8(4)?;
        log2_min_pcm_luma_coding_block_size_minus3 = small(r.ue()?, 2)?;
        log2_diff_max_min_pcm_luma_coding_block_size = small(r.ue()?, 2)?;
        pcm_loop_filter_disabled = r.flag()?;
    }
    let num_short_term_ref_pic_sets = small(r.ue()?, 64)?;
    let mut st_rps = [StRps::default(); MAX_ST_RPS];
    for i in 0..usize::from(num_short_term_ref_pic_sets) {
        let set = st_ref_pic_set(&mut r, i, usize::from(num_short_term_ref_pic_sets), &st_rps)?;
        *st_rps.get_mut(i).ok_or(ParseError::TooMany)? = set;
    }
    let long_term_ref_pics_present = r.flag()?;
    let mut num_long_term_ref_pics_sps = 0u8;
    let mut lt_ref_pic_poc_lsb_sps = [0u32; MAX_LT_SPS];
    let mut used_by_curr_pic_lt_sps = [false; MAX_LT_SPS];
    if long_term_ref_pics_present {
        num_long_term_ref_pics_sps = small(r.ue()?, 32)?;
        for i in 0..usize::from(num_long_term_ref_pics_sps) {
            *lt_ref_pic_poc_lsb_sps
                .get_mut(i)
                .ok_or(ParseError::TooMany)? =
                r.bits(u32::from(log2_max_pic_order_cnt_lsb_minus4) + 4)?;
            *used_by_curr_pic_lt_sps
                .get_mut(i)
                .ok_or(ParseError::TooMany)? = r.flag()?;
        }
    }
    let temporal_mvp_enabled = r.flag()?;
    let strong_intra_smoothing_enabled = r.flag()?;
    // The VUI is walked, not kept: it stands between here and the
    // extensions, which the device has to be told about.
    if r.flag()? {
        vui_parameters(&mut r, u32::from(max_sub_layers_minus1))?;
    }
    let mut range = RangeExtension::default();
    if r.flag()? {
        // sps_extension_present: the range extension is read; the
        // multilayer, 3D and screen-content ones are refused, as no device
        // here decodes them and their syntax would follow.
        let has_range = r.flag()?;
        let multilayer = r.flag()?;
        let three_d = r.flag()?;
        let scc = r.flag()?;
        let four_bits = r.bits(4)?;
        if multilayer || three_d || scc || four_bits != 0 {
            return Err(ParseError::Unsupported);
        }
        if has_range {
            range = RangeExtension {
                transform_skip_rotation_enabled: r.flag()?,
                transform_skip_context_enabled: r.flag()?,
                implicit_rdpcm_enabled: r.flag()?,
                explicit_rdpcm_enabled: r.flag()?,
                extended_precision_processing: r.flag()?,
                intra_smoothing_disabled: r.flag()?,
                high_precision_offsets_enabled: r.flag()?,
                persistent_rice_adaptation_enabled: r.flag()?,
                cabac_bypass_alignment_enabled: r.flag()?,
            };
        }
    }

    Ok(Sps {
        id,
        vps_id,
        max_sub_layers_minus1,
        profile_idc,
        profile_compatibility,
        chroma_format_idc,
        separate_colour_plane,
        width,
        height,
        conformance_window,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb_minus4,
        max_dec_pic_buffering_minus1,
        max_num_reorder_pics,
        max_latency_increase_plus1,
        log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size,
        log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_luma_transform_block_size,
        max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra,
        scaling_list_enabled,
        scaling,
        amp_enabled,
        sample_adaptive_offset_enabled,
        pcm_enabled,
        pcm_sample_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1,
        log2_min_pcm_luma_coding_block_size_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size,
        pcm_loop_filter_disabled,
        num_short_term_ref_pic_sets,
        st_rps,
        long_term_ref_pics_present,
        num_long_term_ref_pics_sps,
        lt_ref_pic_poc_lsb_sps,
        used_by_curr_pic_lt_sps,
        temporal_mvp_enabled,
        strong_intra_smoothing_enabled,
        range,
    })
}

/// Walk `vui_parameters()` to its end. Nothing in it decodes a picture;
/// it is read only because the extensions follow it.
fn vui_parameters(r: &mut BitReader<'_>, max_sub_layers_minus1: u32) -> Result<()> {
    if r.flag()? {
        // aspect_ratio_info_present
        if r.u8(8)? == 255 {
            r.skip(32)?; // sar_width, sar_height
        }
    }
    if r.flag()? {
        r.flag()?; // overscan_appropriate
    }
    if r.flag()? {
        // video_signal_type_present
        r.skip(4)?; // video_format, video_full_range
        if r.flag()? {
            r.skip(24)?; // colour_primaries, transfer, matrix_coeffs
        }
    }
    if r.flag()? {
        // chroma_loc_info_present
        r.ue()?;
        r.ue()?;
    }
    r.skip(3)?; // neutral_chroma_indication, field_seq, frame_field_info_present
    if r.flag()? {
        // default_display_window
        for _ in 0..4 {
            r.ue()?;
        }
    }
    if r.flag()? {
        // vui_timing_info_present
        r.skip(32)?; // num_units_in_tick
        r.skip(32)?; // time_scale
        if r.flag()? {
            r.ue()?; // num_ticks_poc_diff_one_minus1
        }
        if r.flag()? {
            hrd_parameters(r, true, max_sub_layers_minus1)?;
        }
    }
    if r.flag()? {
        // bitstream_restriction
        r.skip(3)?; // tiles_fixed_structure, mvs_over_pic_boundaries, restricted_ref_pic_lists
        for _ in 0..5 {
            r.ue()?;
        }
    }
    Ok(())
}

/// Walk `hrd_parameters()` to its end.
fn hrd_parameters(r: &mut BitReader<'_>, common: bool, max_sub_layers_minus1: u32) -> Result<()> {
    let mut nal = false;
    let mut vcl = false;
    let mut sub_pic = false;
    if common {
        nal = r.flag()?;
        vcl = r.flag()?;
        if nal || vcl {
            sub_pic = r.flag()?;
            if sub_pic {
                r.skip(8)?; // tick_divisor_minus2
                r.skip(5)?; // du_cpb_removal_delay_increment_length_minus1
                r.skip(1)?; // sub_pic_cpb_params_in_pic_timing_sei
                r.skip(5)?; // dpb_output_delay_du_length_minus1
            }
            r.skip(8)?; // bit_rate_scale, cpb_size_scale
            if sub_pic {
                r.skip(4)?; // cpb_size_du_scale
            }
            r.skip(15)?; // the three delay lengths, 5 bits each
        }
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_general = r.flag()?;
        let fixed_within_cvs = if fixed_general { true } else { r.flag()? };
        let mut low_delay = false;
        if fixed_within_cvs {
            r.ue()?; // elemental_duration_in_tc_minus1
        } else {
            low_delay = r.flag()?;
        }
        let cpb_cnt_minus1 = if low_delay { 0 } else { r.ue_max(31)? };
        for present in [nal, vcl] {
            if !present {
                continue;
            }
            for _ in 0..=cpb_cnt_minus1 {
                r.ue()?; // bit_rate_value_minus1
                r.ue()?; // cpb_size_value_minus1
                if sub_pic {
                    r.ue()?; // cpb_size_du_value_minus1
                    r.ue()?; // bit_rate_du_value_minus1
                }
                r.flag()?; // cbr
            }
        }
    }
    Ok(())
}
