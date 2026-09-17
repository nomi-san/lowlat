//! What the encoders write, the readers read back to the same values: the
//! parameter sets and the slice headers of both codecs, checked field by
//! field rather than by decoding.

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
