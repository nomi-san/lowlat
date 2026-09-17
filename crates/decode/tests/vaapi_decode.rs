//! The device decodes every committed clip to the pictures the reference
//! decoder produced. Needs a render node with the open-stack driver, so it
//! is off by default: `cargo test -p lowlat-decode --test vaapi_decode --
//! --ignored`, with `LOWLAT_VAAPI_NODE` naming the node (`renderD128`).

#![allow(clippy::type_complexity)]

mod common;

use std::collections::BTreeMap;
use std::ffi::CString;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::vaapi::{Backend, caps};
use lowlat_decode::{Decoder, Fed, Format, Planes};
use lowlat_drivers::va::Vaapi;

fn node() -> CString {
    let named = std::env::var("LOWLAT_VAAPI_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    CString::new(named).unwrap()
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

/// Decode a clip and return `(y, uv)` checksums of every picture out, in
/// output order, plus the timings.
fn decode(
    backend: &mut Backend<'_>,
    clip: &str,
    codec: Codec,
    ten_bit: bool,
) -> (Vec<(u32, u32)>, Vec<(u32, u32)>) {
    backend.build(&header(codec, ten_bit)).expect("build");
    let mut sums = Vec::new();
    let mut times = Vec::new();
    // Planes at the largest size a fixture has, with an odd pitch so a
    // pitch mistake shows.
    let pitch = 1280 * 2 + 64;
    let mut y = vec![0u8; pitch * 720];
    let mut uv = vec![0u8; pitch * 360];
    let units = common::units(clip);
    for (n, unit) in units.iter().enumerate() {
        let fed = backend
            .feed(unit)
            .unwrap_or_else(|e| panic!("{clip}: unit {n}: {e:?}"));
        if fed == Fed::FormatChanged {
            panic!("{clip}: unit {n} changed format");
        }
        if n + 1 == units.len() {
            backend.drain();
        }
        loop {
            let mut planes = Planes {
                y: &mut y,
                y_pitch: pitch,
                uv: &mut uv,
                uv_pitch: pitch,
            };
            let Some(picture) = backend
                .take(&mut planes)
                .unwrap_or_else(|e| panic!("{clip}: take: {e:?}"))
            else {
                break;
            };
            let sample = match picture.format {
                Format::Nv12 => 1,
                Format::P010 => 2,
            };
            let w = picture.width as usize * sample;
            let h = picture.height as usize;
            let mut yb = Vec::with_capacity(w * h);
            for row in 0..h {
                yb.extend_from_slice(&y[row * pitch..row * pitch + w]);
            }
            let mut uvb = Vec::with_capacity(w * h / 2);
            for row in 0..h / 2 {
                uvb.extend_from_slice(&uv[row * pitch..row * pitch + w]);
            }
            sums.push((common::crc32(&yb), common::crc32(&uvb)));
            times.push((backend.decode_us, backend.readback_us));
        }
    }
    backend.destroy();
    (sums, times)
}

fn check(clip: &str, sums_name: &str, codec: Codec, ten_bit: bool) {
    let va = Vaapi::load().expect("runtime");
    let display = va.open(&node()).expect("render node");
    let caps = caps(&display).expect("caps");
    let able = match (codec, ten_bit) {
        (Codec::H264, _) => caps.h264,
        (Codec::H265, false) => caps.hevc,
        (Codec::H265, true) => caps.hevc_10,
    };
    if !able {
        println!("{clip}: the device does not decode this profile, skipped");
        return;
    }
    let mut backend = Backend::new(&display, (4096, 4096));
    let (ours, times) = decode(&mut backend, clip, codec, ten_bit);
    let theirs = common::sums(sums_name);
    // Output order is a presentation matter (a stream that says nothing
    // about reordering may leave a picture late); the pictures themselves
    // must all be there and all be right.
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
    let readback_max = times.iter().map(|t| t.1).max().unwrap_or(0);
    let decode_mean = times.iter().map(|t| u64::from(t.0)).sum::<u64>() / times.len().max(1) as u64;
    let readback_mean =
        times.iter().map(|t| u64::from(t.1)).sum::<u64>() / times.len().max(1) as u64;
    println!(
        "{clip}: {} of {} pictures, {wrong} wrong; decode mean {decode_mean} us max {decode_max}; readback mean {readback_mean} us max {readback_max}",
        ours.len(),
        theirs.len()
    );
    assert_eq!(ours.len(), theirs.len(), "{clip}: picture count");
    assert_eq!(
        got, expected,
        "{clip}: the pictures differ from the reference decoder's"
    );
}

#[test]
#[ignore = "requires the open-stack driver"]
fn the_synthetic_clips_decode_to_the_reference_pictures() {
    check(
        "synthetic-720p-h264.bin",
        "synthetic-720p-h264.sums",
        Codec::H264,
        false,
    );
    check(
        "synthetic-720p-hevc.bin",
        "synthetic-720p-hevc.sums",
        Codec::H265,
        false,
    );
    check(
        "synthetic-720p-hevc10.bin",
        "synthetic-720p-hevc10.sums",
        Codec::H265,
        true,
    );
}

#[test]
#[ignore = "requires the open-stack driver"]
fn every_h264_fixture_decodes_to_the_reference_pictures() {
    for (clip, sums) in common::fixtures("h264") {
        check(&clip, &sums, Codec::H264, false);
    }
}

#[test]
#[ignore = "requires the open-stack driver"]
fn every_hevc_fixture_decodes_to_the_reference_pictures() {
    for (clip, sums) in common::fixtures("hevc") {
        let ten_bit = clip.contains("main10");
        check(&clip, &sums, Codec::H265, ten_bit);
    }
}
