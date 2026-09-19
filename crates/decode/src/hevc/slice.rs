//! The HEVC slice segment header.

use crate::ParseError;
use crate::bits::BitReader;
use crate::h264::slice::List;
use crate::hevc::pps::Pps;
use crate::hevc::sps::{MAX_LT_SPS, Sps, StRps, st_ref_pic_set};

type Result<T> = core::result::Result<T, ParseError>;

/// Reference indices a list may hold.
pub const MAX_REFS: usize = 15;
/// Long-term pictures a slice may name.
pub const MAX_LT: usize = MAX_LT_SPS;
const MAX_LT_U32: u32 = 32;

/// Unit types with a name the reader acts on.
pub mod unit {
    pub const TRAIL_N: u8 = 0;
    pub const TRAIL_R: u8 = 1;
    pub const TSA_N: u8 = 2;
    pub const STSA_N: u8 = 4;
    pub const RADL_N: u8 = 6;
    pub const RADL_R: u8 = 7;
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const RSV_VCL_N14: u8 = 14;
    pub const BLA_W_LP: u8 = 16;
    pub const BLA_N_LP: u8 = 18;
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA: u8 = 21;
    pub const RSV_IRAP_VCL23: u8 = 23;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
    pub const EOS: u8 = 36;
    pub const EOB: u8 = 37;
    pub const PREFIX_SEI: u8 = 39;
    pub const SUFFIX_SEI: u8 = 40;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    B,
    P,
    I,
}

impl SliceType {
    pub fn code(self) -> u8 {
        match self {
            Self::B => 0,
            Self::P => 1,
            Self::I => 2,
        }
    }

    pub fn is_intra(self) -> bool {
        matches!(self, Self::I)
    }

    pub fn is_b(self) -> bool {
        matches!(self, Self::B)
    }
}

/// A long-term reference the slice names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LongTerm {
    pub poc_lsb: u32,
    pub used_by_curr_pic: bool,
    pub delta_poc_msb_present: bool,
    /// Accumulated `delta_poc_msb_cycle_lt`, as the standard sums it.
    pub delta_poc_msb_cycle: u32,
}

/// Explicit weights for one list, as the device takes them (deltas, not
/// resolved weights).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    pub delta_luma_weight: [i8; MAX_REFS],
    /// Sixteen bits wide for the high-precision offsets of the range
    /// extensions; eight suffice otherwise.
    pub luma_offset: [i16; MAX_REFS],
    pub delta_chroma_weight: [[i8; 2]; MAX_REFS],
    pub chroma_offset: [[i16; 2]; MAX_REFS],
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            delta_luma_weight: [0; MAX_REFS],
            luma_offset: [0; MAX_REFS],
            delta_chroma_weight: [[0; 2]; MAX_REFS],
            chroma_offset: [[0; 2]; MAX_REFS],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PredWeights {
    pub luma_log2_denom: u8,
    pub delta_chroma_log2_denom: i8,
    pub l0: Weights,
    pub l1: Weights,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceHeader {
    pub nal_unit_type: u8,
    pub temporal_id: u8,
    pub first_slice_segment_in_pic: bool,
    pub no_output_of_prior_pics: bool,
    pub pps_id: u8,
    pub dependent_slice_segment: bool,
    pub slice_segment_address: u32,
    pub slice_type: SliceType,
    pub pic_output: bool,
    pub colour_plane_id: u8,
    pub pic_order_cnt_lsb: u32,
    /// The short-term set in force: the sequence's by index, or the one
    /// coded in the header.
    pub st_rps: StRps,
    pub st_rps_from_sps: bool,
    /// Bits the header's own set took, for the device.
    pub st_rps_bits: u32,
    pub long_terms: List<LongTerm, MAX_LT>,
    pub temporal_mvp_enabled: bool,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub list_modification_l0: Option<[u8; MAX_REFS]>,
    pub list_modification_l1: Option<[u8; MAX_REFS]>,
    pub mvd_l1_zero: bool,
    pub cabac_init: bool,
    pub collocated_from_l0: bool,
    pub collocated_ref_idx: u8,
    pub weights: Option<PredWeights>,
    pub five_minus_max_num_merge_cand: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    /// The range extensions' per-unit chroma offsets are in force.
    pub cu_chroma_qp_offset_enabled: bool,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i8,
    pub tc_offset_div2: i8,
    pub loop_filter_across_slices_enabled: bool,
    pub num_entry_point_offsets: u32,
    /// Bytes of header, the two header bytes included, escapes excluded.
    pub header_bytes: u32,
    /// Escape bytes inside the header.
    pub header_escapes: u32,
}

impl SliceHeader {
    pub fn is_irap(&self) -> bool {
        (unit::BLA_W_LP..=unit::RSV_IRAP_VCL23).contains(&self.nal_unit_type)
    }

    pub fn is_idr(&self) -> bool {
        matches!(self.nal_unit_type, unit::IDR_W_RADL | unit::IDR_N_LP)
    }

    pub fn is_bla(&self) -> bool {
        (unit::BLA_W_LP..=unit::BLA_N_LP).contains(&self.nal_unit_type)
    }

    pub fn is_cra(&self) -> bool {
        self.nal_unit_type == unit::CRA
    }

    pub fn is_rasl(&self) -> bool {
        matches!(self.nal_unit_type, unit::RASL_N | unit::RASL_R)
    }

    pub fn is_radl(&self) -> bool {
        matches!(self.nal_unit_type, unit::RADL_N | unit::RADL_R)
    }

    /// A sub-layer non-reference picture: even-numbered types below 16.
    pub fn is_sub_layer_non_reference(&self) -> bool {
        self.nal_unit_type <= unit::RSV_VCL_N14 && self.nal_unit_type % 2 == 0
    }

    /// Pictures the current one may reference: what its sets mark as used.
    pub fn num_pic_total_curr(&self) -> u32 {
        let st = self
            .st_rps
            .used_s0
            .iter()
            .take(usize::from(self.st_rps.num_negative))
            .filter(|u| **u)
            .count()
            + self
                .st_rps
                .used_s1
                .iter()
                .take(usize::from(self.st_rps.num_positive))
                .filter(|u| **u)
                .count();
        let lt = self
            .long_terms
            .as_slice()
            .iter()
            .filter(|l| l.used_by_curr_pic)
            .count();
        u32::try_from(st + lt).unwrap_or(u32::MAX)
    }
}

fn small(value: u32, max: u32) -> Result<u8> {
    if value > max {
        return Err(ParseError::OutOfRange);
    }
    u8::try_from(value).map_err(|_| ParseError::OutOfRange)
}

fn small_signed(value: i32, min: i32, max: i32) -> Result<i8> {
    if value < min || value > max {
        return Err(ParseError::OutOfRange);
    }
    i8::try_from(value).map_err(|_| ParseError::OutOfRange)
}

/// Bits needed to code values below `n`.
fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

/// `half_range` is `WpOffsetHalfRange`: 128, or half the sample range
/// under the high-precision offsets of the range extensions.
fn weights(
    r: &mut BitReader<'_>,
    count: usize,
    chroma: bool,
    chroma_log2_denom: u32,
    half_range: i32,
) -> Result<Weights> {
    let mut w = Weights::default();
    let mut luma_flags = [false; MAX_REFS];
    let mut chroma_flags = [false; MAX_REFS];
    for flag in luma_flags.iter_mut().take(count) {
        *flag = r.flag()?;
    }
    if chroma {
        for flag in chroma_flags.iter_mut().take(count) {
            *flag = r.flag()?;
        }
    }
    for i in 0..count.min(MAX_REFS) {
        if luma_flags.get(i).copied().unwrap_or(false) {
            let dw = small_signed(r.se()?, -128, 127)?;
            let o = r.se()?;
            if o < -half_range || o >= half_range {
                return Err(ParseError::OutOfRange);
            }
            if let Some(slot) = w.delta_luma_weight.get_mut(i) {
                *slot = dw;
            }
            if let Some(slot) = w.luma_offset.get_mut(i) {
                *slot = i16::try_from(o).map_err(|_| ParseError::OutOfRange)?;
            }
        }
        if chroma_flags.get(i).copied().unwrap_or(false) {
            for j in 0..2 {
                let dw = small_signed(r.se()?, -128, 127)?;
                let delta_offset = r.se()?;
                if delta_offset < -4 * half_range || delta_offset >= 4 * half_range {
                    return Err(ParseError::OutOfRange);
                }
                if let Some(slot) = w.delta_chroma_weight.get_mut(i).and_then(|s| s.get_mut(j)) {
                    *slot = dw;
                }
                // 7-56: the offset the delta stands for, which is what the
                // device takes.
                let weight = (1i32 << chroma_log2_denom) + i32::from(dw);
                let offset = (half_range + delta_offset
                    - ((half_range * weight) >> chroma_log2_denom))
                    .clamp(-half_range, half_range - 1);
                if let Some(slot) = w.chroma_offset.get_mut(i).and_then(|s| s.get_mut(j)) {
                    *slot = i16::try_from(offset).unwrap_or(0);
                }
            }
        }
    }
    Ok(w)
}

/// Parse a slice segment header. `unit` is the whole unit, both header
/// bytes included. `first_in_stream` says whether a random-access picture
/// starts the stream, which decides what its leading pictures are.
pub fn parse(
    unit: &[u8],
    pps_of: impl Fn(u8) -> Option<Pps>,
    sps_of: impl Fn(u8) -> Option<Sps>,
    previous: Option<&SliceHeader>,
) -> Result<SliceHeader> {
    let (&first, rest) = unit.split_first().ok_or(ParseError::Truncated)?;
    let (&second, payload) = rest.split_first().ok_or(ParseError::Truncated)?;
    let nal_unit_type = (first >> 1) & 0x3F;
    let layer_id = ((first & 1) << 5) | (second >> 3);
    if layer_id != 0 {
        return Err(ParseError::Unsupported);
    }
    let temporal_id = (second & 0x07)
        .checked_sub(1)
        .ok_or(ParseError::OutOfRange)?;
    let mut r = BitReader::new(payload);

    let first_slice_segment_in_pic = r.flag()?;
    let mut no_output_of_prior_pics = false;
    if (unit::BLA_W_LP..=unit::RSV_IRAP_VCL23).contains(&nal_unit_type) {
        no_output_of_prior_pics = r.flag()?;
    }
    let pps_id = small(r.ue()?, 63)?;
    let pps = pps_of(pps_id).ok_or(ParseError::NoParameterSet)?;
    let sps = sps_of(pps.sps_id).ok_or(ParseError::NoParameterSet)?;

    let mut dependent_slice_segment = false;
    let mut slice_segment_address = 0;
    if !first_slice_segment_in_pic {
        if pps.dependent_slice_segments_enabled {
            dependent_slice_segment = r.flag()?;
        }
        let bits = ceil_log2(sps.pic_size_in_ctbs());
        slice_segment_address = r.bits(bits)?;
        if slice_segment_address >= sps.pic_size_in_ctbs() {
            return Err(ParseError::OutOfRange);
        }
    }

    // A dependent segment takes its values from the independent one before
    // it; only the entry points and the address are its own.
    let mut header = match previous {
        Some(previous) if dependent_slice_segment => SliceHeader {
            nal_unit_type,
            temporal_id,
            first_slice_segment_in_pic,
            no_output_of_prior_pics,
            pps_id,
            dependent_slice_segment,
            slice_segment_address,
            num_entry_point_offsets: 0,
            header_bytes: 0,
            header_escapes: 0,
            ..*previous
        },
        _ if dependent_slice_segment => return Err(ParseError::OutOfRange),
        _ => SliceHeader {
            nal_unit_type,
            temporal_id,
            first_slice_segment_in_pic,
            no_output_of_prior_pics,
            pps_id,
            dependent_slice_segment,
            slice_segment_address,
            slice_type: SliceType::I,
            pic_output: true,
            colour_plane_id: 0,
            pic_order_cnt_lsb: 0,
            st_rps: StRps::default(),
            st_rps_from_sps: false,
            st_rps_bits: 0,
            long_terms: List::default(),
            temporal_mvp_enabled: false,
            sao_luma: false,
            sao_chroma: false,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            list_modification_l0: None,
            list_modification_l1: None,
            mvd_l1_zero: false,
            cabac_init: false,
            collocated_from_l0: true,
            collocated_ref_idx: 0,
            weights: None,
            five_minus_max_num_merge_cand: 0,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            cu_chroma_qp_offset_enabled: false,
            deblocking_filter_disabled: pps.disable_deblocking_filter,
            beta_offset_div2: pps.beta_offset_div2,
            tc_offset_div2: pps.tc_offset_div2,
            loop_filter_across_slices_enabled: pps.loop_filter_across_slices_enabled,
            num_entry_point_offsets: 0,
            header_bytes: 0,
            header_escapes: 0,
        },
    };

    if !dependent_slice_segment {
        r.skip(u32::from(pps.num_extra_slice_header_bits))?;
        header.slice_type = match r.ue_max(2)? {
            0 => SliceType::B,
            1 => SliceType::P,
            _ => SliceType::I,
        };
        if pps.output_flag_present {
            header.pic_output = r.flag()?;
        }
        if sps.separate_colour_plane {
            header.colour_plane_id = r.u8(2)?;
        }
        let idr = matches!(nal_unit_type, unit::IDR_W_RADL | unit::IDR_N_LP);
        if !idr {
            header.pic_order_cnt_lsb =
                r.bits(u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4)?;
            let from_sps = r.flag()?;
            header.st_rps_from_sps = from_sps;
            let count = usize::from(sps.num_short_term_ref_pic_sets);
            if !from_sps {
                let before = r.position();
                header.st_rps = st_ref_pic_set(&mut r, count, count, &sps.st_rps)?;
                header.st_rps_bits = u32::try_from(r.position() - before).unwrap_or(0);
            } else {
                let idx = if count > 1 {
                    usize::try_from(r.bits(ceil_log2(u32::try_from(count).unwrap_or(1)))?)
                        .unwrap_or(0)
                } else {
                    0
                };
                header.st_rps = *sps.st_rps.get(idx).ok_or(ParseError::OutOfRange)?;
                if idx >= count {
                    return Err(ParseError::OutOfRange);
                }
            }
            if sps.long_term_ref_pics_present {
                let num_long_term_sps = if sps.num_long_term_ref_pics_sps > 0 {
                    r.ue_max(u32::from(sps.num_long_term_ref_pics_sps))?
                } else {
                    0
                };
                let num_long_term_pics = r.ue_max(MAX_LT_U32)?;
                let total = num_long_term_sps + num_long_term_pics;
                if total > MAX_LT_U32 {
                    return Err(ParseError::TooMany);
                }
                let mut previous_cycle = 0u32;
                for i in 0..total {
                    let mut lt = LongTerm::default();
                    if i < num_long_term_sps {
                        let idx = if sps.num_long_term_ref_pics_sps > 1 {
                            usize::try_from(
                                r.bits(ceil_log2(u32::from(sps.num_long_term_ref_pics_sps)))?,
                            )
                            .unwrap_or(0)
                        } else {
                            0
                        };
                        lt.poc_lsb = *sps
                            .lt_ref_pic_poc_lsb_sps
                            .get(idx)
                            .ok_or(ParseError::OutOfRange)?;
                        lt.used_by_curr_pic = *sps
                            .used_by_curr_pic_lt_sps
                            .get(idx)
                            .ok_or(ParseError::OutOfRange)?;
                    } else {
                        lt.poc_lsb =
                            r.bits(u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4)?;
                        lt.used_by_curr_pic = r.flag()?;
                    }
                    lt.delta_poc_msb_present = r.flag()?;
                    if lt.delta_poc_msb_present {
                        let cycle = r.ue()?;
                        // 7-52: the cycle accumulates within each of the
                        // two groups.
                        if i == 0 || i == num_long_term_sps {
                            lt.delta_poc_msb_cycle = cycle;
                        } else {
                            lt.delta_poc_msb_cycle = cycle.wrapping_add(previous_cycle);
                        }
                        previous_cycle = lt.delta_poc_msb_cycle;
                    } else if i == 0 || i == num_long_term_sps {
                        previous_cycle = 0;
                    }
                    header.long_terms.push(lt)?;
                }
            }
            if sps.temporal_mvp_enabled {
                header.temporal_mvp_enabled = r.flag()?;
            }
        }
        if sps.sample_adaptive_offset_enabled {
            header.sao_luma = r.flag()?;
            if sps.chroma_format_idc != 0 {
                header.sao_chroma = r.flag()?;
            }
        }
        if !header.slice_type.is_intra() {
            header.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
            header.num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
            if r.flag()? {
                header.num_ref_idx_l0_active_minus1 = small(r.ue()?, 14)?;
                if header.slice_type.is_b() {
                    header.num_ref_idx_l1_active_minus1 = small(r.ue()?, 14)?;
                }
            }
            let total_curr = header.num_pic_total_curr();
            if pps.lists_modification_present && total_curr > 1 {
                let bits = ceil_log2(total_curr);
                if r.flag()? {
                    let mut entries = [0u8; MAX_REFS];
                    for e in entries
                        .iter_mut()
                        .take(usize::from(header.num_ref_idx_l0_active_minus1) + 1)
                    {
                        *e = small(r.bits(bits)?, total_curr - 1)?;
                    }
                    header.list_modification_l0 = Some(entries);
                }
                if header.slice_type.is_b() && r.flag()? {
                    let mut entries = [0u8; MAX_REFS];
                    for e in entries
                        .iter_mut()
                        .take(usize::from(header.num_ref_idx_l1_active_minus1) + 1)
                    {
                        *e = small(r.bits(bits)?, total_curr - 1)?;
                    }
                    header.list_modification_l1 = Some(entries);
                }
            }
            if header.slice_type.is_b() {
                header.mvd_l1_zero = r.flag()?;
            }
            if pps.cabac_init_present {
                header.cabac_init = r.flag()?;
            }
            if header.temporal_mvp_enabled {
                if header.slice_type.is_b() {
                    header.collocated_from_l0 = r.flag()?;
                }
                let active = if header.collocated_from_l0 {
                    header.num_ref_idx_l0_active_minus1
                } else {
                    header.num_ref_idx_l1_active_minus1
                };
                if active > 0 {
                    header.collocated_ref_idx = small(r.ue()?, u32::from(active))?;
                }
            }
            if (pps.weighted_pred && header.slice_type == SliceType::P)
                || (pps.weighted_bipred && header.slice_type.is_b())
            {
                let chroma = sps.chroma_format_idc != 0;
                let luma_log2_denom = small(r.ue()?, 7)?;
                let delta_chroma_log2_denom = if chroma {
                    let delta = r.se()?;
                    let sum = i32::from(luma_log2_denom) + delta;
                    if !(0..=7).contains(&sum) {
                        return Err(ParseError::OutOfRange);
                    }
                    small_signed(delta, -7, 7)?
                } else {
                    0
                };
                let chroma_log2_denom =
                    u32::try_from(i32::from(luma_log2_denom) + i32::from(delta_chroma_log2_denom))
                        .unwrap_or(0);
                let half_range = if sps.range.high_precision_offsets_enabled {
                    1i32 << (u32::from(sps.bit_depth_chroma_minus8) + 7)
                } else {
                    128
                };
                let l0 = weights(
                    &mut r,
                    usize::from(header.num_ref_idx_l0_active_minus1) + 1,
                    chroma,
                    chroma_log2_denom,
                    half_range,
                )?;
                let l1 = if header.slice_type.is_b() {
                    weights(
                        &mut r,
                        usize::from(header.num_ref_idx_l1_active_minus1) + 1,
                        chroma,
                        chroma_log2_denom,
                        half_range,
                    )?
                } else {
                    Weights::default()
                };
                header.weights = Some(PredWeights {
                    luma_log2_denom,
                    delta_chroma_log2_denom,
                    l0,
                    l1,
                });
            }
            header.five_minus_max_num_merge_cand = small(r.ue()?, 4)?;
        }
        header.slice_qp_delta = small_signed(r.se()?, -87, 87)?;
        if pps.slice_chroma_qp_offsets_present {
            header.slice_cb_qp_offset = small_signed(r.se()?, -12, 12)?;
            header.slice_cr_qp_offset = small_signed(r.se()?, -12, 12)?;
        }
        if pps.range.chroma_qp_offset_list_enabled {
            header.cu_chroma_qp_offset_enabled = r.flag()?;
        }
        let mut deblocking_override = false;
        if pps.deblocking_filter_override_enabled {
            deblocking_override = r.flag()?;
        }
        if deblocking_override {
            header.deblocking_filter_disabled = r.flag()?;
            if !header.deblocking_filter_disabled {
                header.beta_offset_div2 = small_signed(r.se()?, -6, 6)?;
                header.tc_offset_div2 = small_signed(r.se()?, -6, 6)?;
            }
        }
        if pps.loop_filter_across_slices_enabled
            && (header.sao_luma || header.sao_chroma || !header.deblocking_filter_disabled)
        {
            header.loop_filter_across_slices_enabled = r.flag()?;
        }
    }

    if pps.tiles_enabled || pps.entropy_coding_sync_enabled {
        header.num_entry_point_offsets = r.ue_max(440 * 64)?;
        if header.num_entry_point_offsets > 0 {
            let offset_len_minus1 = r.ue_max(31)?;
            for _ in 0..header.num_entry_point_offsets {
                r.skip(offset_len_minus1 + 1)?;
            }
        }
    }
    if pps.slice_segment_header_extension_present {
        let length = r.ue_max(256)?;
        for _ in 0..length {
            r.skip(8)?;
        }
    }
    // byte_alignment(): a one, then zeros to the boundary.
    if !r.flag()? {
        return Err(ParseError::OutOfRange);
    }
    r.align();
    header.header_bytes = u32::try_from(2 + r.position() / 8).unwrap_or(0);
    header.header_escapes = r.escapes();
    Ok(header)
}
