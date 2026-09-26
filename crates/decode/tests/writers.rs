//! What the encoders write, the readers read back to the same values: the
//! parameter sets and the slice headers of both codecs, checked field by
//! field rather than by decoding.

// The writers live with the encoders, which are built on Linux so far.
#![cfg(target_os = "linux")]
#![allow(clippy::cast_possible_truncation)]

use lowlat_decode::h264;
use lowlat_decode::hevc;
use lowlat_decode::nal::Units;
use lowlat_encode::{h264 as write264, h265 as write265};

fn unit(bytes: &[u8]) -> &[u8] {
    Units::new(bytes).next().expect("a unit").bytes
}

#[test]
fn the_h264_parameter_sets_read_back() {
    let params = write264::Params {
        width: 1920,
        height: 1080,
        fps: 60,
        level_idc: 42,
        log2_max_frame_num_minus4: 4,
        log2_max_poc_lsb_minus4: 4,
        max_num_ref_frames: 1,
    };
    let mut out = [0u8; 256];
    let n = write264::sequence_parameter_set(&params, &mut out).unwrap();
    let sps = h264::sps::parse(&unit(&out[..n])[1..]).unwrap();
    assert_eq!(sps.profile_idc, 100);
    assert_eq!(sps.level_idc, 42);
    assert_eq!(sps.visible(), (1920, 1080));
    assert_eq!(sps.coded_height(), 1088);
    assert_eq!(sps.log2_max_frame_num_minus4, 4);
    assert_eq!(sps.pic_order_cnt_type, 0);
    assert_eq!(sps.log2_max_pic_order_cnt_lsb_minus4, 4);
    assert_eq!(sps.max_num_ref_frames, 1);
    assert!(sps.frame_mbs_only);
    assert_eq!(sps.chroma_format_idc, 1);
    // The writer states its reorder depth, and it is zero.
    assert_eq!(sps.reorder_frames(), Some(0));

    let n = write264::picture_parameter_set(&mut out).unwrap();
    let pps = h264::pps::parse(&unit(&out[..n])[1..], |id| (id == 0).then_some(sps)).unwrap();
    assert_eq!(pps.id, 0);
    assert!(pps.entropy_coding_mode);
    assert!(pps.deblocking_filter_control_present);

    // A refresh slice, then a predicted one; the header length the writer
    // reports is what the reader consumed.
    let refresh = write264::slice_header(
        &params,
        write264::Picture::Refresh { idr_pic_id: 5 },
        &mut out,
    )
    .unwrap();
    let bytes = unit(&out[..refresh.bytes_written]);
    let header = h264::slice::parse(bytes, |_| Some(pps), |_| Some(sps)).unwrap();
    assert!(header.is_idr());
    assert_eq!(header.idr_pic_id, 5);
    assert_eq!(header.frame_num, 0);
    assert!(header.slice_type.is_intra());
    // The writer counts the start code and the unit header; the reader
    // counts from the payload.
    assert_eq!(header.header_bits, refresh.bit_length - 40);

    let predicted = write264::slice_header(
        &params,
        write264::Picture::Predicted {
            frame_num: 3,
            poc_lsb: 6,
        },
        &mut out,
    )
    .unwrap();
    let bytes = unit(&out[..predicted.bytes_written]);
    let header = h264::slice::parse(bytes, |_| Some(pps), |_| Some(sps)).unwrap();
    assert!(!header.is_idr());
    assert_eq!(header.frame_num, 3);
    assert_eq!(header.pic_order_cnt_lsb, 6);
    assert_eq!(header.slice_type, h264::slice::SliceType::P);
    assert_eq!(header.num_ref_idx_l0_active_minus1, 0);
    assert_eq!(header.header_bits, predicted.bit_length - 40);
}

/// Our own writer's full-chroma sets: the range-extensions profile at 4:4:4,
/// no extension syntax, read back as such.
#[test]
fn the_hevc_full_chroma_sets_read_back() {
    for bit_depth_minus8 in [0, 2] {
        let params = write265::Params {
            width: 1920,
            height: 1080,
            fps: 60,
            level_idc: 123,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
            transform_depth: 2,
            bit_depth_minus8,
            chroma_444: true,
        };
        let mut out = [0u8; 256];
        let n = write265::sequence_parameter_set(&params, &mut out).unwrap();
        let sps = hevc::sps::parse(&unit(&out[..n])[2..]).unwrap();
        assert_eq!(sps.profile_idc, 4);
        assert_eq!(sps.chroma_format_idc, 3);
        assert!(!sps.separate_colour_plane);
        assert!(sps.is_range_extended());
        assert!(!sps.range.any());
        assert_eq!(sps.visible(), (1920, 1080));
        let n = write265::picture_parameter_set(&mut out).unwrap();
        let pps = hevc::pps::parse(&unit(&out[..n])[2..], |id| (id == 0).then_some(sps)).unwrap();
        assert_eq!(pps.range, hevc::pps::RangeExtension::default());
    }
}

/// A sequence and picture set written by hand with every range-extension
/// tool switched on, which no encoder at hand writes: the reader takes each
/// flag and field to the value written, and reads the slice header's own
/// flag that the picture set's list enables.
#[test]
fn the_range_extension_syntax_reads_field_for_field() {
    use lowlat_encode::bitstream::BitWriter;

    let mut raw = [0u8; 128];
    let mut w = BitWriter::new(&mut raw);
    w.bits(0, 4); // sps_video_parameter_set_id
    w.bits(0, 3); // sps_max_sub_layers_minus1
    w.bit(true); // sps_temporal_id_nesting_flag
    // profile_tier_level(1, 0)
    w.bits(0, 2); // general_profile_space
    w.bit(false); // general_tier_flag
    w.bits(4, 5); // general_profile_idc: range extensions
    w.bits(1 << (31 - 4), 32); // compatibility: the same
    w.bits(0b1001, 4); // progressive, interlaced, non_packed, frame_only
    w.bits(0, 32); // 43 constraint and reserved bits, then inbld
    w.bits(0, 12);
    w.bits(123, 8); // general_level_idc
    w.ue(0); // sps_seq_parameter_set_id
    w.ue(3); // chroma_format_idc
    w.bit(false); // separate_colour_plane_flag
    w.ue(64); // pic_width_in_luma_samples
    w.ue(64); // pic_height_in_luma_samples
    w.bit(false); // conformance_window_flag
    w.ue(2); // bit_depth_luma_minus8
    w.ue(2); // bit_depth_chroma_minus8
    w.ue(4); // log2_max_pic_order_cnt_lsb_minus4
    w.bit(true); // sps_sub_layer_ordering_info_present_flag
    w.ue(1); // sps_max_dec_pic_buffering_minus1
    w.ue(0); // sps_max_num_reorder_pics
    w.ue(0); // sps_max_latency_increase_plus1
    w.ue(0); // log2_min_luma_coding_block_size_minus3
    w.ue(2); // log2_diff_max_min_luma_coding_block_size
    w.ue(0); // log2_min_luma_transform_block_size_minus2
    w.ue(3); // log2_diff_max_min_luma_transform_block_size
    w.ue(1); // max_transform_hierarchy_depth_inter
    w.ue(1); // max_transform_hierarchy_depth_intra
    w.bit(false); // scaling_list_enabled_flag
    w.bit(false); // amp_enabled_flag
    w.bit(false); // sample_adaptive_offset_enabled_flag
    w.bit(false); // pcm_enabled_flag
    w.ue(0); // num_short_term_ref_pic_sets
    w.bit(false); // long_term_ref_pics_present_flag
    w.bit(false); // sps_temporal_mvp_enabled_flag
    w.bit(false); // strong_intra_smoothing_enabled_flag
    w.bit(false); // vui_parameters_present_flag
    w.bit(true); // sps_extension_present_flag
    w.bit(true); // sps_range_extension_flag
    w.bit(false); // sps_multilayer_extension_flag
    w.bit(false); // sps_3d_extension_flag
    w.bit(false); // sps_scc_extension_flag
    w.bits(0, 4); // sps_extension_4bits
    // sps_range_extension(): the nine, alternating so a shifted read shows.
    for set in [true, false, true, false, true, false, true, false, true] {
        w.bit(set);
    }
    w.trailing_bits();
    let sps = hevc::sps::parse(w.finish()).unwrap();
    assert_eq!(sps.profile_idc, 4);
    assert_eq!(sps.chroma_format_idc, 3);
    assert_eq!(
        sps.range,
        hevc::sps::RangeExtension {
            transform_skip_rotation_enabled: true,
            transform_skip_context_enabled: false,
            implicit_rdpcm_enabled: true,
            explicit_rdpcm_enabled: false,
            extended_precision_processing: true,
            intra_smoothing_disabled: false,
            high_precision_offsets_enabled: true,
            persistent_rice_adaptation_enabled: false,
            cabac_bypass_alignment_enabled: true,
        }
    );

    let mut raw = [0u8; 128];
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
    w.bit(false); // cu_qp_delta_enabled_flag
    w.se(0); // pps_cb_qp_offset
    w.se(0); // pps_cr_qp_offset
    w.bit(true); // pps_slice_chroma_qp_offsets_present_flag
    w.bit(false); // weighted_pred_flag
    w.bit(false); // weighted_bipred_flag
    w.bit(false); // transquant_bypass_enabled_flag
    w.bit(false); // tiles_enabled_flag
    w.bit(false); // entropy_coding_sync_enabled_flag
    w.bit(true); // pps_loop_filter_across_slices_enabled_flag
    w.bit(false); // deblocking_filter_control_present_flag
    w.bit(false); // pps_scaling_list_data_present_flag
    w.bit(false); // lists_modification_present_flag
    w.ue(0); // log2_parallel_merge_level_minus2
    w.bit(false); // slice_segment_header_extension_present_flag
    w.bit(true); // pps_extension_present_flag
    w.bit(true); // pps_range_extension_flag
    w.bit(false); // pps_multilayer_extension_flag
    w.bit(false); // pps_3d_extension_flag
    w.bit(false); // pps_scc_extension_flag
    w.bits(0, 4); // pps_extension_4bits
    // pps_range_extension()
    w.ue(3); // log2_max_transform_skip_block_size_minus2
    w.bit(true); // cross_component_prediction_enabled_flag
    w.bit(true); // chroma_qp_offset_list_enabled_flag
    w.ue(2); // diff_cu_chroma_qp_offset_depth
    w.ue(1); // chroma_qp_offset_list_len_minus1
    w.se(-3); // cb_qp_offset_list[0]
    w.se(5); // cr_qp_offset_list[0]
    w.se(12); // cb_qp_offset_list[1]
    w.se(-12); // cr_qp_offset_list[1]
    w.ue(1); // log2_sao_offset_scale_luma
    w.ue(2); // log2_sao_offset_scale_chroma
    w.trailing_bits();
    let pps = hevc::pps::parse(w.finish(), |id| (id == 0).then_some(sps)).unwrap();
    assert_eq!(
        pps.range,
        hevc::pps::RangeExtension {
            log2_max_transform_skip_block_size_minus2: 3,
            cross_component_prediction_enabled: true,
            chroma_qp_offset_list_enabled: true,
            diff_cu_chroma_qp_offset_depth: 2,
            chroma_qp_offset_list_len_minus1: 1,
            cb_qp_offset_list: [-3, 12, 0, 0, 0, 0],
            cr_qp_offset_list: [5, -12, 0, 0, 0, 0],
            log2_sao_offset_scale_luma: 1,
            log2_sao_offset_scale_chroma: 2,
        }
    );

    // A refresh slice header under those sets: the qp offsets, then the
    // per-unit chroma offset flag the list enables, then the rest.
    let mut raw = [0u8; 64];
    let mut w = BitWriter::new(&mut raw);
    w.bits(19 << 1, 8); // IDR_W_RADL, layer 0 high bit
    w.bits(1, 8); // layer 0, temporal id plus one
    w.bit(true); // first_slice_segment_in_pic_flag
    w.bit(false); // no_output_of_prior_pics_flag
    w.ue(0); // slice_pic_parameter_set_id
    w.ue(2); // slice_type: I
    w.se(0); // slice_qp_delta
    w.se(-1); // slice_cb_qp_offset
    w.se(1); // slice_cr_qp_offset
    w.bit(true); // cu_chroma_qp_offset_enabled_flag
    w.bit(true); // slice_loop_filter_across_slices_enabled_flag
    w.bit(true); // alignment: the header's own trailing bit
    let bytes = w.finish();
    let header = hevc::slice::parse(bytes, |_| Some(pps), |_| Some(sps), None).unwrap();
    assert!(header.is_idr());
    assert_eq!(header.slice_cb_qp_offset, -1);
    assert_eq!(header.slice_cr_qp_offset, 1);
    assert!(header.cu_chroma_qp_offset_enabled);
    assert!(header.loop_filter_across_slices_enabled);
}

#[test]
fn the_hevc_parameter_sets_read_back() {
    for bit_depth_minus8 in [0, 2] {
        let params = write265::Params {
            width: 1920,
            height: 1080,
            fps: 60,
            level_idc: 123,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
            transform_depth: 2,
            bit_depth_minus8,
            chroma_444: false,
        };
        let mut out = [0u8; 256];
        let n = write265::video_parameter_set(&params, &mut out).unwrap();
        assert!(n > 0);
        let n = write265::sequence_parameter_set(&params, &mut out).unwrap();
        let sps = hevc::sps::parse(&unit(&out[..n])[2..]).unwrap();
        assert_eq!(sps.profile_idc, if bit_depth_minus8 == 0 { 1 } else { 2 });
        assert_eq!(sps.visible(), (1920, 1080));
        assert_eq!(sps.bit_depth_luma_minus8, bit_depth_minus8 as u8);
        assert_eq!(sps.bit_depth_chroma_minus8, bit_depth_minus8 as u8);
        assert_eq!(sps.log2_max_pic_order_cnt_lsb_minus4, 4);
        assert_eq!(sps.max_num_reorder_pics, 0);
        assert_eq!(sps.chroma_format_idc, 1);
        // The writer codes its reference set in every slice header.
        assert_eq!(sps.num_short_term_ref_pic_sets, 0);

        let n = write265::picture_parameter_set(&mut out).unwrap();
        let pps = hevc::pps::parse(&unit(&out[..n])[2..], |id| (id == 0).then_some(sps)).unwrap();
        assert_eq!(pps.id, 0);

        let refresh =
            write265::slice_header(&params, write265::Picture::Refresh, &mut out).unwrap();
        let bytes = unit(&out[..refresh.bytes_written]);
        let header = hevc::slice::parse(bytes, |_| Some(pps), |_| Some(sps), None).unwrap();
        assert!(header.is_idr());
        assert!(header.first_slice_segment_in_pic);
        assert!(header.slice_type.is_intra());
        // The writer pads to the byte, so its whole length is the header.
        assert_eq!(header.header_bytes as usize + 4, refresh.bytes_written);

        let predicted = write265::slice_header(
            &params,
            write265::Picture::Predicted { poc_lsb: 6 },
            &mut out,
        )
        .unwrap();
        let bytes = unit(&out[..predicted.bytes_written]);
        let header = hevc::slice::parse(bytes, |_| Some(pps), |_| Some(sps), None).unwrap();
        assert!(!header.is_idr());
        assert_eq!(header.pic_order_cnt_lsb, 6);
        assert_eq!(header.slice_type, hevc::slice::SliceType::P);
        assert_eq!(header.st_rps.num_negative, 1);
        assert_eq!(header.st_rps.delta_s0[0], -1);
        assert!(header.st_rps.used_s0[0]);
        assert_eq!(header.header_bytes as usize + 4, predicted.bytes_written);
    }
}
