//! The system's own decoder decodes every committed clip of a shape it takes
//! to the pictures the reference decoder produced, **in the stream's order**:
//! H.264 at eight bits, HEVC at eight and ten; refuses as fatal, before the
//! decoder sees them, the streams it would hang on or write wrongly; follows
//! a change of size mid-stream. Needs the system's media framework, and its
//! HEVC extension for HEVC, so off by default: `cargo test -p lowlat-decode
//! --test mf_decode -- --ignored --nocapture`.

#![cfg(windows)]
#![allow(clippy::type_complexity)]

mod common;

use std::sync::mpsc;
use std::time::Duration;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::mf::{Backend, caps};
use lowlat_decode::{Decoder, Fault, Fed, Planes};
use lowlat_drivers::mf;

/// The largest unit a clip carries: room for every committed clip's.
const UNIT_BYTES: usize = 1 << 22;
/// How long a stream the decoder must not be given may take to be refused:
/// a refusal is at once, and a decoder given one does not come back.
const REFUSAL_WAIT: Duration = Duration::from_secs(10);

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

/// Every clip of a shape the decoder takes, with its codec and depth.
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
        if clip.contains("444") {
            continue;
        }
        let ten_bit = clip.contains("main10");
        out.push((clip, sums, Codec::H265, ten_bit));
    }
    out
}

/// Decode `units` into planes, the last drained; the pictures' sums in the
/// order they came out.
fn decode_units(backend: &mut Backend, name: &str, units: &[Vec<u8>]) -> Vec<(u32, u32)> {
    let mut sums = Vec::new();
    let pitch = 1280 * 2 + 64;
    let (mut y, mut uv) = (vec![0u8; pitch * 720], vec![0u8; pitch * 360]);
    for (n, unit) in units.iter().enumerate() {
        let fed = backend
            .feed(unit)
            .unwrap_or_else(|e| panic!("{name}: unit {n}: {e:?}"));
        assert_ne!(fed, Fed::FormatChanged, "{name}: unit {n} changed format");
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
            // A clip's name says its range, and every picture of it says the
            // same.
            assert_eq!(
                picture.full_range,
                name.contains("full-range"),
                "{name}: picture {} is in the wrong range",
                sums.len()
            );
            let row = picture.width as usize * picture.format.sample();
            let rows = picture.height as usize;
            sums.push((
                crc_rows(&y, pitch, row, rows),
                crc_rows(&uv, pitch, row, picture.format.chroma_rows(rows)),
            ));
        }
    }
    sums
}

/// The CRC of `rows` rows of `row` bytes, `pitch` apart.
fn crc_rows(plane: &[u8], pitch: usize, row: usize, rows: usize) -> u32 {
    let mut bytes = Vec::with_capacity(row * rows);
    for r in 0..rows {
        bytes.extend_from_slice(&plane[r * pitch..r * pitch + row]);
    }
    common::crc32(&bytes)
}

/// Compare what was decoded with the reference, in order; `Err` says how
/// it differs.
fn compare(name: &str, ours: &[(u32, u32)], sums: &[&str]) -> Result<(), String> {
    let mut theirs = Vec::new();
    for s in sums {
        let mut one = common::sums(s);
        one.sort_by_key(|s| s.picture);
        theirs.extend(one.into_iter().map(|s| (s.y, s.uv)));
    }
    let wrong = ours.iter().zip(&theirs).filter(|(o, t)| o != t).count();
    println!(
        "  {name}: {} of {} pictures, {wrong} not the reference's at their place",
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

fn framework() -> &'static mf::Mf {
    mf::load().expect("the system's media framework")
}

/// **Every clip of a shape the decoder takes comes out bit for bit and in
/// the stream's order**: H.264 at eight bits, HEVC at eight and ten, the
/// ten-bit ones as ten bits in sixteen.
#[test]
#[ignore = "requires the system's media framework and its HEVC extension"]
fn every_clip_decodes_to_the_reference_pictures_in_order() {
    let mf = framework();
    let (caps, h264, hevc) = caps(mf);
    println!("caps {caps:?}, H.264 {h264:?}, HEVC {hevc:?}");
    assert!(caps.h264 && caps.hevc && caps.hevc_10, "both decoders");
    let mut failed = Vec::new();
    for (clip, sums, codec, ten_bit) in clips() {
        let mut backend = Backend::new(mf, UNIT_BYTES);
        backend.build(&header(codec, ten_bit)).expect("build");
        let ours = decode_units(&mut backend, &clip, &common::units(&clip));
        backend.destroy();
        if let Err(e) = compare(&clip, &ours, &[&sums]) {
            failed.push(format!("{clip}: {e}"));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// **A change of size mid-stream is followed**: two clips of different
/// sizes one after the other, every picture of both at its place.
#[test]
#[ignore = "requires the system's media framework and its HEVC extension"]
fn a_change_of_size_mid_stream_is_followed() {
    let mf = framework();
    for (first, second, codec) in [
        ("h264-ipp-cabac", "h264-nvenc-ll", Codec::H264),
        ("hevc-ipp", "hevc-nvenc-ll", Codec::H265),
    ] {
        let path = |n: &str| format!("fixtures/{n}.bin");
        let mut units = common::units(&path(first));
        units.extend(common::units(&path(second)));
        let mut backend = Backend::new(mf, UNIT_BYTES);
        backend.build(&header(codec, false)).expect("build");
        let name = format!("{first} then {second}");
        let ours = decode_units(&mut backend, &name, &units);
        backend.destroy();
        let sums = |n: &str| format!("fixtures/{n}.sums");
        compare(&name, &ours, &[&sums(first), &sums(second)]).expect("in order");
    }
}

/// A sequence parameter set's bits, written by hand.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    bit: u32,
}

impl Bits {
    fn put(&mut self, value: u32, width: u32) {
        for i in (0..width).rev() {
            if self.bit % 8 == 0 {
                self.bytes.push(0);
            }
            if value >> i & 1 == 1 {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.bit % 8);
            }
            self.bit += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let coded = value + 1;
        let width = 32 - coded.leading_zeros();
        self.put(0, width - 1);
        self.put(coded, width);
    }

    /// The stop bit, and the unit with its start code and header.
    fn unit(mut self) -> Vec<u8> {
        self.put(1, 1);
        let mut unit = vec![0, 0, 0, 1, 0x67];
        unit.extend(self.bytes);
        unit
    }
}

/// An H.264 sequence parameter set of `profile`, its chroma format and
/// luma depth, cropped `left` units from the left, 128 square.
fn h264_sps(profile: u32, chroma: u32, depth_minus8: u32, left: u32) -> Vec<u8> {
    let mut b = Bits::default();
    b.put(profile, 8);
    b.put(0, 8);
    b.put(40, 8);
    b.ue(0);
    if profile >= 100 {
        b.ue(chroma);
        if chroma == 3 {
            b.put(0, 1);
        }
        b.ue(depth_minus8);
        b.ue(depth_minus8);
        b.put(0, 1);
        b.put(0, 1);
    }
    b.ue(0); // log2_max_frame_num_minus4
    b.ue(2); // pic_order_cnt_type
    b.ue(1); // max_num_ref_frames
    b.put(0, 1);
    b.ue(7); // 128 wide
    b.ue(7); // 128 tall
    b.put(1, 1); // frame_mbs_only
    b.put(1, 1); // direct_8x8_inference
    if left > 0 {
        b.put(1, 1);
        b.ue(left);
        b.ue(0);
        b.ue(0);
        b.ue(0);
    } else {
        b.put(0, 1);
    }
    b.put(0, 1); // no VUI
    b.unit()
}

/// Feed `unit` to a decoder built for `codec` on a thread of its own, and
/// the answer within [`REFUSAL_WAIT`]: a decoder handed a stream it hangs on
/// never answers, and the test fails rather than hangs.
fn fed_within(codec: Codec, units: Vec<Vec<u8>>) -> Result<Fed, Fault> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut backend = Backend::new(framework(), UNIT_BYTES);
        backend.build(&header(codec, false)).expect("build");
        let mut last = Ok(Fed::NeedMoreData);
        for unit in &units {
            last = backend.feed(unit);
            if last.is_err() {
                break;
            }
            let pitch = 1280 * 2 + 64;
            let (mut y, mut uv) = (vec![0u8; pitch * 720], vec![0u8; pitch * 360]);
            let mut planes = Planes {
                y: &mut y,
                y_pitch: pitch,
                uv: &mut uv,
                uv_pitch: pitch,
                v: &mut [],
                v_pitch: 0,
            };
            if let Err(fault) = backend.take(&mut planes) {
                last = Err(fault);
                break;
            }
        }
        let _ = tx.send(last);
    });
    rx.recv_timeout(REFUSAL_WAIT)
        .expect("an answer: the decoder was given a stream it hangs on")
}

/// **A stream the decoder must not be given is refused as fatal before it
/// sees it**: H.264 at ten bits, at full chroma, or cropped from the left --
/// the first two hang the decoder, the third it writes to the wrong place --
/// and HEVC at full chroma.
#[test]
#[ignore = "requires the system's media framework and its HEVC extension"]
fn a_stream_it_must_not_be_given_is_refused_at_once() {
    framework();
    for (name, sps) in [
        ("H.264 High 10", h264_sps(110, 1, 2, 0)),
        ("H.264 High 4:4:4", h264_sps(244, 3, 0, 0)),
        ("H.264 cropped from the left", h264_sps(100, 1, 0, 2)),
    ] {
        assert_eq!(
            fed_within(Codec::H264, vec![sps]),
            Err(Fault::Fatal),
            "{name}"
        );
    }
    // The control: the same set at eight-bit 4:2:0 is taken.
    assert_eq!(
        fed_within(Codec::H264, vec![h264_sps(100, 1, 0, 0)]),
        Ok(Fed::NeedMoreData)
    );
    for clip in ["fixtures/hevc-444.bin", "fixtures/hevc-444-main10.bin"] {
        assert_eq!(
            fed_within(Codec::H265, common::units(clip)),
            Err(Fault::Fatal),
            "{clip}"
        );
    }
}

/// **Every unit's picture comes out of its own call**, both codecs, ten-bit
/// too: after the n-th unit of a stream that never reorders, n pictures have
/// come out -- none held for the next unit, which the HEVC decoder does to a
/// unit that does not end with a delimiter.
#[test]
#[ignore = "requires the system's media framework and its HEVC extension"]
fn every_units_picture_comes_out_of_its_own_call() {
    let mf = framework();
    for (clip, codec, ten_bit) in [
        ("synthetic-720p-h264.bin", Codec::H264, false),
        ("synthetic-720p-hevc.bin", Codec::H265, false),
        ("synthetic-720p-hevc10.bin", Codec::H265, true),
        ("fixtures/hevc-nvenc-ll.bin", Codec::H265, false),
        ("fixtures/h264-nvenc-ll.bin", Codec::H264, false),
    ] {
        let mut backend = Backend::new(mf, UNIT_BYTES);
        backend.build(&header(codec, ten_bit)).expect("build");
        let pitch = 1280 * 2 + 64;
        let (mut y, mut uv) = (vec![0u8; pitch * 720], vec![0u8; pitch * 360]);
        let units = common::units(clip);
        let (mut out, mut late) = (0usize, 0usize);
        for (n, unit) in units.iter().enumerate() {
            backend.feed(unit).expect("feed");
            loop {
                let mut planes = Planes {
                    y: &mut y,
                    y_pitch: pitch,
                    uv: &mut uv,
                    uv_pitch: pitch,
                    v: &mut [],
                    v_pitch: 0,
                };
                if backend.take(&mut planes).expect("take").is_none() {
                    break;
                }
                out += 1;
            }
            if out != n + 1 {
                late += 1;
            }
        }
        backend.destroy();
        println!(
            "  {clip}: {late} of {} units without their picture",
            units.len()
        );
        assert_eq!(late, 0, "{clip}");
    }
}
