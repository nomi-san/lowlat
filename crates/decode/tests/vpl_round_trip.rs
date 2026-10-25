//! Intel's encoder on a D3D11 device, fed the synthetic source, and every
//! access unit it writes decoded by the library's own D3D11 decoder: each
//! picture must be the frame it was made from, in both codecs and at ten
//! bits; the library's own readers find no reordering, the first codec's
//! predicted slices plain and the second's bidirectional; a keyframe comes
//! when asked and only then, each with its parameter sets and no delimiter,
//! across a rate changed on every picture, which never waits on the picture
//! in the engine; the engine takes one picture at a time; the wait wakes on
//! the picture; nothing is allocated per picture on the loop's side; and what
//! the part codes is asked with nothing built, the answer the build's.
//! Needs Intel's GPU and runtime, so off by
//! default: `cargo test -p lowlat-decode --test vpl_round_trip -- --ignored
//! --nocapture --test-threads 1`.

// The encoder is bound to a D3D11 device on Windows alone.
#![cfg(windows)]
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::time::{Duration, Instant};

use lowlat_capture::Frame;
use lowlat_capture::synthetic::Synthetic;
use lowlat_common::alloc_counter::{Counting, assert_no_alloc};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::d3d11::Backend;
use lowlat_decode::{Decoder, Format, Planes, h264, hevc};
use lowlat_drivers::d3d11::{Adapter, D3d11};
use lowlat_drivers::vpl::Vpl;
use lowlat_encode::{Poll, vpl};

#[global_allocator]
static ALLOC: Counting = Counting;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
/// Long enough for drift to show: a reference chain that carried one wrong
/// picture carries it on.
const PICTURES: usize = 60;
/// Intel's vendor number, as its adapters report it.
const INTEL: u32 = 0x8086;

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

/// The ratio of the largest sample to the error between two runs of eight-bit
/// samples, in decibels; infinite where they are equal.
fn psnr(got: &[u8], want: &[u8]) -> f64 {
    let squared: u64 = got
        .iter()
        .zip(want)
        .map(|(&g, &w)| (i64::from(g) - i64::from(w)).unsigned_abs().pow(2))
        .sum();
    if squared == 0 {
        return f64::INFINITY;
    }
    let mean = squared as f64 / got.len().min(want.len()).max(1) as f64;
    10.0 * (255.0 * 255.0 / mean).log10()
}

/// A decoded sample: at ten bits the high byte of its word, since the value
/// sits high in the word and every source here is eight bits widened.
fn sample(plane: &[u8], pitch: usize, row: usize, column: usize, ten_bit: bool) -> u8 {
    if ten_bit {
        plane[row * pitch + column * 2 + 1]
    } else {
        plane[row * pitch + column]
    }
}

/// Luma and chroma of a picture read back, against the frame it was made from.
fn compare(want: &Frame<'_>, y: &[u8], uv: &[u8], pitch: usize, ten_bit: bool) -> (f64, f64) {
    let width = want.width as usize;
    let rows = want.height as usize;
    let mut got = Vec::with_capacity(width * rows);
    let mut wanted = Vec::with_capacity(width * rows);
    for row in 0..rows {
        for column in 0..width {
            got.push(sample(y, pitch, row, column, ten_bit));
            wanted.push(want.luma.bytes[row * want.luma.stride + column]);
        }
    }
    let luma = psnr(&got, &wanted);
    got.clear();
    wanted.clear();
    for row in 0..rows.div_ceil(2) {
        for column in 0..width {
            got.push(sample(uv, pitch, row, column, ten_bit));
            wanted.push(want.chroma.bytes[row * want.chroma.stride + column]);
        }
    }
    (luma, psnr(&got, &wanted))
}

fn config(codec: Codec, ten_bit: bool, thorough: bool) -> vpl::Config {
    vpl::Config {
        codec: if codec == Codec::H264 {
            vpl::Codec::H264
        } else {
            vpl::Codec::H265
        },
        width: WIDTH,
        height: HEIGHT,
        fps: 60,
        bitrate_bps: 20_000_000,
        min_qp: lowlat_encode::DEFAULT_MIN_QP,
        thorough,
        chroma: vpl::Chroma::Yuv420,
        depth: if ten_bit {
            vpl::Depth::Ten
        } else {
            vpl::Depth::Eight
        },
    }
}

/// The Intel GPU's adapter.
fn intel(d3d11: &D3d11) -> Adapter {
    d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .find(|a| a.decodes_here() && a.vendor == INTEL)
        .expect("an Intel GPU")
}

/// `body` run with an encoder built for `config` on a device of the Intel
/// GPU, and a device of the decoder's own there, as a client's is.
fn with_encoder<R>(
    config: vpl::Config,
    body: impl FnOnce(&mut vpl::Encoder<'_>, &lowlat_drivers::d3d11::Device) -> R,
) -> R {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = intel(&d3d11);
    let index = d3d11
        .plain_index(adapter.luid)
        .expect("the enumeration")
        .expect("the adapter");
    let runtime = Vpl::for_adapter(&adapter).expect("Intel's runtime");
    let device = d3d11.open(adapter.luid).expect("a device");
    let session = runtime
        .encoder_session(&device, index)
        .expect("an encoder's session");
    let mut encoder = vpl::Encoder::new(&session, &device, config).expect("an encoder");
    let decoding = d3d11.open(adapter.luid).expect("a second device");
    body(&mut encoder, &decoding)
}

/// The next unit, waited for on the encoder: its bytes and whether it is a
/// keyframe.
fn next_unit(encoder: &mut vpl::Encoder<'_>) -> (Vec<u8>, bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match encoder.poll().expect("poll") {
            Poll::Ready {
                bitstream,
                keyframe,
            } => return (bitstream.to_vec(), keyframe),
            Poll::Pending => {
                assert!(Instant::now() < deadline, "no unit within two seconds");
                encoder.wait_finished(Duration::from_millis(50));
            }
        }
    }
}

/// The types of the units in an access unit, in order.
fn unit_types(bytes: &[u8], codec: Codec) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = bytes[at..].windows(3).position(|w| w == [0, 0, 1]) {
        let header = at + found + 3;
        let Some(&first) = bytes.get(header) else {
            break;
        };
        out.push(if codec == Codec::H264 {
            first & 0x1f
        } else {
            (first >> 1) & 0x3f
        });
        at = header;
    }
    out
}

/// Whether a unit codes an IDR picture.
fn codes_idr(types: &[u8], codec: Codec) -> bool {
    types.iter().any(|&kind| match codec {
        Codec::H264 => kind == 5,
        _ => kind == 19 || kind == 20,
    })
}

/// Decode `units` in order and answer, for each picture, its luma and chroma
/// against the generator's frames from the first, with how many came back.
fn decode_all(
    decoding: &lowlat_drivers::d3d11::Device,
    codec: Codec,
    ten_bit: bool,
    units: &[Vec<u8>],
) -> (usize, f64, f64) {
    let mut decoder = Backend::new(decoding, (4096, 4096));
    decoder.build(&header(codec, ten_bit)).expect("build");
    let mut reference = Synthetic::new(WIDTH, HEIGHT);
    // An odd pitch, so a pitch mistake shows.
    let pitch = WIDTH as usize * 2 + 64;
    let mut y = vec![0u8; pitch * HEIGHT as usize];
    let mut uv = vec![0u8; pitch * HEIGHT as usize];
    let mut v = vec![0u8; pitch * HEIGHT as usize];
    let (mut worst_luma, mut worst_chroma) = (f64::INFINITY, f64::INFINITY);
    let mut decoded = 0usize;
    for (index, unit) in units.iter().enumerate() {
        decoder.feed(unit).expect("feed");
        if index + 1 == units.len() {
            decoder.drain();
        }
        loop {
            let mut planes = Planes {
                y: &mut y,
                y_pitch: pitch,
                uv: &mut uv,
                uv_pitch: pitch,
                v: &mut v,
                v_pitch: pitch,
            };
            let Some(picture) = decoder.take(&mut planes).expect("take") else {
                break;
            };
            assert_eq!((picture.width, picture.height), (WIDTH, HEIGHT));
            assert_eq!(
                picture.format,
                if ten_bit { Format::P010 } else { Format::Nv12 }
            );
            let (luma, chroma) = compare(&reference.acquire(), &y, &uv, pitch, ten_bit);
            worst_luma = worst_luma.min(luma);
            worst_chroma = worst_chroma.min(chroma);
            decoded += 1;
        }
    }
    decoder.destroy();
    (decoded, worst_luma, worst_chroma)
}

/// Encode the generator's pictures one at a time and decode them back: the
/// worst luma and chroma either saw.
fn round_trip(codec: Codec, ten_bit: bool, thorough: bool) -> (f64, f64) {
    with_encoder(config(codec, ten_bit, thorough), |encoder, decoding| {
        let mut source = Synthetic::new(WIDTH, HEIGHT);
        let mut units = Vec::with_capacity(PICTURES);
        for index in 0..PICTURES {
            encoder
                .submit(&source.acquire(), index == 0)
                .expect("submit");
            units.push(next_unit(encoder).0);
        }
        let (decoded, luma, chroma) = decode_all(decoding, codec, ten_bit, &units);
        assert_eq!(decoded, PICTURES, "pictures went missing on the way");
        (luma, chroma)
    })
}

fn check(name: &str, codec: Codec, ten_bit: bool, thorough: bool) {
    let (luma, chroma) = round_trip(codec, ten_bit, thorough);
    println!("{name}: {PICTURES} pictures, worst luma {luma:.1} dB, worst chroma {chroma:.1} dB");
    // **Far below what a right picture scores and far above a wrong one**: a
    // picture compared with its neighbour, or a lost colour plane, is well
    // under 30.
    assert!(
        luma > 40.0,
        "{name}: luma at {luma:.1} dB is not the source"
    );
    assert!(
        chroma > 40.0,
        "{name}: chroma at {chroma:.1} dB is not the source"
    );
}

#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn h264_comes_back_as_it_went_in() {
    check("h264", Codec::H264, false, false);
}

#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn hevc_comes_back_as_it_went_in() {
    check("hevc", Codec::H265, false, false);
}

#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn ten_bit_hevc_comes_back_as_it_went_in() {
    check("hevc10", Codec::H265, true, false);
}

/// The highest quality setting's encoder: the most thorough effort, built
/// and carrying the picture as the default's.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn a_thorough_encoder_comes_back_as_it_went_in() {
    check("hevc thorough", Codec::H265, false, true);
}

/// The streams this backend writes, for an external parser to say what their
/// sequence sets declare. Written beside each other in `LOWLAT_DUMP`, the
/// temporary directory otherwise:
///
/// ```text
/// ffmpeg -hide_banner -loglevel trace -i vpl.h264 -c copy -bsf:v trace_headers -f null -
/// ```
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn it_encodes_for_an_external_parser() {
    let dir = std::env::var_os("LOWLAT_DUMP").map_or_else(std::env::temp_dir, Into::into);
    for (codec, name) in [(Codec::H264, "vpl.h264"), (Codec::H265, "vpl.h265")] {
        let stream = with_encoder(config(codec, false, false), |encoder, _| {
            let mut source = Synthetic::new(WIDTH, HEIGHT);
            let mut stream = Vec::new();
            for index in 0..8 {
                encoder
                    .submit(&source.acquire(), index == 0)
                    .expect("submit");
                stream.extend_from_slice(&next_unit(encoder).0);
            }
            stream
        });
        let path = dir.join(name);
        std::fs::write(&path, &stream).expect("write");
        println!("wrote {}", path.display());
    }
}

/// **The library's own readers over the streams**: the sequence sets declare
/// no reordering, and every slice of a predicted picture is plain predicted
/// in the first codec and bidirectional in the second -- the runtime's
/// low-delay kind, which every Intel part codes, where some hang their engine
/// on plain predicted ones -- and every keyframe's intra.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn our_readers_find_no_reordering_and_the_slices_each_codec_should_have() {
    for codec in [Codec::H264, Codec::H265] {
        with_encoder(config(codec, false, false), |encoder, _| {
            let mut source = Synthetic::new(WIDTH, HEIGHT);
            let mut first = h264::Stream::new();
            let mut second = hevc::Stream::new();
            for index in 0..PICTURES {
                encoder
                    .submit(&source.acquire(), index == 0)
                    .expect("submit");
                let (unit, keyframe) = next_unit(encoder);
                let (slices, reorder) = if codec == Codec::H264 {
                    assert_eq!(first.read(&unit).expect("read"), h264::Read::Picture);
                    let job = first.job().expect("a picture");
                    let slices: Vec<char> = job
                        .slices
                        .as_slice()
                        .iter()
                        .flatten()
                        .map(|slice| match slice.header.slice_type {
                            h264::slice::SliceType::I => 'I',
                            h264::slice::SliceType::P => 'P',
                            h264::slice::SliceType::B => 'B',
                            _ => '?',
                        })
                        .collect();
                    let reorder = job.sps.reorder_frames();
                    first.finish().expect("finish");
                    while let Some(output) = first.next_output() {
                        first.dpb.taken(output.slot);
                    }
                    (slices, reorder)
                } else {
                    assert_eq!(second.read(&unit).expect("read"), hevc::Read::Picture);
                    let job = second.job().expect("a picture");
                    let slices: Vec<char> = job
                        .slices
                        .as_slice()
                        .iter()
                        .flatten()
                        .map(|slice| match slice.header.slice_type {
                            hevc::slice::SliceType::I => 'I',
                            hevc::slice::SliceType::P => 'P',
                            hevc::slice::SliceType::B => 'B',
                        })
                        .collect();
                    let reorder = Some(u32::from(job.sps.max_num_reorder_pics));
                    second.finish().expect("finish");
                    while let Some(output) = second.next_output() {
                        second.dpb.taken(output.slot);
                    }
                    (slices, reorder)
                };
                assert_eq!(reorder, Some(0), "{codec:?}: the reorder depth");
                let want = match (keyframe, codec) {
                    (true, _) => 'I',
                    (false, Codec::H264) => 'P',
                    (false, _) => 'B',
                };
                assert!(
                    !slices.is_empty() && slices.iter().all(|&kind| kind == want),
                    "{codec:?} picture {index}: slices {slices:?}, {want} wanted"
                );
            }
        });
    }
}

/// Keyframes forced at 0 and 30 and none asked for after, over 400
/// pictures: those two and no others, each led by its parameter sets, and no
/// delimiter anywhere.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn a_forced_keyframe_carries_its_sets_and_none_comes_unasked() {
    for codec in [Codec::H264, Codec::H265] {
        with_encoder(config(codec, false, false), |encoder, _| {
            let mut source = Synthetic::new(WIDTH, HEIGHT);
            let (set, delimiter) = match codec {
                Codec::H264 => (7, 9),
                _ => (32, 35),
            };
            for index in 0..400 {
                let forced = index == 0 || index == 30;
                encoder.submit(&source.acquire(), forced).expect("submit");
                let (unit, keyframe) = next_unit(encoder);
                let types = unit_types(&unit, codec);
                assert_eq!(keyframe, forced, "{codec:?} picture {index}: {types:?}");
                assert_eq!(
                    codes_idr(&types, codec),
                    forced,
                    "{codec:?} picture {index}: {types:?}"
                );
                assert_eq!(
                    types.first() == Some(&set),
                    forced,
                    "{codec:?} picture {index}: {types:?}"
                );
                assert!(
                    !types.contains(&delimiter),
                    "{codec:?} picture {index}: a delimiter in {types:?}"
                );
            }
        });
    }
}

/// The rate changed before every picture across 2 to 40 Mbit/s, the encoder
/// built at the top as at the session's ceiling: one keyframe in the stream,
/// the first, reported and coded alike, and the stream decodes as it went in.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn no_keyframe_crosses_a_rate_change_on_every_picture() {
    for codec in [Codec::H264, Codec::H265] {
        let built = vpl::Config {
            bitrate_bps: 40_000_000,
            ..config(codec, false, false)
        };
        with_encoder(built, |encoder, decoding| {
            let mut source = Synthetic::new(WIDTH, HEIGHT);
            let (mut reported, mut coded) = (0, 0);
            let mut units = Vec::new();
            for index in 0..120u32 {
                if index > 0 {
                    encoder
                        .reconfigure(2_000_000 + (index % 20) * 2_000_000)
                        .expect("reconfigure");
                }
                encoder
                    .submit(&source.acquire(), index == 0)
                    .expect("submit");
                let (unit, keyframe) = next_unit(encoder);
                reported += usize::from(keyframe);
                coded += usize::from(codes_idr(&unit_types(&unit, codec), codec));
                units.push(unit);
            }
            println!("{codec:?}: {reported} keyframes reported, {coded} coded");
            assert_eq!(
                (reported, coded),
                (1, 1),
                "{codec:?}: a keyframe crossed a rate change"
            );
            let (decoded, luma, _) = decode_all(decoding, codec, false, &units);
            assert_eq!(decoded, units.len(), "{codec:?}: pictures went missing");
            assert!(luma > 30.0, "{codec:?}: luma at {luma:.1} dB");
        });
    }
}

/// **A rate past what the runtime's sixteen-bit fields carry in kilobits**,
/// 100 Mbit/s, built through their shared multiplier, and changed down to 10
/// and back: one keyframe, and the pictures come back.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn a_rate_past_sixteen_bits_of_kilobits_builds_and_changes() {
    for codec in [Codec::H264, Codec::H265] {
        let built = vpl::Config {
            bitrate_bps: 100_000_000,
            ..config(codec, false, false)
        };
        with_encoder(built, |encoder, decoding| {
            let mut source = Synthetic::new(WIDTH, HEIGHT);
            let mut units = Vec::new();
            let mut keyframes = 0;
            for index in 0..90u32 {
                match index {
                    30 => encoder.reconfigure(10_000_000).expect("down"),
                    60 => encoder.reconfigure(100_000_000).expect("up"),
                    _ => {}
                }
                encoder
                    .submit(&source.acquire(), index == 0)
                    .expect("submit");
                let (unit, keyframe) = next_unit(encoder);
                keyframes += usize::from(keyframe);
                units.push(unit);
            }
            assert_eq!(keyframes, 1, "{codec:?}: a keyframe crossed a change");
            let (decoded, luma, chroma) = decode_all(decoding, codec, false, &units);
            println!("{codec:?} at 100 Mbit/s: worst luma {luma:.1} dB, chroma {chroma:.1} dB");
            assert_eq!(decoded, units.len(), "{codec:?}: pictures went missing");
            assert!(luma > 40.0 && chroma > 40.0, "{codec:?}: not the source");
        });
    }
}

/// **The engine takes one picture at a time**: a second handed over while
/// the first is in it is refused as back pressure, the generator's input
/// left as it was, and taken once the first is collected -- each picture the
/// frame it was made from.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn a_second_picture_waits_for_the_first() {
    for codec in [Codec::H264, Codec::H265] {
        with_encoder(config(codec, false, false), |encoder, decoding| {
            let mut source = Synthetic::new(WIDTH, HEIGHT);
            let mut units = Vec::new();
            let mut frame = source.acquire();
            for index in 0..30 {
                encoder.submit(&frame, index == 0).expect("submit");
                assert_eq!(encoder.in_flight(), 1);
                frame = source.acquire();
                let second = encoder.submit(&frame, false);
                assert!(
                    matches!(second, Err(vpl::Error::QueueFull)),
                    "{codec:?} picture {index}: a second picture: {second:?}"
                );
                units.push(next_unit(encoder).0);
                assert_eq!(encoder.in_flight(), 0);
            }
            let (decoded, luma, chroma) = decode_all(decoding, codec, false, &units);
            println!(
                "{codec:?}: {decoded} pictures, worst luma {luma:.1} dB, chroma {chroma:.1} dB"
            );
            assert_eq!(decoded, units.len(), "{codec:?}: pictures went missing");
            assert!(luma > 40.0 && chroma > 40.0, "{codec:?}: not the source");
        });
    }
}

/// One picture in the encoder at a time, 2 ms apart: the wait returns as it
/// finishes, far inside its limit, and the collect finds it at once. With
/// nothing in flight the wait answers at once that there was nothing to wait
/// for.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn the_wait_wakes_on_the_picture() {
    with_encoder(config(Codec::H265, false, false), |encoder, _| {
        assert!(!encoder.wait_finished(Duration::from_millis(100)));
        let mut source = Synthetic::new(WIDTH, HEIGHT);
        let mut waits = Vec::new();
        for index in 0..30 {
            encoder
                .submit(&source.acquire(), index == 0)
                .expect("submit");
            let began = Instant::now();
            while !matches!(encoder.poll().expect("poll"), Poll::Ready { .. }) {
                assert!(encoder.wait_finished(Duration::from_millis(500)));
                assert!(
                    began.elapsed() < Duration::from_millis(200),
                    "the wait ran on"
                );
            }
            waits.push(began.elapsed());
            std::thread::sleep(Duration::from_millis(2));
        }
        waits.sort();
        println!(
            "submit to collected: p50 {:?}, max {:?}",
            waits[waits.len() / 2],
            waits[waits.len() - 1]
        );
        assert!(
            waits[waits.len() / 2] < Duration::from_millis(10),
            "the wait did not wake on the picture"
        );
    });
}

/// **A change of rate never waits on the picture in the engine**, which the
/// runtime's own change does: asked with a picture in flight it is kept, the
/// loop held for no encode, and made before the next picture goes in; asked
/// with nothing in flight it is made at once. Each cost printed, the stream
/// checked for keyframes.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn a_change_of_rate_waits_for_no_picture() {
    for codec in [Codec::H264, Codec::H265] {
        let built = vpl::Config {
            bitrate_bps: 40_000_000,
            ..config(codec, false, false)
        };
        with_encoder(built, |encoder, _| {
            let mut source = Synthetic::new(WIDTH, HEIGHT);
            let (mut idle, mut busy) = (Vec::new(), Vec::new());
            let mut keyframes = 0;
            for index in 0..120u32 {
                let rate = 10_000_000 + (index % 2) * 10_000_000;
                if index % 2 == 0 && index > 0 {
                    let began = Instant::now();
                    encoder.reconfigure(rate).expect("reconfigure");
                    idle.push(began.elapsed());
                    assert_eq!(encoder.config().bitrate_bps, rate, "made at once");
                }
                encoder
                    .submit(&source.acquire(), index == 0)
                    .expect("submit");
                if index % 2 == 1 {
                    let before = encoder.config().bitrate_bps;
                    let began = Instant::now();
                    encoder.reconfigure(rate).expect("reconfigure");
                    busy.push(began.elapsed());
                    assert_eq!(encoder.config().bitrate_bps, before, "kept");
                }
                keyframes += usize::from(next_unit(encoder).1);
            }
            idle.sort();
            busy.sort();
            println!(
                "{codec:?}: a change with nothing in flight p50 {:?} max {:?}; with a picture \
                 in flight p50 {:?} max {:?}",
                idle[idle.len() / 2],
                idle[idle.len() - 1],
                busy[busy.len() / 2],
                busy[busy.len() - 1]
            );
            assert!(
                busy[busy.len() - 1] < Duration::from_millis(1),
                "{codec:?}: a change held the loop for the picture in flight"
            );
            assert_eq!(keyframes, 1, "{codec:?}: a keyframe crossed a change");
        });
    }
}

/// Nothing allocated per picture on the loop's side, once warm: the frame
/// written into an input, the submit, the wait, the collect and a rate
/// change.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn a_picture_allocates_nothing_on_the_loops_side() {
    for (codec, ten_bit) in [(Codec::H264, false), (Codec::H265, true)] {
        with_encoder(config(codec, ten_bit, false), |encoder, _| {
            let mut source = Synthetic::new(WIDTH, HEIGHT);
            for index in 0..60u32 {
                let frame = source.acquire();
                let warm = index >= 8;
                let mut picture = || {
                    encoder
                        .reconfigure(10_000_000 + index * 100_000)
                        .expect("reconfigure");
                    encoder.submit(&frame, index == 0).expect("submit");
                    loop {
                        if matches!(encoder.poll().expect("poll"), Poll::Ready { .. }) {
                            break;
                        }
                        encoder.wait_finished(Duration::from_millis(50));
                    }
                };
                if warm {
                    assert_no_alloc(&mut picture);
                } else {
                    picture();
                }
            }
        });
    }
}

/// **What the part codes is asked with nothing built, and the answer is the
/// build's**: the census's question -- HEVC's full chroma from the packed
/// word at eight and ten bits -- and the first codec at an ordinary picture,
/// past the width and the rate of pictures it builds at on some parts, and at
/// full chroma, which it never codes here; each answered by the census and
/// by a build on the same session, which must agree.
#[test]
#[ignore = "requires Intel's GPU and runtime"]
fn what_the_part_codes_is_asked_with_nothing_built() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = intel(&d3d11);
    let index = d3d11
        .plain_index(adapter.luid)
        .expect("the enumeration")
        .expect("the adapter");
    let runtime = Vpl::for_adapter(&adapter).expect("Intel's runtime");
    let device = d3d11.open(adapter.luid).expect("a device");
    let session = runtime
        .encoder_session(&device, index)
        .expect("an encoder's session");
    let full = |depth| vpl::Config {
        chroma: vpl::Chroma::Yuv444,
        depth,
        ..config(Codec::H265, false, false)
    };
    let first = config(Codec::H264, false, false);
    let asked = [
        ("full chroma at eight bits", full(vpl::Depth::Eight)),
        ("full chroma at ten bits", full(vpl::Depth::Ten)),
        ("the first codec", first),
        (
            "the first codec 5120 wide",
            vpl::Config {
                width: 5120,
                height: 1440,
                ..first
            },
        ),
        (
            "the first codec at 200 a second",
            vpl::Config { fps: 200, ..first },
        ),
        (
            "the first codec at full chroma",
            vpl::Config {
                chroma: vpl::Chroma::Yuv444,
                ..first
            },
        ),
    ];
    for (name, config) in asked {
        let coded = vpl::codes(&session, &config);
        let built = vpl::Encoder::new(&session, &device, config).is_ok();
        println!(
            "{}: {name}: coded {coded}, built {built}",
            adapter.description
        );
        assert_eq!(coded, built, "{name}: the census and the build disagree");
    }
    assert!(
        vpl::codes(&session, &first),
        "the first codec at an ordinary picture refused"
    );
}
