//! The H.264 reader over every committed clip, with no device: every unit
//! reads, every picture is placed, and what leaves the buffer is what the
//! reference decoder produced, picture for picture, in order.

mod common;

use lowlat_decode::h264::dpb::Structure;
use lowlat_decode::h264::{Read, Stream};

/// Run a clip through the reader, returning the order counts of the
/// pictures as they left the buffer.
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
            // The header must end before the unit does, and on the unit's
            // own bytes.
            assert!(
                slice.header.header_bits / 8 < slice.len,
                "{clip}: unit {n} header overruns"
            );
            let active0 = usize::from(slice.header.num_ref_idx_l0_active_minus1) + 1;
            if !slice.header.slice_type.is_intra() {
                assert!(
                    slice.l0.len >= 1 && slice.l0.len <= active0,
                    "{clip}: unit {n} list 0 has {} entries for {active0} active",
                    slice.l0.len
                );
            }
            if slice.header.slice_type.is_b() {
                assert!(slice.l1.len >= 1, "{clip}: unit {n} list 1 is empty");
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
    for (clip, sums) in common::fixtures("h264") {
        let order = run(&clip);
        let expected = common::sums(&sums).len();
        assert_eq!(
            order.len(),
            expected,
            "{clip}: {} pictures out of {expected}",
            order.len()
        );
        // Within a refresh period the order counts rise; a refresh restarts
        // them.
        let mut last = i32::MIN;
        let mut restarts = 0;
        for &poc in &order {
            if poc < last {
                restarts += 1;
                last = poc;
            } else {
                last = poc;
            }
        }
        assert!(
            restarts <= 3,
            "{clip}: output order broke {restarts} times: {order:?}"
        );
    }
}

#[test]
fn the_synthetic_clip_reads_with_one_reference_and_no_reordering() {
    let clip = "synthetic-720p-h264.bin";
    let mut stream = Stream::new();
    let units = common::units(clip);
    assert_eq!(units.len(), 120);
    let mut pictures = 0;
    for unit in &units {
        assert_eq!(stream.read(unit).unwrap(), Read::Picture);
        let job = stream.job().unwrap();
        assert_eq!(job.current.structure, Structure::Frame);
        assert_eq!(job.sps.visible(), (1280, 720));
        assert_eq!(job.sps.max_num_ref_frames, 1);
        assert_eq!(stream.dpb.reorder(), 0, "the stream declares no reordering");
        stream.finish().unwrap();
        // Every picture leaves as soon as it is decoded.
        let out = stream.next_output().expect("a picture out per picture in");
        stream.dpb.taken(out.slot);
        assert!(stream.next_output().is_none());
        pictures += 1;
    }
    assert_eq!(pictures, 120);
}

/// The vendor encoder left to its defaults reorders nothing and says
/// nothing about it, under order counts that follow the frame number. Every
/// picture leaves as soon as it is decoded, before the frame number wraps
/// and after: the count carries on rising, where one that restarted would
/// read as a picture belonging before those already out.
#[test]
fn a_stream_silent_about_reordering_leaves_as_it_arrives_past_a_frame_number_wrap() {
    let clip = "fixtures/h264-nvenc-wrap.bin";
    let units = common::units(clip);
    let mut stream = Stream::new();
    let mut last = None;
    for (n, unit) in units.iter().enumerate() {
        assert_eq!(
            stream.read(unit).unwrap(),
            Read::Picture,
            "{clip}: unit {n}"
        );
        if n == 0 {
            // What makes the clip this case, checked so that a regenerated
            // clip which stopped being it fails here.
            let sps = &stream.job().unwrap().sps;
            assert_eq!(sps.pic_order_cnt_type, 2, "{clip}");
            assert_eq!(sps.reorder_frames(), None, "{clip} states its reordering");
            assert!(
                units.len() > sps.max_frame_num() as usize + 16,
                "{clip}: {} pictures do not pass the wrap at {}",
                units.len(),
                sps.max_frame_num()
            );
        }
        stream.finish().unwrap();
        let out = stream.next_output().unwrap_or_else(|| {
            panic!(
                "{clip}: unit {n} held its picture back, reorder depth {}",
                stream.dpb.reorder()
            )
        });
        stream.dpb.taken(out.slot);
        assert!(stream.next_output().is_none(), "{clip}: unit {n}");
        if let Some(last) = last {
            assert!(
                out.poc > last,
                "{clip}: unit {n} counted {} after {last}",
                out.poc
            );
        }
        last = Some(out.poc);
    }
}

/// The range a renderer needs is read from the parameter set: the clip an
/// encoder was told to make in the full range reads as such, and every other
/// as the video range, which is also what a set that says nothing means.
#[test]
fn the_full_range_is_read_from_the_parameter_set() {
    let mut clips: Vec<String> = common::fixtures("h264")
        .into_iter()
        .map(|(clip, _)| clip)
        .collect();
    clips.push("synthetic-720p-h264.bin".to_string());
    let mut full = 0;
    for clip in &clips {
        let mut stream = Stream::new();
        let units = common::units(clip);
        assert_eq!(stream.read(&units[0]).unwrap(), Read::Picture, "{clip}");
        let expected = clip.contains("full-range");
        assert_eq!(
            stream.active_sps().unwrap().vui.video_full_range,
            expected,
            "{clip}"
        );
        full += usize::from(expected);
    }
    assert_eq!(full, 1, "the full-range clip was not among {clips:?}");
}

#[test]
fn a_unit_without_parameter_sets_first_is_refused_not_decoded() {
    let clip = "synthetic-720p-h264.bin";
    let units = common::units(clip);
    let mut stream = Stream::new();
    // The second unit names a picture set the stream has not carried.
    assert!(stream.read(&units[1]).is_err());
    // Then the keyframe arrives and everything is fine.
    assert_eq!(stream.read(&units[0]).unwrap(), Read::Picture);
}

#[test]
fn a_truncated_unit_is_refused_not_a_panic() {
    let units = common::units("synthetic-720p-h264.bin");
    let mut stream = Stream::new();
    for cut in [1usize, 5, 8, 12, 20, 40] {
        let short = &units[0][..units[0].len().min(cut)];
        let _ = stream.read(short);
    }
    for cut in 1..units[0].len().min(200) {
        let mut stream = Stream::new();
        let _ = stream.read(&units[0][..cut]);
    }
}
