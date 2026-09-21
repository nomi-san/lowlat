//! The machine's own codec library decodes every committed clip to the
//! pictures the reference decoder produced, through the software backend and
//! its conversion to the four formats. Needs an LGPL pair, so it is off by
//! default: `LOWLAT_FFMPEG_DIR=<dir> cargo test -p lowlat-decode --test
//! software_decode -- --ignored`, or a machine whose own pair is LGPL.

#![allow(clippy::type_complexity)]

mod common;

use std::collections::BTreeMap;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::software::{Backend, caps};
use lowlat_decode::{Decoder, Fed, Planes};
use lowlat_drivers::lavc::Lavc;

fn pair() -> Lavc {
    Lavc::load(None).expect("an LGPL pair: name one with LOWLAT_FFMPEG_DIR")
}

/// Whether the pair opens the codec: what its capability row says.
fn decodes(lavc: &Lavc, codec: Codec) -> bool {
    let caps = caps(lavc);
    match codec {
        Codec::H264 => caps.h264,
        Codec::H265 => caps.hevc,
    }
}

fn header(codec: Codec, ten_bit: bool) -> VideoHeader {
    VideoHeader {
        frame_id: 1,
        width: 0,
        height: 0,
        codec,
        rotation: Rotation::None,
        ten_bit,
        locked: false,
        announced: false,
        metadata: false,
    }
}

fn check(lavc: &Lavc, clip: &str, sums_name: &str, codec: Codec, ten_bit: bool) {
    if !decodes(lavc, codec) {
        println!("{clip}: the pair does not open this codec, skipped");
        return;
    }
    let mut backend = Backend::new(lavc);
    backend.build(&header(codec, ten_bit)).expect("build");
    let (ours, times) = common::decode_clip(
        &mut backend,
        clip,
        |b| b.drain(),
        |b| (b.decode_us, b.readback_us),
    );
    backend.destroy();
    let theirs = common::sums(sums_name);
    // Output order is a presentation matter; the pictures themselves must
    // all be there and all be right.
    let mut expected: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &theirs {
        *expected.entry((s.y, s.uv)).or_default() += 1;
    }
    let mut got: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &ours {
        *got.entry(*s).or_default() += 1;
    }
    let wrong = ours.iter().filter(|s| !expected.contains_key(s)).count();
    let decode_max = times.iter().map(|t| t.0).max().unwrap_or(0);
    let convert_max = times.iter().map(|t| t.1).max().unwrap_or(0);
    let decode_mean = times.iter().map(|t| u64::from(t.0)).sum::<u64>() / times.len().max(1) as u64;
    let convert_mean =
        times.iter().map(|t| u64::from(t.1)).sum::<u64>() / times.len().max(1) as u64;
    println!(
        "{clip}: {} of {} pictures, {wrong} wrong; decode mean {decode_mean} us max {decode_max}; convert mean {convert_mean} us max {convert_max}",
        ours.len(),
        theirs.len()
    );
    assert_eq!(
        wrong, 0,
        "{clip}: a picture the reference decoder never produced"
    );
    // **A stream that reorders more than it declares loses a picture at
    // each depth the library discovers**: a picture that arrives for an
    // earlier place than the last one put out is dropped, the buffer
    // deepened, and the stream carries on. The one committed clip that
    // reorders with no declaration is the vendor encoder's B-frame clip;
    // this crate's own readers hold such a picture instead. The reference
    // run knew the depth from a probe of the whole file, which a stream
    // never gets.
    let undeclared = clip.contains("nvenc-bframes");
    if undeclared && ours.len() + 1 == theirs.len() {
        println!("{clip}: one picture lost at the reorder depth's discovery, as documented");
        return;
    }
    assert_eq!(ours.len(), theirs.len(), "{clip}: picture count");
    assert_eq!(
        got, expected,
        "{clip}: the pictures differ from the reference decoder's"
    );
}

#[test]
#[ignore = "needs an LGPL codec library pair"]
fn the_synthetic_clips_decode_to_the_reference_pictures() {
    let lavc = pair();
    check(
        &lavc,
        "synthetic-720p-h264.bin",
        "synthetic-720p-h264.sums",
        Codec::H264,
        false,
    );
    check(
        &lavc,
        "synthetic-720p-hevc.bin",
        "synthetic-720p-hevc.sums",
        Codec::H265,
        false,
    );
    check(
        &lavc,
        "synthetic-720p-hevc10.bin",
        "synthetic-720p-hevc10.sums",
        Codec::H265,
        true,
    );
}

#[test]
#[ignore = "needs an LGPL codec library pair"]
fn every_h264_fixture_decodes_to_the_reference_pictures() {
    let lavc = pair();
    for (clip, sums) in common::fixtures("h264") {
        check(&lavc, &clip, &sums, Codec::H264, false);
    }
}

/// Every second-codec fixture, full chroma included: the one backend that
/// takes all of them on every machine.
#[test]
#[ignore = "needs an LGPL codec library pair"]
fn every_hevc_fixture_decodes_to_the_reference_pictures() {
    let lavc = pair();
    for (clip, sums) in common::fixtures("hevc") {
        let ten_bit = clip.contains("10");
        check(&lavc, &clip, &sums, Codec::H265, ten_bit);
    }
}

/// **The output delay, in pictures, per clip.** A stream with no
/// bidirectional pictures decodes each unit to a picture at once, which is
/// what the low-delay flag and the stream's own reordering declaration are
/// for; a stream with them holds as many as its declaration says. Measured
/// as the most units ever fed ahead of the pictures out, before the drain.
#[test]
#[ignore = "needs an LGPL codec library pair"]
fn the_output_delay_is_the_streams_own() {
    let lavc = pair();
    let mut clips: Vec<(String, Codec)> = vec![
        ("synthetic-720p-h264.bin".into(), Codec::H264),
        ("synthetic-720p-hevc.bin".into(), Codec::H265),
        ("synthetic-720p-hevc10.bin".into(), Codec::H265),
    ];
    clips.extend(
        common::fixtures("h264")
            .into_iter()
            .map(|(c, _)| (c, Codec::H264)),
    );
    clips.extend(
        common::fixtures("hevc")
            .into_iter()
            .map(|(c, _)| (c, Codec::H265)),
    );
    let pitch = 1280 * 2 + 64;
    let mut y = vec![0u8; pitch * 720];
    let mut u = vec![0u8; pitch * 720];
    let mut v = vec![0u8; pitch * 720];
    for (clip, codec) in clips {
        if !decodes(&lavc, codec) {
            println!("{clip}: the pair does not open this codec, skipped");
            continue;
        }
        let ten_bit = clip.contains("10");
        let mut backend = Backend::new(&lavc);
        backend.build(&header(codec, ten_bit)).expect("build");
        let mut fed = 0usize;
        let mut out = 0usize;
        let mut deepest = 0usize;
        for unit in common::units(&clip) {
            let result = backend.feed(&unit).expect("feed");
            fed += 1;
            if result == Fed::Picture {
                loop {
                    let mut planes = Planes {
                        y: &mut y,
                        y_pitch: pitch,
                        uv: &mut u,
                        uv_pitch: pitch,
                        v: &mut v,
                        v_pitch: pitch,
                    };
                    if backend.take(&mut planes).expect("take").is_none() {
                        break;
                    }
                    out += 1;
                }
            }
            deepest = deepest.max(fed - out);
        }
        backend.destroy();
        println!("{clip}: deepest {deepest} unit(s) ahead of the pictures out");
        let no_reordering =
            clip.contains("-ll") || clip.contains("ipp") || clip.contains("synthetic");
        if no_reordering {
            assert_eq!(deepest, 0, "{clip}: a picture per unit, at once");
        }
    }
}
