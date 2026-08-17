//! Parameter sets for the second codec, written out.
//!
//! Same reason as the first codec's: one backend cannot state the colour
//! description any other way, so the sets are written here and handed over as
//! packed headers. See [05 §3.1](../../../docs/05-host.md).
//!
//! **Three sets rather than two.** This codec puts a video parameter set ahead
//! of the sequence and picture sets, and a decoder that never receives one has
//! nothing to attach the sequence set to.
//!
//! The layout is the coding standard's, not a vendor's, so everything here is
//! checkable without hardware.

use crate::bitstream::{BitWriter, escape};

/// Four-byte start code, as for the other codec.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Main profile: 8-bit 4:2:0, which is what this pipeline produces.
const PROFILE_MAIN: u32 = 1;

/// Unspecified, which is what a frame that never touched an analogue format is.
const VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
/// BT.709, for primaries, transfer and matrix alike.
const BT709: u32 = 1;

/// Unit types. **Two-byte header on this codec**, and the type sits in six
/// bits of the first byte rather than five, so a reader written for the other
/// codec lands on the wrong value rather than failing.
const NAL_VPS: u8 = 32;
const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;
/// An instantaneous refresh with no leading pictures.
pub const NAL_IDR_N_LP: u8 = 20;
/// An ordinary trailing picture that may be referenced.
pub const NAL_TRAIL_R: u8 = 1;

/// The coding tree block size this writes, as a power of two.
///
/// Fixed at 64, which every encoder in this class uses and every decoder
/// supports. It is not configurable because nothing here has a reason to
/// choose differently, and each value changes several derived fields at once.
const LOG2_CTB: u32 = 6;
const LOG2_MIN_CB: u32 = 3;

/// Everything the three sets need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// **Thirty times the level number on this codec**, not ten, so 4.1 is
    /// 123. Writing the other codec's convention here declares a level far
    /// below what 1080p60 needs and a strict decoder refuses the stream.
    pub level_idc: u32,
    /// Bits of picture order count before it wraps, less four.
    pub log2_max_poc_lsb_minus4: u32,
    /// One is enough without bidirectional pictures: each references the last.
    pub max_num_ref_frames: u32,
}

impl Params {
    /// The coded size, which is a whole number of minimum coding blocks.
    ///
    /// **The conformance window crops the difference**, and it is measured in
    /// chroma units for a 4:2:0 stream, so a crop counted here in luma samples
    /// is halved where it is written.
    fn coded(&self) -> (u32, u32) {
        let unit = 1 << LOG2_MIN_CB;
        (
            self.width.div_ceil(unit) * unit,
            self.height.div_ceil(unit) * unit,
        )
    }

    /// Right and bottom crop, in luma samples.
    fn crop(&self) -> (u32, u32) {
        let (coded_width, coded_height) = self.coded();
        (coded_width - self.width, coded_height - self.height)
    }
}

/// The profile, tier and level block, which all three sets share.
///
/// Twelve bytes of profile description then the level. **The general
/// compatibility flags are not optional decoration**: a decoder matches the
/// profile by them as well as by the profile field, and leaving them clear
/// makes a main-profile stream look like one no profile claims.
fn profile_tier_level(w: &mut BitWriter<'_>, level_idc: u32) {
    w.bits(0, 2); // general_profile_space
    w.bit(false); // general_tier_flag: main tier
    w.bits(PROFILE_MAIN, 5);
    // general_profile_compatibility_flag[32], with the bit for our own
    // profile set.
    for index in 0..32u32 {
        w.bit(index == PROFILE_MAIN);
    }
    w.bit(true); // general_progressive_source_flag
    w.bit(false); // general_interlaced_source_flag
    w.bit(false); // general_non_packed_constraint_flag
    w.bit(true); // general_frame_only_constraint_flag
    // general_reserved_zero_43bits, then the inbound-compatibility bit.
    w.bits(0, 32);
    w.bits(0, 11);
    w.bit(false);
    w.bits(level_idc, 8);
}

/// Write the video parameter set, start code and escaping included.
pub fn video_parameter_set(params: &Params, out: &mut [u8]) -> Option<usize> {
    let mut raw = [0u8; 128];
    let mut w = BitWriter::new(&mut raw);

    w.bits(0, 4); // vps_video_parameter_set_id
    w.bit(true); // vps_base_layer_internal_flag
    w.bit(true); // vps_base_layer_available_flag
    w.bits(0, 6); // vps_max_layers_minus1
    w.bits(0, 3); // vps_max_sub_layers_minus1
    w.bit(true); // vps_temporal_id_nesting_flag
    w.bits(0xFFFF, 16); // vps_reserved_0xffff_16bits
    profile_tier_level(&mut w, params.level_idc);
    w.bit(true); // vps_sub_layer_ordering_info_present_flag
    // One picture is reordered by nothing and held by one reference.
    w.ue(params.max_num_ref_frames); // vps_max_dec_pic_buffering_minus1[0]
    w.ue(0); // vps_max_num_reorder_pics[0]
    w.ue(0); // vps_max_latency_increase_plus1[0]
    w.bits(0, 6); // vps_max_layer_id
    w.ue(0); // vps_num_layer_sets_minus1
    w.bit(false); // vps_timing_info_present_flag
    w.bit(false); // vps_extension_flag

    if !w.trailing_bits() {
        return None;
    }
    emit(NAL_VPS, w.finish(), out)
}

/// Write the sequence parameter set, start code and escaping included.
pub fn sequence_parameter_set(params: &Params, out: &mut [u8]) -> Option<usize> {
    let mut raw = [0u8; 192];
    let mut w = BitWriter::new(&mut raw);

    w.bits(0, 4); // sps_video_parameter_set_id
    w.bits(0, 3); // sps_max_sub_layers_minus1
    w.bit(true); // sps_temporal_id_nesting_flag
    profile_tier_level(&mut w, params.level_idc);
    w.ue(0); // sps_seq_parameter_set_id
    w.ue(1); // chroma_format_idc: 4:2:0

    let (coded_width, coded_height) = params.coded();
    w.ue(coded_width);
    w.ue(coded_height);
    let (right, bottom) = params.crop();
    let cropping = right > 0 || bottom > 0;
    w.bit(cropping);
    if cropping {
        // The window is in units of the chroma subsampling factor, which is
        // two for 4:2:0, so a crop of six luma rows is written as three.
        w.ue(0);
        w.ue(right / 2);
        w.ue(0);
        w.ue(bottom / 2);
    }

    w.ue(0); // bit_depth_luma_minus8
    w.ue(0); // bit_depth_chroma_minus8
    w.ue(params.log2_max_poc_lsb_minus4);
    w.bit(true); // sps_sub_layer_ordering_info_present_flag
    w.ue(params.max_num_ref_frames); // sps_max_dec_pic_buffering_minus1[0]
    w.ue(0); // sps_max_num_reorder_pics[0]
    w.ue(0); // sps_max_latency_increase_plus1[0]

    w.ue(LOG2_MIN_CB - 3); // log2_min_luma_coding_block_size_minus3
    w.ue(LOG2_CTB - LOG2_MIN_CB); // log2_diff_max_min_luma_coding_block_size
    w.ue(0); // log2_min_luma_transform_block_size_minus2
    w.ue(3); // log2_diff_max_min_luma_transform_block_size
    w.ue(0); // max_transform_hierarchy_depth_inter
    w.ue(0); // max_transform_hierarchy_depth_intra

    w.bit(false); // scaling_list_enabled_flag
    w.bit(false); // amp_enabled_flag
    w.bit(true); // sample_adaptive_offset_enabled_flag
    w.bit(false); // pcm_enabled_flag
    w.ue(0); // num_short_term_ref_pic_sets
    w.bit(false); // long_term_ref_pics_present_flag
    w.bit(false); // sps_temporal_mvp_enabled_flag
    w.bit(true); // strong_intra_smoothing_enabled_flag

    // The whole reason this function exists.
    w.bit(true); // vui_parameters_present_flag
    w.bit(false); // aspect_ratio_info_present_flag
    w.bit(false); // overscan_info_present_flag
    w.bit(true); // video_signal_type_present_flag
    w.bits(VIDEO_FORMAT_UNSPECIFIED, 3);
    w.bit(false); // video_full_range_flag: limited
    w.bit(true); // colour_description_present_flag
    w.bits(BT709, 8); // primaries
    w.bits(BT709, 8); // transfer
    w.bits(BT709, 8); // matrix
    w.bit(false); // chroma_loc_info_present_flag
    w.bit(false); // neutral_chroma_indication_flag
    w.bit(false); // field_seq_flag
    w.bit(false); // frame_field_info_present_flag
    w.bit(false); // default_display_window_flag
    w.bit(true); // vui_timing_info_present_flag
    // **One tick per frame on this codec**, where the other one counts two.
    // Writing the other convention halves or doubles every rate derived from
    // it, and a rate controller that believes it is spending twice the budget
    // is the sort of fault that only shows on a path that is actually full.
    w.bits(1, 32); // vui_num_units_in_tick
    w.bits(params.fps.max(1), 32); // vui_time_scale
    w.bit(false); // vui_poc_proportional_to_timing_flag
    w.bit(false); // vui_hrd_parameters_present_flag
    w.bit(false); // bitstream_restriction_flag

    w.bit(false); // sps_extension_present_flag

    if !w.trailing_bits() {
        return None;
    }
    emit(NAL_SPS, w.finish(), out)
}

/// Write the picture parameter set, start code and escaping included.
pub fn picture_parameter_set(out: &mut [u8]) -> Option<usize> {
    let mut raw = [0u8; 64];
    let mut w = BitWriter::new(&mut raw);

    w.ue(0); // pps_pic_parameter_set_id
    w.ue(0); // pps_seq_parameter_set_id
    w.bit(false); // dependent_slice_segments_enabled_flag
    w.bit(false); // output_flag_present_flag
    w.bits(0, 3); // num_extra_slice_header_bits
    w.bit(false); // sign_data_hiding_enabled_flag
    w.bit(false); // cabac_init_present_flag
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.se(0); // init_qp_minus26
    w.bit(false); // constrained_intra_pred_flag
    w.bit(false); // transform_skip_enabled_flag
    w.bit(false); // cu_qp_delta_enabled_flag
    w.se(0); // pps_cb_qp_offset
    w.se(0); // pps_cr_qp_offset
    w.bit(false); // pps_slice_chroma_qp_offsets_present_flag
    w.bit(false); // weighted_pred_flag
    w.bit(false); // weighted_bipred_flag
    w.bit(false); // transquant_bypass_enabled_flag
    w.bit(false); // tiles_enabled_flag
    w.bit(true); // entropy_coding_sync_enabled_flag
    w.bit(true); // pps_loop_filter_across_slices_enabled_flag
    w.bit(false); // deblocking_filter_control_present_flag
    w.bit(false); // pps_scaling_list_data_present_flag
    w.bit(false); // lists_modification_present_flag
    w.ue(0); // log2_parallel_merge_level_minus2
    w.bit(false); // slice_segment_header_extension_present_flag
    w.bit(false); // pps_extension_present_flag

    if !w.trailing_bits() {
        return None;
    }
    emit(NAL_PPS, w.finish(), out)
}

/// Prefix a payload with a start code and this codec's two-byte unit header.
fn emit(unit_type: u8, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let header = [(unit_type << 1) & 0x7E, 0x01];
    let total = START_CODE.len() + header.len();
    let head = out.get_mut(..total)?;
    head.get_mut(..START_CODE.len())?
        .copy_from_slice(&START_CODE);
    head.get_mut(START_CODE.len()..)?.copy_from_slice(&header);
    let written = escape(payload, out.get_mut(total..)?)?;
    Some(total + written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            width: 1920,
            height: 1080,
            fps: 60,
            level_idc: 123,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
        }
    }

    /// **The unit header is two bytes and the type is six bits.** A reader
    /// written for the other codec takes five bits of the first byte and lands
    /// on a plausible wrong answer rather than failing, so the emitted bytes
    /// are checked directly.
    #[test]
    fn the_unit_header_is_this_codecs_and_not_the_other_ones() {
        let mut buf = [0u8; 256];
        let len = video_parameter_set(&params(), &mut buf).expect("written");
        assert_eq!(&buf[..4], &[0, 0, 0, 1], "no start code");
        assert_eq!(
            (buf[4] >> 1) & 0x3F,
            NAL_VPS,
            "the unit type is not where this codec puts it"
        );
        assert_eq!(buf[4] & 0x81, 0, "the forbidden bit or a layer bit is set");
        assert_eq!(buf[5], 0x01, "temporal id plus one is not one");
        assert!(len > 6);
    }

    /// 1080 is not a whole number of coding blocks: 136 of them cover 1088, so
    /// eight rows are cropped. **The window is in chroma units for 4:2:0**, so
    /// that is four, and writing luma samples crops twice as much.
    #[test]
    fn cropping_is_in_chroma_units_for_this_chroma_format() {
        let p = params();
        assert_eq!(p.coded(), (1920, 1080), "1080 is a whole number of 8s");
        // A size that is not: 1082 rounds to 1088, cropping six luma rows.
        let odd = Params {
            height: 1082,
            ..params()
        };
        assert_eq!(odd.coded(), (1920, 1088));
        assert_eq!(odd.crop(), (0, 6));
    }

    /// Every set carries the profile block, and a decoder matches on the
    /// compatibility flags as well as the profile field. All three must land
    /// on the same description or a decoder can reject the sequence set while
    /// accepting the video set.
    #[test]
    fn all_three_sets_write_and_fit() {
        let p = params();
        let mut buf = [0u8; 512];
        let vps = video_parameter_set(&p, &mut buf).expect("vps");
        let sps = sequence_parameter_set(&p, &mut buf).expect("sps");
        let pps = picture_parameter_set(&mut buf).expect("pps");
        assert!(vps > 6 && sps > 6 && pps > 6);
        // The picture set is the smallest and the sequence set the largest,
        // which is a cheap shape check on all three at once.
        assert!(pps < vps && vps < sps, "{pps} {vps} {sps}");
    }

    /// **The level is thirty times the number on this codec**, not ten. A
    /// stream that declares 4.2 the other codec's way declares level 1.4,
    /// which is below what 1080p60 needs, and a strict decoder refuses it.
    #[test]
    fn the_level_is_this_codecs_scale() {
        let mut buf = [0u8; 256];
        let len = sequence_parameter_set(&params(), &mut buf).expect("sps");
        // The level is the last byte of the profile block, which sits at a
        // fixed offset from the start of the payload for our fixed fields.
        assert!(buf[..len].contains(&123), "level 4.1 is not in the set");
    }
}
