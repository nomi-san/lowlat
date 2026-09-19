//! The HEVC picture parameter set.

use crate::ParseError;
use crate::bits::BitReader;
use crate::hevc::sps::{ScalingLists, Sps, scaling_list_data};

type Result<T> = core::result::Result<T, ParseError>;

/// Tile columns and rows the device takes.
pub const MAX_TILE_COLUMNS: usize = 20;
pub const MAX_TILE_ROWS: usize = 22;
const MAX_TILE_COLUMNS_MINUS1: u32 = 19;
const MAX_TILE_ROWS_MINUS1: u32 = 21;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding_enabled: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u8,
    pub cb_qp_offset: i8,
    pub cr_qp_offset: i8,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub uniform_spacing: bool,
    pub column_width_minus1: [u16; MAX_TILE_COLUMNS],
    pub row_height_minus1: [u16; MAX_TILE_ROWS],
    pub loop_filter_across_tiles_enabled: bool,
    pub loop_filter_across_slices_enabled: bool,
    pub deblocking_filter_override_enabled: bool,
    pub disable_deblocking_filter: bool,
    pub beta_offset_div2: i8,
    pub tc_offset_div2: i8,
    pub scaling_list_data_present: bool,
    /// The lists in force under this set when scaling is enabled: the
    /// picture's own where coded, the sequence's otherwise.
    pub scaling: ScalingLists,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level_minus2: u8,
    pub slice_segment_header_extension_present: bool,
    pub range: RangeExtension,
}

/// Chroma offsets a picture's list may carry.
pub const MAX_CHROMA_QP_OFFSETS: usize = 6;

/// `pps_range_extension()`. All zero for a picture that carries none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RangeExtension {
    pub log2_max_transform_skip_block_size_minus2: u8,
    pub cross_component_prediction_enabled: bool,
    pub chroma_qp_offset_list_enabled: bool,
    pub diff_cu_chroma_qp_offset_depth: u8,
    pub chroma_qp_offset_list_len_minus1: u8,
    pub cb_qp_offset_list: [i8; MAX_CHROMA_QP_OFFSETS],
    pub cr_qp_offset_list: [i8; MAX_CHROMA_QP_OFFSETS],
    pub log2_sao_offset_scale_luma: u8,
    pub log2_sao_offset_scale_chroma: u8,
}

fn small_signed(value: i32, min: i32, max: i32) -> Result<i8> {
    if value < min || value > max {
        return Err(ParseError::OutOfRange);
    }
    i8::try_from(value).map_err(|_| ParseError::OutOfRange)
}

fn small(value: u32, max: u32) -> Result<u8> {
    if value > max {
        return Err(ParseError::OutOfRange);
    }
    u8::try_from(value).map_err(|_| ParseError::OutOfRange)
}

/// Parse a picture parameter set from its payload. The sequence set it
/// names must already be known.
pub fn parse(payload: &[u8], sps_of: impl Fn(u8) -> Option<Sps>) -> Result<Pps> {
    let mut r = BitReader::new(payload);
    let id = small(r.ue()?, 63)?;
    let sps_id = small(r.ue()?, 15)?;
    let sps = sps_of(sps_id).ok_or(ParseError::NoParameterSet)?;
    let dependent_slice_segments_enabled = r.flag()?;
    let output_flag_present = r.flag()?;
    let num_extra_slice_header_bits = r.u8(3)?;
    let sign_data_hiding_enabled = r.flag()?;
    let cabac_init_present = r.flag()?;
    let num_ref_idx_l0_default_active_minus1 = small(r.ue()?, 14)?;
    let num_ref_idx_l1_default_active_minus1 = small(r.ue()?, 14)?;
    let init_qp_minus26 = small_signed(r.se()?, -62, 25)?;
    let constrained_intra_pred = r.flag()?;
    let transform_skip_enabled = r.flag()?;
    let cu_qp_delta_enabled = r.flag()?;
    let diff_cu_qp_delta_depth = if cu_qp_delta_enabled {
        small(
            r.ue()?,
            u32::from(sps.log2_diff_max_min_luma_coding_block_size),
        )?
    } else {
        0
    };
    let cb_qp_offset = small_signed(r.se()?, -12, 12)?;
    let cr_qp_offset = small_signed(r.se()?, -12, 12)?;
    let slice_chroma_qp_offsets_present = r.flag()?;
    let weighted_pred = r.flag()?;
    let weighted_bipred = r.flag()?;
    let transquant_bypass_enabled = r.flag()?;
    let tiles_enabled = r.flag()?;
    let entropy_coding_sync_enabled = r.flag()?;
    let mut num_tile_columns_minus1 = 0u8;
    let mut num_tile_rows_minus1 = 0u8;
    let mut uniform_spacing = true;
    let mut column_width_minus1 = [0u16; MAX_TILE_COLUMNS];
    let mut row_height_minus1 = [0u16; MAX_TILE_ROWS];
    let mut loop_filter_across_tiles_enabled = true;
    if tiles_enabled {
        num_tile_columns_minus1 = small(r.ue()?, MAX_TILE_COLUMNS_MINUS1)?;
        num_tile_rows_minus1 = small(r.ue()?, MAX_TILE_ROWS_MINUS1)?;
        uniform_spacing = r.flag()?;
        if !uniform_spacing {
            for i in 0..usize::from(num_tile_columns_minus1) {
                *column_width_minus1.get_mut(i).ok_or(ParseError::TooMany)? =
                    u16::try_from(r.ue_max(u32::from(u16::MAX))?)
                        .map_err(|_| ParseError::OutOfRange)?;
            }
            for i in 0..usize::from(num_tile_rows_minus1) {
                *row_height_minus1.get_mut(i).ok_or(ParseError::TooMany)? =
                    u16::try_from(r.ue_max(u32::from(u16::MAX))?)
                        .map_err(|_| ParseError::OutOfRange)?;
            }
        }
        loop_filter_across_tiles_enabled = r.flag()?;
    }
    let loop_filter_across_slices_enabled = r.flag()?;
    let deblocking_filter_control_present = r.flag()?;
    let mut deblocking_filter_override_enabled = false;
    let mut disable_deblocking_filter = false;
    let mut beta_offset_div2 = 0;
    let mut tc_offset_div2 = 0;
    if deblocking_filter_control_present {
        deblocking_filter_override_enabled = r.flag()?;
        disable_deblocking_filter = r.flag()?;
        if !disable_deblocking_filter {
            beta_offset_div2 = small_signed(r.se()?, -6, 6)?;
            tc_offset_div2 = small_signed(r.se()?, -6, 6)?;
        }
    }
    let scaling_list_data_present = r.flag()?;
    let scaling = if scaling_list_data_present {
        scaling_list_data(&mut r)?
    } else {
        sps.scaling
    };
    let lists_modification_present = r.flag()?;
    let log2_parallel_merge_level_minus2 = small(r.ue()?, 4)?;
    let slice_segment_header_extension_present = r.flag()?;
    let mut range = RangeExtension::default();
    if r.flag()? {
        // pps_extension_present: the range extension is read; the
        // multilayer, 3D and screen-content ones are refused, as in the
        // sequence set.
        let has_range = r.flag()?;
        let multilayer = r.flag()?;
        let three_d = r.flag()?;
        let scc = r.flag()?;
        let four_bits = r.bits(4)?;
        if multilayer || three_d || scc || four_bits != 0 {
            return Err(ParseError::Unsupported);
        }
        if has_range {
            if transform_skip_enabled {
                range.log2_max_transform_skip_block_size_minus2 = small(r.ue()?, 3)?;
            }
            range.cross_component_prediction_enabled = r.flag()?;
            range.chroma_qp_offset_list_enabled = r.flag()?;
            if range.chroma_qp_offset_list_enabled {
                range.diff_cu_chroma_qp_offset_depth = small(r.ue()?, 3)?;
                range.chroma_qp_offset_list_len_minus1 = small(r.ue()?, 5)?;
                for i in 0..=usize::from(range.chroma_qp_offset_list_len_minus1) {
                    *range
                        .cb_qp_offset_list
                        .get_mut(i)
                        .ok_or(ParseError::TooMany)? = small_signed(r.se()?, -12, 12)?;
                    *range
                        .cr_qp_offset_list
                        .get_mut(i)
                        .ok_or(ParseError::TooMany)? = small_signed(r.se()?, -12, 12)?;
                }
            }
            range.log2_sao_offset_scale_luma = small(r.ue()?, 6)?;
            range.log2_sao_offset_scale_chroma = small(r.ue()?, 6)?;
        }
    }

    Ok(Pps {
        id,
        sps_id,
        dependent_slice_segments_enabled,
        output_flag_present,
        num_extra_slice_header_bits,
        sign_data_hiding_enabled,
        cabac_init_present,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        init_qp_minus26,
        constrained_intra_pred,
        transform_skip_enabled,
        cu_qp_delta_enabled,
        diff_cu_qp_delta_depth,
        cb_qp_offset,
        cr_qp_offset,
        slice_chroma_qp_offsets_present,
        weighted_pred,
        weighted_bipred,
        transquant_bypass_enabled,
        tiles_enabled,
        entropy_coding_sync_enabled,
        num_tile_columns_minus1,
        num_tile_rows_minus1,
        uniform_spacing,
        column_width_minus1,
        row_height_minus1,
        loop_filter_across_tiles_enabled,
        loop_filter_across_slices_enabled,
        deblocking_filter_override_enabled,
        disable_deblocking_filter,
        beta_offset_div2,
        tc_offset_div2,
        scaling_list_data_present,
        scaling,
        lists_modification_present,
        log2_parallel_merge_level_minus2,
        slice_segment_header_extension_present,
        range,
    })
}
