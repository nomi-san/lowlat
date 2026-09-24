//! The HEVC reader over every committed clip, with no device.

mod common;

use lowlat_decode::hevc::{Read, Stream};

fn run(clip: &str) -> Vec<i32> {
    let mut stream = Stream::new();
    let mut out = Vec::new();
    for (n, unit) in common::units(clip).iter().enumerate() {
        let read = stream
            .read(unit)
            .unwrap_or_else(|e| panic!("{clip}: unit {n} refused: {e:?}"));
        assert_eq!(read, Read::Picture, "{clip}: unit {n} read as {read:?}");
        let job = stream.job().expect("a staged picture");
        assert!(job.slices.len >= 1, "{clip}: unit {n} has no slices");
        for slice in job.slices.as_slice().iter().flatten() {
            assert!(
                (slice.header.header_bytes as usize) < slice.len,
                "{clip}: unit {n} header overruns"
            );
            if !slice.header.slice_type.is_intra() {
                assert_eq!(
                    slice.l0.len,
                    usize::from(slice.header.num_ref_idx_l0_active_minus1) + 1,
                    "{clip}: unit {n} list 0"
                );
            }
            if slice.header.slice_type.is_b() {
                assert_eq!(
                    slice.l1.len,
                    usize::from(slice.header.num_ref_idx_l1_active_minus1) + 1,
                    "{clip}: unit {n} list 1"
                );
            }
        }
        stream
            .finish()
            .unwrap_or_else(|e| panic!("{clip}: unit {n} finish: {e:?}"));
        while let Some(output) = stream.next_output() {
            out.push(output.poc);
            stream.dpb.taken(output.slot);
        }
    }
    stream.drain();
    while let Some(output) = stream.next_output() {
        out.push(output.poc);
        stream.dpb.taken(output.slot);
    }
    out
}

#[test]
fn every_fixture_reads_and_outputs_every_picture_in_order() {
    for (clip, sums) in common::fixtures("hevc") {
        let order = run(&clip);
        let expected = common::sums(&sums).len();
        assert_eq!(
            order.len(),
            expected,
            "{clip}: {} pictures out of {expected}: {order:?}",
            order.len()
        );
        // The order counts rise within a period and restart at a refresh.
        let mut last = i32::MIN;
        let mut restarts = 0;
        for &poc in &order {
            if poc < last {
                restarts += 1;
            }
            last = poc;
        }
        assert!(
            restarts <= 2,
            "{clip}: output order broke {restarts} times: {order:?}"
        );
        println!("{clip}: {order:?}");
    }
}

#[test]
fn the_synthetic_clips_read_with_no_reordering() {
    for clip in ["synthetic-720p-hevc.bin", "synthetic-720p-hevc10.bin"] {
        let mut stream = Stream::new();
        let units = common::units(clip);
        assert_eq!(units.len(), 120);
        for unit in &units {
            assert_eq!(stream.read(unit).unwrap(), Read::Picture);
            let job = stream.job().unwrap();
            assert_eq!(job.sps.visible(), (1280, 720));
            assert_eq!(stream.dpb.reorder(), 0);
            stream.finish().unwrap();
            let out = stream.next_output().expect("a picture out per picture in");
            stream.dpb.taken(out.slot);
            assert!(stream.next_output().is_none());
        }
        let ten_bit = clip.contains("10");
        assert_eq!(
            stream.active_sps().unwrap().bit_depth_luma_minus8,
            if ten_bit { 2 } else { 0 }
        );
    }
}

/// The full-chroma fixtures read as the range-extensions profile at 4:4:4,
/// the VUI both encoders write walked through to the extension flag. Neither
/// encoder writes the extension syntax itself (checked with an independent
/// header trace); the writers test builds that by hand.
#[test]
fn the_full_chroma_fixtures_read_as_the_range_extensions_profile() {
    for (clip, ten_bit) in [
        ("fixtures/hevc-444.bin", false),
        ("fixtures/hevc-444-main10.bin", true),
        ("fixtures/hevc-nvenc-444.bin", false),
        ("fixtures/hevc-nvenc-444-10.bin", true),
    ] {
        let mut stream = Stream::new();
        let units = common::units(clip);
        assert_eq!(stream.read(&units[0]).unwrap(), Read::Picture, "{clip}");
        let job = stream.job().unwrap();
        assert_eq!(job.sps.profile_idc, 4, "{clip}");
        assert_eq!(job.sps.chroma_format_idc, 3, "{clip}");
        assert!(job.sps.is_range_extended(), "{clip}");
        assert!(!job.sps.range.any(), "{clip}");
        assert_eq!(
            job.sps.bit_depth_luma_minus8,
            if ten_bit { 2 } else { 0 },
            "{clip}"
        );
        assert_eq!(job.pps.range, Default::default(), "{clip}");
    }
}

/// The range a renderer needs is read from the parameter set, at either
/// depth: the clips an encoder was told to make in the full range read as
/// such, and every other as the video range, which is also what a set that
/// says nothing means. The walk goes on through the rest of the VUI to the
/// extension flag, which the full-chroma clips check.
#[test]
fn the_full_range_is_read_from_the_parameter_set() {
    let mut clips: Vec<String> = common::fixtures("hevc")
        .into_iter()
        .map(|(clip, _)| clip)
        .collect();
    clips.push("synthetic-720p-hevc.bin".to_string());
    clips.push("synthetic-720p-hevc10.bin".to_string());
    let mut full = 0;
    for clip in &clips {
        let mut stream = Stream::new();
        let units = common::units(clip);
        assert_eq!(stream.read(&units[0]).unwrap(), Read::Picture, "{clip}");
        let expected = clip.contains("full-range");
        assert_eq!(
            stream.active_sps().unwrap().video_full_range,
            expected,
            "{clip}"
        );
        full += usize::from(expected);
    }
    assert_eq!(full, 2, "a full-range clip was not among {clips:?}");
}

#[test]
fn a_truncated_unit_is_refused_not_a_panic() {
    let units = common::units("synthetic-720p-hevc.bin");
    for cut in 1..units[0].len().min(200) {
        let mut stream = Stream::new();
        let _ = stream.read(&units[0][..cut]);
    }
}
