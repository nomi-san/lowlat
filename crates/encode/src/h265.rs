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
pub const PROFILE_MAIN: u32 = 1;

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
pub const LOG2_CTB: u32 = 6;
pub const LOG2_MIN_CB: u32 = 3;

/// What the coded picture size is rounded up to.
///
/// **Not the standard's minimum coding block.** Eight is legal and the device
/// codes at sixteen regardless, rewriting the size it was told; a set that
/// declares the smaller number has it corrected in place while the conformance
/// window written for it stays, so the picture comes out with the rounding
/// still on it and no crop for it.
const CODED_ALIGN: u32 = 16;

/// Transform block sizes, as powers of two.
pub const LOG2_MIN_TB: u32 = 2;
pub const LOG2_MAX_TB: u32 = 5;

/// How far a transform may be split below the coding unit.
pub const TRANSFORM_HIERARCHY_DEPTH: u32 = 4;

// **These sets describe what the hardware will actually do, not what we would
// prefer**, which is why the block sizes and tool flags above are public: the
// device is told them separately and the two answers have to be the same
// value. A coding tool declared off that the encoder uses anyway produces a
// bitstream a decoder reads with the wrong syntax, and it reports a decode
// failure rather than a mismatch.

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
    /// The coded size, which is what the device actually encodes.
    ///
    /// **The conformance window crops the difference**, and it is measured in
    /// chroma units for a 4:2:0 stream, so a crop counted here in luma samples
    /// is halved where it is written.
    pub fn coded(&self) -> (u32, u32) {
        (
            self.width.div_ceil(CODED_ALIGN) * CODED_ALIGN,
            self.height.div_ceil(CODED_ALIGN) * CODED_ALIGN,
        )
    }

    /// Coding tree blocks in a picture, which is what one slice covers.
    ///
    /// Counted over the coded size rather than the visible one: the last row
    /// of blocks is whole even where the picture it carries is not.
    pub fn ctus(&self) -> u32 {
        let size = 1 << LOG2_CTB;
        let (coded_width, coded_height) = self.coded();
        coded_width.div_ceil(size) * coded_height.div_ceil(size)
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
    w.ue(LOG2_MIN_TB - 2); // log2_min_luma_transform_block_size_minus2
    w.ue(LOG2_MAX_TB - LOG2_MIN_TB); // log2_diff_max_min_luma_transform_block_size
    w.ue(TRANSFORM_HIERARCHY_DEPTH); // max_transform_hierarchy_depth_inter
    w.ue(TRANSFORM_HIERARCHY_DEPTH); // max_transform_hierarchy_depth_intra

    w.bit(false); // scaling_list_enabled_flag
    w.bit(true); // amp_enabled_flag
    w.bit(true); // sample_adaptive_offset_enabled_flag
    w.bit(false); // pcm_enabled_flag
    // **No set is stored here**, so every predicted slice carries its own
    // inline. One reference and one delta is shorter written out than a table
    // lookup would be, and it keeps the set and the slice that uses it in one
    // place rather than two that have to agree.
    w.ue(0); // num_short_term_ref_pic_sets
    w.bit(false); // long_term_ref_pics_present_flag
    w.bit(false); // sps_temporal_mvp_enabled_flag
    w.bit(false); // strong_intra_smoothing_enabled_flag

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
    w.bit(true); // transform_skip_enabled_flag
    // **Rate control has no other handle on this codec.** The other one lets
    // every block carry a quantiser delta unconditionally; here the delta only
    // exists if this flag turns it on, so a stream without it is stuck at the
    // slice quantiser and the configured bitrate does nothing.
    w.bit(true); // cu_qp_delta_enabled_flag
    w.ue(0); // diff_cu_qp_delta_depth
    w.se(0); // pps_cb_qp_offset
    w.se(0); // pps_cr_qp_offset
    w.bit(false); // pps_slice_chroma_qp_offsets_present_flag
    w.bit(false); // weighted_pred_flag
    w.bit(false); // weighted_bipred_flag
    w.bit(false); // transquant_bypass_enabled_flag
    w.bit(false); // tiles_enabled_flag
    // **Wavefront parallelism would put entry point offsets in every slice
    // header**, and those are byte counts into slice data this side never
    // sees. One slice per picture needs none of it.
    w.bit(false); // entropy_coding_sync_enabled_flag
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

    /// 1080 is not a whole number of coded blocks: the picture is coded at
    /// 1088 and eight rows are cropped. **The window is in chroma units for
    /// 4:2:0**, so that is four, and writing luma samples crops twice as much.
    ///
    /// **The rounding is the device's sixteen, not the standard's eight.** At
    /// eight, 1080 needs no crop at all, and a set saying so is exactly what
    /// produced a picture eight rows too tall: the device codes 1088 either
    /// way and corrects the size in the set, leaving nothing to crop it.
    #[test]
    fn cropping_is_in_chroma_units_and_rounds_the_way_the_device_codes() {
        let p = params();
        assert_eq!(p.coded(), (1920, 1088), "1080 was not rounded up");
        assert_eq!(p.crop(), (0, 8), "the eight rows are not being cropped");

        // A size already whole needs no cropping at all.
        let whole = Params {
            height: 1200,
            ..params()
        };
        assert_eq!(whole.coded(), (1920, 1200));
        assert_eq!(whole.crop(), (0, 0));
    }

    /// One slice covers the whole picture, so the count is over the coded size
    /// and the last row of blocks is whole even where the picture is not.
    #[test]
    fn the_block_count_is_over_the_coded_size() {
        assert_eq!(params().ctus(), 30 * 17, "1088 rows is 17 blocks, not 16");
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

    /// The two kinds of slice are carried in different unit types, and a
    /// decoder classifies a refresh from that type alone.
    #[test]
    fn a_slice_header_names_its_own_unit_type() {
        let mut buf = [0u8; 64];
        let refresh = slice_header(&params(), Picture::Refresh, &mut buf).expect("refresh");
        assert_eq!((buf[4] >> 1) & 0x3F, NAL_IDR_N_LP);
        assert_eq!(buf[5], 0x01);

        let predicted =
            slice_header(&params(), Picture::Predicted { poc_lsb: 1 }, &mut buf).expect("trail");
        assert_eq!((buf[4] >> 1) & 0x3F, NAL_TRAIL_R);

        // A predicted slice carries a picture order count and a reference set
        // that a refresh does not, so it cannot be the shorter of the two.
        assert!(
            predicted.bytes_written > refresh.bytes_written,
            "{predicted:?} {refresh:?}"
        );
    }

    /// **Byte aligned, and the length is reported in bits.** The alignment
    /// element belongs to the header on this codec, so a length that is not a
    /// whole number of bytes means the element was not written and the slice
    /// data would start mid-byte.
    /// **Both headers, byte for byte, as an external parser read them.**
    ///
    /// The tests around this one check shape, and shape is not enough here:
    /// every field is one to three bits, so a wrong flag, a missing alignment
    /// element, or two fields written in the wrong order all produce a header
    /// of the same length that a decoder reads as different values. These
    /// bytes were confirmed field by field against an independent parser, and
    /// a change to the header has to be re-confirmed the same way rather than
    /// by updating the constants.
    #[test]
    fn the_headers_are_the_bytes_an_external_parser_agreed_with() {
        let mut buf = [0u8; 64];

        let refresh = slice_header(&params(), Picture::Refresh, &mut buf).expect("refresh");
        assert_eq!(
            &buf[..refresh.bytes_written],
            &[0x00, 0x00, 0x00, 0x01, 0x28, 0x01, 0xaf, 0xe0]
        );
        assert_eq!(refresh.bit_length, refresh.bytes_written * 8);

        let predicted =
            slice_header(&params(), Picture::Predicted { poc_lsb: 1 }, &mut buf).expect("trail");
        assert_eq!(
            &buf[..predicted.bytes_written],
            &[0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0xd0, 0x09, 0x7d, 0xe0]
        );
        assert_eq!(predicted.bit_length, predicted.bytes_written * 8);
    }

    /// The count is written at the width the sequence set declared, so the two
    /// have to be read from the same parameters or every field after it moves.
    #[test]
    fn the_order_count_is_written_at_the_declared_width() {
        let mut narrow = params();
        narrow.log2_max_poc_lsb_minus4 = 0;
        let mut wide = params();
        wide.log2_max_poc_lsb_minus4 = 8;

        let mut buf = [0u8; 64];
        let short = slice_header(&narrow, Picture::Predicted { poc_lsb: 1 }, &mut buf)
            .expect("narrow")
            .bit_length;
        let long = slice_header(&wide, Picture::Predicted { poc_lsb: 1 }, &mut buf)
            .expect("wide")
            .bit_length;
        assert!(long > short, "the declared width did not reach the header");
    }

    #[test]
    fn a_short_buffer_refuses_rather_than_truncating() {
        let mut out = [0u8; 5];
        assert!(video_parameter_set(&params(), &mut out).is_none());
        assert!(sequence_parameter_set(&params(), &mut out).is_none());
        assert!(picture_parameter_set(&mut out).is_none());
        assert!(slice_header(&params(), Picture::Refresh, &mut out).is_none());
    }

    /// Write the three sets and both slice headers somewhere an independent
    /// parser can read them.
    ///
    /// **The tests above check structure, not meaning.** They agree with the
    /// writer because both came from the same reading of the standard. What
    /// settles whether a decoder reads these the way they are intended is an
    /// external parser, and this is the step that feeds it.
    #[test]
    #[ignore = "writes a file for an external parser"]
    fn dump_for_an_external_parser() {
        let p = params();
        let mut buf = [0u8; 512];
        let mut bytes = Vec::new();
        let mut take = |len: usize, buf: &[u8]| bytes.extend_from_slice(&buf[..len]);

        take(video_parameter_set(&p, &mut buf).expect("vps"), &buf);
        take(sequence_parameter_set(&p, &mut buf).expect("sps"), &buf);
        take(picture_parameter_set(&mut buf).expect("pps"), &buf);
        let refresh = slice_header(&p, Picture::Refresh, &mut buf).expect("refresh");
        take(refresh.bytes_written, &buf);
        let predicted = slice_header(&p, Picture::Predicted { poc_lsb: 1 }, &mut buf).expect("p");
        take(predicted.bytes_written, &buf);

        let path = std::env::var("LOWLAT_DUMP").unwrap_or_else(|_| "/tmp/params.h265".into());
        std::fs::write(&path, &bytes).expect("write");
        println!("wrote {path}, {} bytes", bytes.len());
    }
}

/// A slice header, and the exact number of bits it occupies.
///
/// **Byte aligned on this codec**, unlike the other one. The header ends with
/// an alignment element of its own, so the slice data that follows starts on a
/// byte boundary and the bit length is always the byte count times eight.
#[derive(Debug, Clone, Copy)]
pub struct SliceHeader {
    pub bytes_written: usize,
    pub bit_length: usize,
}

/// Which kind of picture a slice belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picture {
    /// An instantaneous refresh: decodable alone, and it restarts the count.
    Refresh,
    /// Predicted from the picture immediately before it.
    Predicted { poc_lsb: u32 },
}

impl Picture {
    /// The unit type a slice of this kind is carried in.
    pub fn unit_type(self) -> u8 {
        match self {
            Self::Refresh => NAL_IDR_N_LP,
            Self::Predicted { .. } => NAL_TRAIL_R,
        }
    }

    /// Intra or predicted, in the numbering the slice header uses.
    ///
    /// **Two is intra here and zero is bidirectional**, which is the other
    /// codec's numbering inverted. Carrying the other one over declares an
    /// intra picture predicted and a decoder reads reference syntax that was
    /// never written.
    pub fn slice_type(self) -> u8 {
        match self {
            Self::Refresh => 2,
            Self::Predicted { .. } => 1,
        }
    }
}

/// Merge candidates a predicted slice may choose from.
///
/// The header carries five minus this, so five writes a zero.
pub const MAX_NUM_MERGE_CAND: u32 = 5;

/// Write the slice header for one picture.
///
/// **Every field here has a counterpart the device is told separately**, and
/// the device codes the slice data from its copy while a decoder reads this
/// one. The two must agree value for value: this side writes the header and
/// never the data, so a disagreement is not caught anywhere before the
/// decoder.
pub fn slice_header(params: &Params, picture: Picture, out: &mut [u8]) -> Option<SliceHeader> {
    let mut raw = [0u8; 64];
    let mut w = BitWriter::new(&mut raw);

    w.bit(true); // first_slice_segment_in_pic_flag
    if picture == Picture::Refresh {
        // Present on every refresh unit type, and on no other.
        w.bit(false); // no_output_of_prior_pics_flag
    }
    w.ue(0); // slice_pic_parameter_set_id
    w.ue(u32::from(picture.slice_type()));

    if let Picture::Predicted { poc_lsb } = picture {
        // Fixed width, and the width is what the sequence set declared. A
        // refresh carries no count at all: it restarts the order by being one.
        w.bits(poc_lsb, params.log2_max_poc_lsb_minus4 + 4);
        // **The reference set is written out here rather than indexed**,
        // because the sequence set stores none. One negative reference, one
        // picture back, and it is used by this picture.
        w.bit(false); // short_term_ref_pic_set_sps_flag
        w.ue(1); // num_negative_pics
        w.ue(0); // num_positive_pics
        w.ue(0); // delta_poc_s0_minus1[0]: the picture immediately before
        w.bit(true); // used_by_curr_pic_s0_flag[0]
    }

    // Present because the sequence set enables the offset filter.
    w.bit(true); // slice_sao_luma_flag
    w.bit(true); // slice_sao_chroma_flag

    if let Picture::Predicted { .. } = picture {
        // One reference, which is what the picture set already defaults to.
        w.bit(false); // num_ref_idx_active_override_flag
        w.ue(5u32.saturating_sub(MAX_NUM_MERGE_CAND)); // five_minus_max_num_merge_cand
    }

    w.se(0); // slice_qp_delta, from the picture set's initial quantiser

    // Present because the picture set enables it and the offset filter is on.
    w.bit(true); // slice_loop_filter_across_slices_enabled_flag

    // **The alignment belongs to the header on this codec**, so it is written
    // here and the slice data starts on the next byte.
    if !w.trailing_bits() {
        return None;
    }
    let bytes_written = emit(picture.unit_type(), w.finish(), out)?;
    Some(SliceHeader {
        bytes_written,
        // Whole bytes by construction, escaping included: the device is told
        // the length of the buffer it was handed, not of what was written into
        // it before escaping.
        bit_length: bytes_written * 8,
    })
}
