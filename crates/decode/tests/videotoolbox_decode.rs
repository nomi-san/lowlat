//! The system's decoder on macOS decodes every committed clip to the pictures
//! the reference decoder produced, **in the stream's order**, through the
//! hardware where the Mac has it and the system's software decoder alike;
//! keeps its session across a rebuild that brings the same parameter sets
//! back; follows a change of size mid-stream; says what it decodes.
//!
//! The system's decoder is on every Mac, so these run by default.

#![cfg(target_os = "macos")]
#![allow(clippy::type_complexity)]

mod common;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::videotoolbox::{Backend, caps};
use lowlat_decode::{Decoder, Fed, Format, Planes};
use lowlat_drivers::vt::{self, Vt};

/// The largest unit a clip carries: room for every committed clip's.
const UNIT_BYTES: usize = 1 << 22;
/// The one clip the system's software decoder decodes wrongly: four slices
/// a picture and B-frames, which no host sends (12 of its 24 pictures, every
/// one right through the hardware, measured on an M2 under macOS 26).
const WRONG_IN_SOFTWARE: &str = "fixtures/h264-slices.bin";

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

fn frameworks() -> &'static Vt {
    vt::load().expect("the system's video frameworks")
}

/// Every committed clip, with its codec and depth.
fn clips() -> Vec<(String, String, Codec, bool)> {
    let mut out: Vec<(String, String, Codec, bool)> = [
        ("synthetic-720p-h264", Codec::H264, false),
        ("synthetic-720p-hevc", Codec::H265, false),
        ("synthetic-720p-hevc10", Codec::H265, true),
    ]
    .into_iter()
    .map(|(n, c, t)| (format!("{n}.bin"), format!("{n}.sums"), c, t))
    .collect();
    for (clip, sums) in common::fixtures("h264") {
        out.push((clip, sums, Codec::H264, false));
    }
    for (clip, sums) in common::fixtures("hevc") {
        let ten_bit = clip.contains("main10") || clip.contains("-10");
        out.push((clip, sums, Codec::H265, ten_bit));
    }
    out
}

/// The CRC of `rows` rows of `row` bytes, `pitch` apart.
fn crc_rows(plane: &[u8], pitch: usize, row: usize, rows: usize) -> u32 {
    let mut bytes = Vec::with_capacity(row * rows);
    for r in 0..rows {
        bytes.extend_from_slice(&plane[r * pitch..r * pitch + row]);
    }
    common::crc32(&bytes)
}

/// The CRC the reference takes of chroma interleaved at full size: every
/// first component's row, then every second's, as three planes would lie.
fn crc_interleaved(plane: &[u8], pitch: usize, width: usize, sample: usize, rows: usize) -> u32 {
    let mut bytes = Vec::with_capacity(width * sample * rows * 2);
    for component in 0..2 {
        for r in 0..rows {
            let row = &plane[r * pitch..r * pitch + 2 * width * sample];
            for x in 0..width {
                let at = (2 * x + component) * sample;
                bytes.extend_from_slice(&row[at..at + sample]);
            }
        }
    }
    common::crc32(&bytes)
}

/// Decode `units` into planes, the last drained, rebuilding at a change of
/// format as the client's feed does; the pictures' sums in the order they
/// came out.
fn decode_units(
    backend: &mut Backend,
    name: &str,
    codec: Codec,
    ten_bit: bool,
    units: &[Vec<u8>],
) -> Vec<(u32, u32)> {
    let mut sums = Vec::new();
    let pitch = 1280 * 4 + 64;
    let (mut y, mut uv) = (vec![0u8; pitch * 720], vec![0u8; pitch * 720]);
    for (n, unit) in units.iter().enumerate() {
        let mut fed = backend
            .feed(unit)
            .unwrap_or_else(|e| panic!("{name}: unit {n}: {e:?}"));
        if fed == Fed::FormatChanged {
            backend.destroy();
            backend.build(&header(codec, ten_bit)).expect("rebuild");
            fed = backend
                .feed(unit)
                .unwrap_or_else(|e| panic!("{name}: unit {n} again: {e:?}"));
            assert_ne!(fed, Fed::FormatChanged, "{name}: unit {n} changed twice");
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
                v: &mut [],
                v_pitch: 0,
            };
            let Some(picture) = backend
                .take(&mut planes)
                .unwrap_or_else(|e| panic!("{name}: take: {e:?}"))
            else {
                break;
            };
            assert_eq!(
                picture.full_range,
                name.contains("full-range"),
                "{name}: picture {} is in the wrong range",
                sums.len()
            );
            let format = picture.format;
            assert!(
                !format.planar_chroma(),
                "{name}: the system's decoder hands out two planes"
            );
            let (width, rows) = (picture.width as usize, picture.height as usize);
            let luma = crc_rows(&y, pitch, width * format.sample(), rows);
            let chroma = if format.full_chroma() {
                crc_interleaved(&uv, pitch, width, format.sample(), rows)
            } else {
                crc_rows(
                    &uv,
                    pitch,
                    width * format.sample(),
                    format.chroma_rows(rows),
                )
            };
            sums.push((luma, chroma));
        }
    }
    sums
}

/// Compare what was decoded with the reference, in order; `Err` says how
/// it differs. **The one clip that reorders without saying how deep** is
/// judged by its pictures alone, as every backend whose order is this
/// crate's reader's is: the reader learns the depth from the picture that
/// proves it, which leaves that picture late, never lost or wrong. No host
/// reorders at all.
fn compare(name: &str, ours: &[(u32, u32)], sums: &[&str], how: &str) -> Result<(), String> {
    let mut theirs = Vec::new();
    for s in sums {
        let mut one = common::sums(s);
        one.sort_by_key(|s| s.picture);
        theirs.extend(one.into_iter().map(|s| (s.y, s.uv)));
    }
    if name.contains("nvenc-bframes") {
        let (mut a, mut b) = (ours.to_vec(), theirs.clone());
        a.sort_unstable();
        b.sort_unstable();
        let late = ours.iter().zip(&theirs).filter(|(o, t)| o != t).count();
        println!(
            "  {name} by {how}: {} of {} pictures, {late} at another place, judged by the pictures",
            ours.len(),
            theirs.len()
        );
        return if a == b {
            Ok(())
        } else {
            Err(format!(
                "{} pictures of {}, not the reference's",
                ours.len(),
                theirs.len()
            ))
        };
    }
    let wrong = ours.iter().zip(&theirs).filter(|(o, t)| o != t).count();
    println!(
        "  {name} by {how}: {} of {} pictures, {wrong} not the reference's at their place",
        ours.len(),
        theirs.len()
    );
    if ours.len() != theirs.len() || wrong > 0 {
        return Err(format!(
            "{} pictures of {}, {wrong} wrong or out of place",
            ours.len(),
            theirs.len()
        ));
    }
    Ok(())
}

/// Decode every clip through `make`'s backends, the software decoder alone
/// where `software` says; the clips that differ.
fn decode_every_clip(make: impl Fn() -> Backend, software: bool) -> Vec<String> {
    let mut failed = Vec::new();
    for (clip, sums, codec, ten_bit) in clips() {
        let mut backend = make();
        backend.build(&header(codec, ten_bit)).expect("build");
        let ours = decode_units(&mut backend, &clip, codec, ten_bit, &common::units(&clip));
        let how = match backend.hardware() {
            Some(true) => "the hardware",
            Some(false) => "the software decoder",
            None => "no session",
        };
        if software {
            assert_eq!(
                backend.hardware(),
                Some(false),
                "{clip}: the hardware decoded it"
            );
        }
        backend.destroy();
        if backend.hardware() == Some(false) && clip == WRONG_IN_SOFTWARE {
            println!(
                "  {clip} by {how}: not judged, the system's software decoder decodes it wrongly"
            );
            continue;
        }
        if let Err(e) = compare(&clip, &ours, &[&sums], how) {
            failed.push(format!("{clip}: {e}"));
        }
    }
    failed
}

/// **Every clip comes out bit for bit and in the stream's order**, through
/// whichever decoder the system chooses for it: the B-frame clips in display
/// order though the decoder gives them back in decode order, full chroma in
/// two planes at both depths, the cropped clips at their visible size.
#[test]
fn every_clip_decodes_to_the_reference_pictures_in_order() {
    let vt = frameworks();
    let failed = decode_every_clip(|| Backend::new(vt, UNIT_BYTES), false);
    assert!(failed.is_empty(), "{failed:#?}");
}

/// **The system's software decoder does the same**, the hardware refused:
/// every clip but the one it decodes wrongly, which is skipped and said.
#[test]
fn the_software_decoder_decodes_every_clip_in_order() {
    let vt = frameworks();
    let failed = decode_every_clip(|| Backend::software_only(vt, UNIT_BYTES), true);
    assert!(failed.is_empty(), "{failed:#?}");
}

/// **A rebuild that brings the same parameter sets back keeps the session**,
/// and one with other sets makes another: a clip decoded, the decoder torn
/// down and built again as a host's repeated sets make the client do, the
/// clip again; then a clip of the other depth.
#[test]
fn a_rebuild_with_the_same_sets_keeps_the_session() {
    let vt = frameworks();
    let mut backend = Backend::new(vt, UNIT_BYTES);
    let clip = "fixtures/hevc-nvenc-ll";
    for pass in 0..2 {
        backend.build(&header(Codec::H265, false)).expect("build");
        let units = common::units(&format!("{clip}.bin"));
        let ours = decode_units(&mut backend, clip, Codec::H265, false, &units);
        compare(clip, &ours, &[&format!("{clip}.sums")], "this session").expect("in order");
        backend.destroy();
        assert_eq!(backend.sessions(), 1, "pass {pass} made another session");
    }
    let clip = "fixtures/hevc-nvenc-main10";
    backend.build(&header(Codec::H265, true)).expect("build");
    let units = common::units(&format!("{clip}.bin"));
    let ours = decode_units(&mut backend, clip, Codec::H265, true, &units);
    compare(clip, &ours, &[&format!("{clip}.sums")], "a new session").expect("in order");
    assert_eq!(backend.sessions(), 2, "other sets kept the session");
}

/// **A change of size mid-stream is followed**: two clips of different
/// sizes one after the other, every picture of both at its place.
#[test]
fn a_change_of_size_mid_stream_is_followed() {
    let vt = frameworks();
    for (first, second, codec) in [
        ("h264-ipp-cabac", "h264-nvenc-ll", Codec::H264),
        ("hevc-ipp", "hevc-nvenc-ll", Codec::H265),
    ] {
        let path = |n: &str| format!("fixtures/{n}.bin");
        let mut units = common::units(&path(first));
        units.extend(common::units(&path(second)));
        let mut backend = Backend::new(vt, UNIT_BYTES);
        backend.build(&header(codec, false)).expect("build");
        let name = format!("{first} then {second}");
        let ours = decode_units(&mut backend, &name, codec, false, &units);
        backend.destroy();
        let sums = |n: &str| format!("fixtures/{n}.sums");
        compare(&name, &ours, &[&sums(first), &sums(second)], "one decoder").expect("in order");
    }
}

/// **What the system decodes is asked of it**: both codecs on every Mac the
/// library runs on, and the declaration's every combination said, with what
/// of it the hardware decodes.
#[test]
fn the_system_says_what_it_decodes() {
    let (decodes, hardware) = caps(frameworks());
    println!("decodes {decodes:?}");
    println!("by hardware {hardware:?}");
    assert!(decodes.h264 && decodes.hevc, "{decodes:?}");
    assert!(
        !hardware.hevc_10 || decodes.hevc_10,
        "the hardware decodes what the system does not"
    );
}

/// A picture's layout names its plane count truthfully: two planes for every
/// layout the system's decoder hands out.
#[test]
fn the_layouts_handed_out_are_two_planes() {
    for format in [Format::Nv12, Format::P010, Format::Nv24, Format::P410] {
        assert!(!format.planar_chroma(), "{format:?}");
    }
    assert_eq!(Format::Nv24.chroma_row_bytes(100), 200);
    assert_eq!(Format::P410.chroma_row_bytes(100), 400);
    assert_eq!(Format::Nv24.chroma_rows(100), 100);
}
