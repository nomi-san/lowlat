//! The H.264 picture parameter set.

use crate::ParseError;
use crate::bits::BitReader;
use crate::h264::sps::{ScalingLists, Sps, scaling_lists};

type Result<T> = core::result::Result<T, ParseError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_slice_groups_minus1: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub scaling_matrix_present: bool,
    /// The lists in force for pictures under this set: the picture's own
    /// where coded, the sequence's otherwise, resolved at parse.
    pub scaling: ScalingLists,
    pub second_chroma_qp_index_offset: i8,
}

fn small_signed(value: i32, min: i32, max: i32) -> Result<i8> {
    if value < min || value > max {
        return Err(ParseError::OutOfRange);
    }
    i8::try_from(value).map_err(|_| ParseError::OutOfRange)
}

/// Parse a picture parameter set from its payload. The sequence set it
/// names must already be known, for the scaling-list fall-back and the
/// chroma format.
pub fn parse(payload: &[u8], sps_of: impl Fn(u8) -> Option<Sps>) -> Result<Pps> {
    let mut r = BitReader::new(payload);
    let id = u8::try_from(r.ue_max(255)?).map_err(|_| ParseError::OutOfRange)?;
    let sps_id = u8::try_from(r.ue_max(31)?).map_err(|_| ParseError::OutOfRange)?;
    let sps = sps_of(sps_id).ok_or(ParseError::NoParameterSet)?;
    let entropy_coding_mode = r.flag()?;
    let bottom_field_pic_order_in_frame_present = r.flag()?;
    let num_slice_groups_minus1 = u8::try_from(r.ue_max(7)?).map_err(|_| ParseError::OutOfRange)?;
    if num_slice_groups_minus1 > 0 {
        // Slice groups are parsed so the set is read whole, and refused: no
        // device here decodes them.
        let map_type = r.ue_max(6)?;
        match map_type {
            0 => {
                for _ in 0..=num_slice_groups_minus1 {
                    r.ue()?;
                }
            }
            2 => {
                for _ in 0..num_slice_groups_minus1 {
                    r.ue()?;
                    r.ue()?;
                }
            }
            3..=5 => {
                r.flag()?;
                r.ue()?;
            }
            6 => {
                let count = r.ue()?;
                let bits = 32 - u32::from(num_slice_groups_minus1).leading_zeros();
                for _ in 0..=count {
                    r.skip(bits)?;
                }
            }
            _ => {}
        }
        return Err(ParseError::Unsupported);
    }
    let num_ref_idx_l0_default_active_minus1 =
        u8::try_from(r.ue_max(31)?).map_err(|_| ParseError::OutOfRange)?;
    let num_ref_idx_l1_default_active_minus1 =
        u8::try_from(r.ue_max(31)?).map_err(|_| ParseError::OutOfRange)?;
    let weighted_pred = r.flag()?;
    let weighted_bipred_idc = r.u8(2)?;
    let pic_init_qp_minus26 = small_signed(r.se()?, -62, 25)?;
    let pic_init_qs_minus26 = small_signed(r.se()?, -26, 25)?;
    let chroma_qp_index_offset = small_signed(r.se()?, -12, 12)?;
    let deblocking_filter_control_present = r.flag()?;
    let constrained_intra_pred = r.flag()?;
    let redundant_pic_cnt_present = r.flag()?;

    let mut transform_8x8_mode = false;
    let mut scaling_matrix_present = false;
    let mut scaling = sps.scaling;
    let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
    if r.more_rbsp_data() {
        transform_8x8_mode = r.flag()?;
        scaling_matrix_present = r.flag()?;
        if scaling_matrix_present {
            // An uncoded list falls back to the sequence's when the
            // sequence coded a matrix (rule B; `scaling` already holds those)
            // and to the defaults when it did not (rule A).
            let count = 6 + if transform_8x8_mode {
                if sps.chroma_format_idc == 3 { 6 } else { 2 }
            } else {
                0
            };
            scaling_lists(&mut r, &mut scaling, count, !sps.scaling_matrix_present)?;
        }
        second_chroma_qp_index_offset = small_signed(r.se()?, -12, 12)?;
    }

    Ok(Pps {
        id,
        sps_id,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_slice_groups_minus1,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode,
        scaling_matrix_present,
        scaling,
        second_chroma_qp_index_offset,
    })
}
