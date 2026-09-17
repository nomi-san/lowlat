//! The H.264 slice header.

use crate::ParseError;
use crate::bits::BitReader;
use crate::h264::pps::Pps;
use crate::h264::sps::Sps;

type Result<T> = core::result::Result<T, ParseError>;

/// The most reference indices a list may hold: 32 fields.
pub const MAX_REFS: usize = 32;
/// Reference list modification operations per list: one per active index
/// and the end marker.
pub const MAX_MODIFICATIONS: usize = MAX_REFS + 1;
/// Memory management operations a marking may carry.
pub const MAX_MMCO: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    fn from_code(code: u32) -> Result<Self> {
        Ok(match code % 5 {
            0 => Self::P,
            1 => Self::B,
            2 => Self::I,
            3 => Self::Sp,
            4 => Self::Si,
            _ => return Err(ParseError::OutOfRange),
        })
    }

    /// The value the device is handed: the standard's own code, folded.
    pub fn code(self) -> u8 {
        match self {
            Self::P => 0,
            Self::B => 1,
            Self::I => 2,
            Self::Sp => 3,
            Self::Si => 4,
        }
    }

    pub fn is_intra(self) -> bool {
        matches!(self, Self::I | Self::Si)
    }

    pub fn is_b(self) -> bool {
        matches!(self, Self::B)
    }
}

/// One reference list modification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modification {
    /// `modification_of_pic_nums_idc` 0: subtract from the predicted picture
    /// number.
    Subtract(u32),
    /// 1: add.
    Add(u32),
    /// 2: a long-term picture number.
    LongTerm(u32),
}

/// One memory management control operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mmco {
    /// 1: mark a short-term picture unused, by difference of picture numbers.
    UnmarkShort(u32),
    /// 2: mark a long-term picture unused, by long-term picture number.
    UnmarkLong(u32),
    /// 3: make a short-term picture long-term, with the index.
    ToLong(u32, u32),
    /// 4: the maximum long-term index, plus one (zero means none).
    MaxLongTerm(u32),
    /// 5: everything unused, and the counts reset.
    UnmarkAll,
    /// 6: the current picture long-term, with the index.
    CurrentLong(u32),
}

/// A fixed list with a count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct List<T: Copy, const N: usize> {
    pub items: [T; N],
    pub len: usize,
}

impl<T: Copy + Default, const N: usize> Default for List<T, N> {
    fn default() -> Self {
        Self {
            items: [T::default(); N],
            len: 0,
        }
    }
}

impl<T: Copy, const N: usize> List<T, N> {
    pub fn as_slice(&self) -> &[T] {
        self.items.get(..self.len).unwrap_or(&[])
    }

    pub fn push(&mut self, item: T) -> Result<()> {
        let slot = self.items.get_mut(self.len).ok_or(ParseError::TooMany)?;
        *slot = item;
        self.len += 1;
        Ok(())
    }
}

/// Explicit prediction weights for one list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    pub luma_weight: [i16; MAX_REFS],
    pub luma_offset: [i16; MAX_REFS],
    pub chroma_weight: [[i16; 2]; MAX_REFS],
    pub chroma_offset: [[i16; 2]; MAX_REFS],
    pub luma_flag: bool,
    pub chroma_flag: bool,
}

impl Weights {
    fn defaults(luma_denom: u8, chroma_denom: u8) -> Self {
        Self {
            luma_weight: [1i16 << luma_denom; MAX_REFS],
            luma_offset: [0; MAX_REFS],
            chroma_weight: [[1i16 << chroma_denom; 2]; MAX_REFS],
            chroma_offset: [[0; 2]; MAX_REFS],
            luma_flag: false,
            chroma_flag: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredWeights {
    pub luma_log2_denom: u8,
    pub chroma_log2_denom: u8,
    pub l0: Weights,
    pub l1: Weights,
}

/// The reference marking a slice carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "the operations list is the header's, fixed and small; a box would allocate per slice"
)]
pub enum Marking {
    /// Not a reference picture.
    None,
    /// An instantaneous refresh.
    Idr {
        no_output_of_prior_pics: bool,
        long_term: bool,
    },
    /// Sliding window.
    Sliding,
    /// The operations, in order.
    Adaptive(List<Option<Mmco>, MAX_MMCO>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceHeader {
    pub nal_unit_type: u8,
    pub nal_ref_idc: u8,
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    pub pps_id: u8,
    pub colour_plane_id: u8,
    pub frame_num: u32,
    pub field_pic: bool,
    pub bottom_field: bool,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred: bool,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub modifications_l0: List<Option<Modification>, MAX_MODIFICATIONS>,
    pub modifications_l1: List<Option<Modification>, MAX_MODIFICATIONS>,
    pub weights: Option<PredWeights>,
    pub marking: Marking,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub sp_for_switch: bool,
    pub slice_qs_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    /// Bits of header read, escapes excluded, from the first payload bit.
    pub header_bits: usize,
}

impl SliceHeader {
    pub fn is_idr(&self) -> bool {
        self.nal_unit_type == 5
    }

    pub fn is_reference(&self) -> bool {
        self.nal_ref_idc != 0
    }

    /// Whether the picture's marking carries a reset (memory management 5).
    pub fn has_mmco5(&self) -> bool {
        match self.marking {
            Marking::Adaptive(ops) => ops
                .as_slice()
                .iter()
                .any(|op| matches!(op, Some(Mmco::UnmarkAll))),
            _ => false,
        }
    }
}

fn modifications(r: &mut BitReader<'_>) -> Result<List<Option<Modification>, MAX_MODIFICATIONS>> {
    let mut list = List::default();
    if !r.flag()? {
        return Ok(list);
    }
    loop {
        let idc = r.ue_max(3)?;
        let op = match idc {
            0 => Modification::Subtract(r.ue()?),
            1 => Modification::Add(r.ue()?),
            2 => Modification::LongTerm(r.ue()?),
            _ => break,
        };
        list.push(Some(op))?;
    }
    Ok(list)
}

fn weights(
    r: &mut BitReader<'_>,
    count: usize,
    chroma: bool,
    luma_denom: u8,
    chroma_denom: u8,
) -> Result<Weights> {
    let mut w = Weights::defaults(luma_denom, chroma_denom);
    for i in 0..count.min(MAX_REFS) {
        if r.flag()? {
            w.luma_flag = true;
            let weight = r.se()?;
            let offset = r.se()?;
            let weight = i16::try_from(weight).map_err(|_| ParseError::OutOfRange)?;
            let offset = i16::try_from(offset).map_err(|_| ParseError::OutOfRange)?;
            if !(-128..=127).contains(&weight) || !(-128..=127).contains(&offset) {
                return Err(ParseError::OutOfRange);
            }
            if let Some(slot) = w.luma_weight.get_mut(i) {
                *slot = weight;
            }
            if let Some(slot) = w.luma_offset.get_mut(i) {
                *slot = offset;
            }
        }
        if chroma && r.flag()? {
            w.chroma_flag = true;
            for j in 0..2 {
                let weight = r.se()?;
                let offset = r.se()?;
                let weight = i16::try_from(weight).map_err(|_| ParseError::OutOfRange)?;
                let offset = i16::try_from(offset).map_err(|_| ParseError::OutOfRange)?;
                if !(-128..=127).contains(&weight) || !(-128..=127).contains(&offset) {
                    return Err(ParseError::OutOfRange);
                }
                if let Some(slot) = w.chroma_weight.get_mut(i).and_then(|s| s.get_mut(j)) {
                    *slot = weight;
                }
                if let Some(slot) = w.chroma_offset.get_mut(i).and_then(|s| s.get_mut(j)) {
                    *slot = offset;
                }
            }
        }
    }
    Ok(w)
}

fn marking(r: &mut BitReader<'_>, idr: bool) -> Result<Marking> {
    if idr {
        let no_output_of_prior_pics = r.flag()?;
        let long_term = r.flag()?;
        return Ok(Marking::Idr {
            no_output_of_prior_pics,
            long_term,
        });
    }
    if !r.flag()? {
        return Ok(Marking::Sliding);
    }
    let mut ops = List::default();
    loop {
        let op = match r.ue_max(6)? {
            0 => break,
            1 => Mmco::UnmarkShort(r.ue()?),
            2 => Mmco::UnmarkLong(r.ue()?),
            3 => {
                let difference = r.ue()?;
                Mmco::ToLong(difference, r.ue()?)
            }
            4 => Mmco::MaxLongTerm(r.ue()?),
            5 => Mmco::UnmarkAll,
            _ => Mmco::CurrentLong(r.ue()?),
        };
        ops.push(Some(op))?;
    }
    Ok(Marking::Adaptive(ops))
}

fn small_signed(value: i32, min: i32, max: i32) -> Result<i8> {
    if value < min || value > max {
        return Err(ParseError::OutOfRange);
    }
    i8::try_from(value).map_err(|_| ParseError::OutOfRange)
}

/// Parse a slice header. `unit` is the whole unit, header byte included;
/// the parameter sets it names are looked up as they are met.
pub fn parse(
    unit: &[u8],
    pps_of: impl Fn(u8) -> Option<Pps>,
    sps_of: impl Fn(u8) -> Option<Sps>,
) -> Result<SliceHeader> {
    let (&first, payload) = unit.split_first().ok_or(ParseError::Truncated)?;
    let nal_unit_type = first & 0x1F;
    let nal_ref_idc = (first >> 5) & 0x03;
    if nal_unit_type == 20 || nal_unit_type == 21 {
        // Multiview and scalable extensions.
        return Err(ParseError::Unsupported);
    }
    let idr = nal_unit_type == 5;
    let mut r = BitReader::new(payload);

    let first_mb_in_slice = r.ue()?;
    let slice_type = SliceType::from_code(r.ue_max(9)?)?;
    let pps_id = u8::try_from(r.ue_max(255)?).map_err(|_| ParseError::OutOfRange)?;
    let pps = pps_of(pps_id).ok_or(ParseError::NoParameterSet)?;
    let sps = sps_of(pps.sps_id).ok_or(ParseError::NoParameterSet)?;
    if idr && !slice_type.is_intra() {
        return Err(ParseError::OutOfRange);
    }

    let mut colour_plane_id = 0u8;
    if sps.separate_colour_plane {
        colour_plane_id = r.u8(2)?;
    }
    let frame_num = r.bits(u32::from(sps.log2_max_frame_num_minus4) + 4)?;
    let mut field_pic = false;
    let mut bottom_field = false;
    if !sps.frame_mbs_only {
        field_pic = r.flag()?;
        if field_pic {
            bottom_field = r.flag()?;
        }
    }
    let mut idr_pic_id = 0;
    if idr {
        idr_pic_id = r.ue_max(65535)?;
    }
    let mut pic_order_cnt_lsb = 0;
    let mut delta_pic_order_cnt_bottom = 0;
    let mut delta_pic_order_cnt = [0i32; 2];
    if sps.pic_order_cnt_type == 0 {
        pic_order_cnt_lsb = r.bits(u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4)?;
        if pps.bottom_field_pic_order_in_frame_present && !field_pic {
            delta_pic_order_cnt_bottom = r.se()?;
        }
    }
    if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        delta_pic_order_cnt[0] = r.se()?;
        if pps.bottom_field_pic_order_in_frame_present && !field_pic {
            delta_pic_order_cnt[1] = r.se()?;
        }
    }
    let mut redundant_pic_cnt = 0;
    if pps.redundant_pic_cnt_present {
        redundant_pic_cnt = r.ue_max(127)?;
    }
    let mut direct_spatial_mv_pred = false;
    if slice_type.is_b() {
        direct_spatial_mv_pred = r.flag()?;
    }
    let mut num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    let mut num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    if !slice_type.is_intra() && r.flag()? {
        let max = if field_pic { 31 } else { 15 };
        num_ref_idx_l0_active_minus1 =
            u8::try_from(r.ue_max(max)?).map_err(|_| ParseError::OutOfRange)?;
        if slice_type.is_b() {
            num_ref_idx_l1_active_minus1 =
                u8::try_from(r.ue_max(max)?).map_err(|_| ParseError::OutOfRange)?;
        }
    }
    let mut modifications_l0 = List::default();
    let mut modifications_l1 = List::default();
    if !slice_type.is_intra() {
        modifications_l0 = modifications(&mut r)?;
        if slice_type.is_b() {
            modifications_l1 = modifications(&mut r)?;
        }
    }
    let mut weights = None;
    if (pps.weighted_pred && matches!(slice_type, SliceType::P | SliceType::Sp))
        || (pps.weighted_bipred_idc == 1 && slice_type.is_b())
    {
        let chroma = sps.chroma_format_idc != 0 && !sps.separate_colour_plane;
        let luma_log2_denom = u8::try_from(r.ue_max(7)?).map_err(|_| ParseError::OutOfRange)?;
        let chroma_log2_denom = if chroma {
            u8::try_from(r.ue_max(7)?).map_err(|_| ParseError::OutOfRange)?
        } else {
            0
        };
        let l0 = self::weights(
            &mut r,
            usize::from(num_ref_idx_l0_active_minus1) + 1,
            chroma,
            luma_log2_denom,
            chroma_log2_denom,
        )?;
        let l1 = if slice_type.is_b() {
            self::weights(
                &mut r,
                usize::from(num_ref_idx_l1_active_minus1) + 1,
                chroma,
                luma_log2_denom,
                chroma_log2_denom,
            )?
        } else {
            Weights::defaults(luma_log2_denom, chroma_log2_denom)
        };
        weights = Some(PredWeights {
            luma_log2_denom,
            chroma_log2_denom,
            l0,
            l1,
        });
    }
    let marking = if nal_ref_idc != 0 {
        marking(&mut r, idr)?
    } else {
        Marking::None
    };
    let mut cabac_init_idc = 0;
    if pps.entropy_coding_mode && !slice_type.is_intra() {
        cabac_init_idc = u8::try_from(r.ue_max(2)?).map_err(|_| ParseError::OutOfRange)?;
    }
    let slice_qp_delta = small_signed(r.se()?, -87, 87)?;
    let mut sp_for_switch = false;
    let mut slice_qs_delta = 0;
    if matches!(slice_type, SliceType::Sp | SliceType::Si) {
        if slice_type == SliceType::Sp {
            sp_for_switch = r.flag()?;
        }
        slice_qs_delta = small_signed(r.se()?, -51, 51)?;
    }
    let mut disable_deblocking_filter_idc = 0;
    let mut slice_alpha_c0_offset_div2 = 0;
    let mut slice_beta_offset_div2 = 0;
    if pps.deblocking_filter_control_present {
        disable_deblocking_filter_idc =
            u8::try_from(r.ue_max(2)?).map_err(|_| ParseError::OutOfRange)?;
        if disable_deblocking_filter_idc != 1 {
            slice_alpha_c0_offset_div2 = small_signed(r.se()?, -6, 6)?;
            slice_beta_offset_div2 = small_signed(r.se()?, -6, 6)?;
        }
    }
    // Slice groups were refused at the picture set, so nothing follows.

    Ok(SliceHeader {
        nal_unit_type,
        nal_ref_idc,
        first_mb_in_slice,
        slice_type,
        pps_id,
        colour_plane_id,
        frame_num,
        field_pic,
        bottom_field,
        idr_pic_id,
        pic_order_cnt_lsb,
        delta_pic_order_cnt_bottom,
        delta_pic_order_cnt,
        redundant_pic_cnt,
        direct_spatial_mv_pred,
        num_ref_idx_l0_active_minus1,
        num_ref_idx_l1_active_minus1,
        modifications_l0,
        modifications_l1,
        weights,
        marking,
        cabac_init_idc,
        slice_qp_delta,
        sp_for_switch,
        slice_qs_delta,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        header_bits: r.position(),
    })
}
