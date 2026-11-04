//! The quality setting moved under a running encoder, on each vendor's: every
//! move between the three settings, the rate changing between them, and every
//! access unit decoded by one of the library's own decoders -- D3D11's on
//! Windows, NVIDIA's on Linux -- against the picture it was made from. A
//! keyframe comes exactly where the move said it costs one, and nowhere else.
//! The frame rate moved the same way, and what a picture costs after each
//! move. Needs each vendor's GPU and driver, so off by default: `cargo test
//! -p lowlat-decode --test quality_moves -- --ignored --nocapture`.

// NVIDIA's encoder on both; AMD's and Intel's on a D3D11 device, on Windows.
#![cfg(any(windows, target_os = "linux"))]
#![allow(clippy::cast_possible_truncation)]

#[cfg(windows)]
use std::sync::Arc;
use std::time::{Duration, Instant};

use lowlat_capture::Frame;
use lowlat_capture::synthetic::Synthetic;
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::{Decoder, Planes};
#[cfg(windows)]
use lowlat_drivers::amf::Amf;
#[cfg(windows)]
use lowlat_drivers::d3d11::{Adapter, D3d11, Device};
#[cfg(windows)]
use lowlat_drivers::vpl::Vpl;
use lowlat_encode::{Encoder, Moved, Poll, Quality, nvenc};
#[cfg(windows)]
use lowlat_encode::{amf, vpl};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
/// Pictures between two moves: long enough for a reference chain the move
/// broke to carry the fault on.
const SEGMENT: usize = 30;
/// Every ordered pair of settings once, from the one a host starts at.
const WALK: [Quality; 7] = [
    Quality::LowestLatency,
    Quality::Highest,
    Quality::Balanced,
    Quality::Highest,
    Quality::LowestLatency,
    Quality::Balanced,
    Quality::LowestLatency,
];
/// Frame rates a host moves its encoder through, from the one it is built
/// at: a 120 Hz panel's own, a quarter of it, and past it both ways.
const RATES: [u32; 7] = [120, 30, 60, 144, 30, 240, 120];
/// Rows of unpredictable detail a spend runs over: dear enough at the
/// slowest of [`RATES`] to take its whole share, cheap enough at the fastest
/// to keep to its own.
const DETAIL_ROWS: u32 = 120;
/// The one rate a spend runs at.
const SPEND_BPS: u32 = 8_000_000;
#[cfg(windows)]
const NVIDIA: u32 = 0x10de;
#[cfg(windows)]
const INTEL: u32 = 0x8086;

fn header(codec: Codec) -> VideoHeader {
    VideoHeader {
        frame_id: 1,
        width: 0,
        height: 0,
        codec,
        rotation: Rotation::None,
        ten_bit: false,
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

/// Luma and chroma of a picture read back, against the frame it was made from.
fn compare(want: &Frame<'_>, y: &[u8], uv: &[u8], pitch: usize) -> (f64, f64) {
    let width = want.width as usize;
    let rows = want.height as usize;
    let mut got = Vec::with_capacity(width * rows);
    let mut wanted = Vec::with_capacity(width * rows);
    for row in 0..rows {
        got.extend_from_slice(&y[row * pitch..][..width]);
        wanted.extend_from_slice(&want.luma.bytes[row * want.luma.stride..][..width]);
    }
    let luma = psnr(&got, &wanted);
    got.clear();
    wanted.clear();
    for row in 0..rows.div_ceil(2) {
        got.extend_from_slice(&uv[row * pitch..][..width]);
        wanted.extend_from_slice(&want.chroma.bytes[row * want.chroma.stride..][..width]);
    }
    (luma, psnr(&got, &wanted))
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

fn codes_idr(types: &[u8], codec: Codec) -> bool {
    types.iter().any(|&kind| match codec {
        Codec::H264 => kind == 5,
        _ => kind == 19 || kind == 20,
    })
}

fn carries_sets(types: &[u8], codec: Codec) -> bool {
    types.iter().any(|&kind| match codec {
        Codec::H264 => kind == 7,
        _ => kind == 33,
    })
}

/// The parameter sets in an access unit, each whole with its start code.
fn parameter_sets(bytes: &[u8], codec: Codec) -> Vec<Vec<u8>> {
    let starts: Vec<usize> = bytes
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .map(|(at, _)| at)
        .collect();
    let mut sets = Vec::new();
    for (index, &at) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(bytes.len());
        let Some(&first) = bytes.get(at + 3) else {
            continue;
        };
        let set = match codec {
            Codec::H264 => matches!(first & 0x1f, 7 | 8),
            _ => matches!((first >> 1) & 0x3f, 32..=34),
        };
        if set {
            // A four-byte start code leaves its leading zero on the unit
            // before; trailing zeros are not part of a set either.
            let mut unit = bytes[at..end].to_vec();
            while unit.last() == Some(&0) {
                unit.pop();
            }
            sets.push(unit);
        }
    }
    sets
}

/// The next unit, waited for on the encoder.
fn collect<E: Encoder>(encoder: &mut E) -> (Vec<u8>, bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match encoder.poll().expect("poll") {
            Poll::Ready {
                bitstream,
                keyframe,
            } => return (bitstream.to_vec(), keyframe),
            Poll::Pending => {
                assert!(Instant::now() < deadline, "no unit within two seconds");
                if !encoder.wait_finished(Duration::from_millis(1)) {
                    std::hint::spin_loop();
                }
            }
        }
    }
}

/// Every unit of a walk, which pictures its moves said open with a keyframe,
/// and how many pictures went in.
type Walked = (Vec<(Vec<u8>, bool)>, Vec<usize>, usize);

/// [`walk`] through [`WALK`]'s qualities.
fn quality_walk<E: Encoder>(encoder: &mut E, depth: usize, name: &str) -> Walked {
    walk(encoder, depth, name, WALK.len(), |encoder, segment| {
        let to = WALK[segment];
        let moved = encoder.set_quality(to).expect("set_quality");
        println!("{name}: {:?} -> {to:?}", WALK[segment - 1]);
        moved
    })
}

/// [`walk`] through [`RATES`]' frame rates.
fn rate_walk<E: Encoder>(encoder: &mut E, depth: usize, name: &str) -> Walked {
    walk(encoder, depth, name, RATES.len(), |encoder, segment| {
        let to = RATES[segment];
        let moved = encoder.set_fps(to).expect("set_fps");
        println!("{name}: {} -> {to} fps", RATES[segment - 1]);
        moved
    })
}

/// Walk `encoder` through `segments` stretches, `depth` pictures in flight
/// where it holds them, moved by `move_to` at the start of each stretch after
/// the first and the rate moved in the middle of every one: every unit, and
/// which pictures the moves said open with a keyframe.
fn walk<E: Encoder>(
    encoder: &mut E,
    depth: usize,
    name: &str,
    segments: usize,
    mut move_to: impl FnMut(&mut E, usize) -> Moved,
) -> Walked {
    let mut source = Synthetic::new(WIDTH, HEIGHT);
    let total = segments * SEGMENT;
    let mut units = Vec::with_capacity(total);
    let mut refreshes = vec![0];
    let mut in_flight = 0usize;
    for index in 0..total {
        if index > 0 && index % SEGMENT == 0 {
            let started = Instant::now();
            let moved = move_to(encoder, index / SEGMENT);
            println!(
                "{name}: picture {index}: {moved:?} in {:.3} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
            match moved {
                Moved::Live => {}
                Moved::Refresh => refreshes.push(index),
                Moved::Rebuild => panic!("{name}: a backend that moves answered rebuild"),
            }
        }
        if index % SEGMENT == SEGMENT / 2 {
            let rate = if (index / SEGMENT) % 2 == 0 {
                12_000_000
            } else {
                20_000_000
            };
            encoder.reconfigure(rate).expect("reconfigure");
        }
        let frame = source.acquire();
        // **A refusal is back pressure**: what is in flight is collected and
        // the same picture goes in again, so every picture is the source's.
        while encoder.submit(&frame, index == 0).is_err() {
            assert!(
                in_flight > 0,
                "{name}: picture {index} refused with nothing in flight"
            );
            units.push(collect(encoder));
            in_flight -= 1;
        }
        in_flight += 1;
        while in_flight >= depth {
            units.push(collect(encoder));
            in_flight -= 1;
        }
    }
    while in_flight > 0 {
        units.push(collect(encoder));
        in_flight -= 1;
    }
    (units, refreshes, total)
}

/// What a picture costs in each stretch of [`RATES`] on the detail source at
/// [`SPEND_BPS`], as a share of what one picture at that stretch's rate has:
/// the mean of each stretch's second half, once the rate control has settled.
/// **One for an encoder that took the move**; one that kept its old rate
/// spends the old rate's share, a quarter at 30 after 120.
fn spends<E: Encoder>(encoder: &mut E, depth: usize, name: &str) -> Vec<f64> {
    let mut source = Synthetic::with_detail(WIDTH, HEIGHT, DETAIL_ROWS);
    let total = RATES.len() * SEGMENT;
    let mut sizes = Vec::with_capacity(total);
    let mut in_flight = 0usize;
    for index in 0..total {
        if index > 0 && index % SEGMENT == 0 {
            let moved = encoder.set_fps(RATES[index / SEGMENT]).expect("set_fps");
            assert_ne!(
                moved,
                Moved::Rebuild,
                "{name}: a backend that moves answered rebuild"
            );
        }
        let frame = source.acquire();
        while encoder.submit(&frame, index == 0).is_err() {
            assert!(
                in_flight > 0,
                "{name}: picture {index} refused with nothing in flight"
            );
            sizes.push(collect(encoder).0.len());
            in_flight -= 1;
        }
        in_flight += 1;
        while in_flight >= depth {
            sizes.push(collect(encoder).0.len());
            in_flight -= 1;
        }
    }
    while in_flight > 0 {
        sizes.push(collect(encoder).0.len());
        in_flight -= 1;
    }
    assert_eq!(sizes.len(), total, "{name}: units went missing");
    RATES
        .iter()
        .enumerate()
        .map(|(segment, &fps)| {
            let settled = &sizes[segment * SEGMENT + SEGMENT / 2..(segment + 1) * SEGMENT];
            let bits = settled.iter().sum::<usize>() as f64 * 8.0 / settled.len() as f64;
            let share = f64::from(SPEND_BPS) / f64::from(fps);
            println!(
                "{name}: {fps} fps: {bits:.0} bits a picture, {:.2} of its share",
                bits / share
            );
            bits / share
        })
        .collect()
}

/// Every picture decoded in order by `decoder`, `drain` its own end of
/// stream: how many came back and the worst luma and chroma among them.
fn decode_all<D: Decoder>(
    decoder: &mut D,
    drain: impl Fn(&mut D),
    codec: Codec,
    units: &[(Vec<u8>, bool)],
) -> (usize, f64, f64) {
    decoder.build(&header(codec)).expect("build");
    let mut reference = Synthetic::new(WIDTH, HEIGHT);
    // An odd pitch, so a pitch mistake shows.
    let pitch = WIDTH as usize + 64;
    let mut y = vec![0u8; pitch * HEIGHT as usize];
    let mut uv = vec![0u8; pitch * HEIGHT as usize];
    let mut v = vec![0u8; pitch * HEIGHT as usize];
    let (mut worst_luma, mut worst_chroma) = (f64::INFINITY, f64::INFINITY);
    let mut decoded = 0usize;
    for (index, (unit, _)) in units.iter().enumerate() {
        decoder.feed(unit).expect("feed");
        if index + 1 == units.len() {
            drain(decoder);
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
            let (luma, chroma) = compare(&reference.acquire(), &y, &uv, pitch);
            worst_luma = worst_luma.min(luma);
            worst_chroma = worst_chroma.min(chroma);
            decoded += 1;
        }
    }
    decoder.destroy();
    (decoded, worst_luma, worst_chroma)
}

/// What every walk must show: each picture back as it went in, a keyframe
/// with its sets exactly where a move said one, and none anywhere else.
fn check<D: Decoder>(
    name: &str,
    codec: Codec,
    decoder: &mut D,
    drain: impl Fn(&mut D),
    walked: Walked,
) {
    let (units, refreshes, total) = walked;
    assert_eq!(units.len(), total, "{name}: units went missing");
    for (index, (unit, flagged)) in units.iter().enumerate() {
        let types = unit_types(unit, codec);
        let keyframe = *flagged || codes_idr(&types, codec);
        let expected = refreshes.contains(&index);
        assert_eq!(
            keyframe, expected,
            "{name}: picture {index} keyframe={keyframe}, the moves said {expected} ({types:?})"
        );
        if expected {
            assert!(
                carries_sets(&types, codec),
                "{name}: the keyframe at {index} carries no parameter sets ({types:?})"
            );
        }
    }
    let (decoded, luma, chroma) = decode_all(decoder, drain, codec, &units);
    println!(
        "{name}: {decoded}/{total} decoded, worst luma {luma:.1} dB, worst chroma {chroma:.1} dB, \
         keyframes at {refreshes:?}"
    );
    assert_eq!(decoded, total, "{name}: pictures went missing on the way");
    // **Far below what a right picture scores and far above a wrong one**: a
    // move that changed the sets without resending them decoded at 17 dB.
    assert!(
        luma > 40.0,
        "{name}: luma at {luma:.1} dB is not the source"
    );
    assert!(
        chroma > 40.0,
        "{name}: chroma at {chroma:.1} dB is not the source"
    );
}

/// [`check`], decoded by D3D11's decoder on a device of its own, as a
/// client's is.
#[cfg(windows)]
fn check_d3d11(name: &str, codec: Codec, decoding: &Device, walked: Walked) {
    let mut decoder = lowlat_decode::d3d11::Backend::new(decoding, (4096, 4096));
    check(name, codec, &mut decoder, |d| d.drain(), walked);
}

#[cfg(windows)]
fn adapter(d3d11: &D3d11, vendor: u32) -> Adapter {
    d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .find(|a| a.decodes_here() && a.vendor == vendor)
        .expect("an adapter of the encoder's vendor")
}

fn nvenc_config(codec: Codec, quality: Quality) -> nvenc::Config {
    nvenc::Config {
        codec: if codec == Codec::H264 {
            nvenc::Codec::H264
        } else {
            nvenc::Codec::H265
        },
        width: WIDTH,
        height: HEIGHT,
        fps: 60,
        bitrate_bps: 20_000_000,
        min_qp: quality.min_qp(),
        thorough: quality.thorough(),
        chroma: nvenc::Chroma::Yuv420,
        depth: nvenc::Depth::Eight,
    }
}

/// What a run of one encoder checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// [`WALK`]'s quality moves, every picture decoded.
    Qualities,
    /// [`RATES`]' frame-rate moves, every picture decoded.
    Rates,
    /// [`RATES`]' moves on the detail source: what a picture costs after
    /// each ([`spends`]).
    Spend,
}

/// The frame rate and the rate an encoder starts `run` at: a run of rates at
/// the first of [`RATES`], a spend at [`SPEND_BPS`] too.
fn start_of(run: Run) -> (u32, u32) {
    match run {
        Run::Qualities => (60, 20_000_000),
        Run::Rates => (RATES[0], 20_000_000),
        Run::Spend => (RATES[0], SPEND_BPS),
    }
}

/// The configuration NVIDIA's encoder starts `run` at.
fn nvenc_run_config(codec: Codec, run: Run) -> nvenc::Config {
    let (fps, bitrate_bps) = start_of(run);
    nvenc::Config {
        fps,
        bitrate_bps,
        ..nvenc_config(codec, WALK[0])
    }
}

/// `run` on an encoder whose pictures D3D11's decoder reads back.
#[cfg(windows)]
fn run_d3d11<E: Encoder>(
    encoder: &mut E,
    depth: usize,
    name: &str,
    codec: Codec,
    decoding: &Device,
    run: Run,
) {
    match run {
        Run::Qualities => check_d3d11(name, codec, decoding, quality_walk(encoder, depth, name)),
        Run::Rates => check_d3d11(name, codec, decoding, rate_walk(encoder, depth, name)),
        Run::Spend => held_to_share(name, &spends(encoder, depth, name)),
    }
}

/// What a spend must show: every stretch held to its own rate's share as the
/// first is, at the rate the encoder was built at -- **against that one, not
/// against the share itself**, since a rate control keeps under its target by
/// a margin of its own: 0.60 to 0.80 of it, measured on NVIDIA's.
fn held_to_share(name: &str, spent: &[f64]) {
    let built = spent[0];
    for (&fps, &share) in RATES.iter().zip(spent) {
        assert!(
            (0.75..1.33).contains(&(share / built)),
            "{name}: {fps} fps spends {share:.2} of its share, {built:.2} at the rate built at"
        );
    }
}

#[cfg(windows)]
fn nvenc_walk(codec: Codec, name: &str, run: Run) {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = adapter(&d3d11, NVIDIA);
    let api = nvenc::Api::load().expect("encoder runtime");
    let session = api
        .open_session(Arc::new(d3d11.open(adapter.luid).expect("a device")))
        .expect("session");
    let mut encoder = session
        .initialize(nvenc_run_config(codec, run))
        .expect("initialize");
    // A device of the decoder's own, as a client's is.
    let decoding = d3d11.open(adapter.luid).expect("a second device");
    run_d3d11(&mut encoder, 3, name, codec, &decoding, run);
}

/// On Linux the encoder takes the compute runtime's context, and NVIDIA's
/// decoder reads the stream back in the same one.
#[cfg(target_os = "linux")]
fn nvenc_walk(codec: Codec, name: &str, run: Run) {
    let cuda = lowlat_drivers::cuda::Cuda::load().expect("compute runtime");
    let device = cuda.any_device().expect("a device");
    let context = cuda.retain_primary(&device).expect("context");
    context.make_current().expect("current");
    let api = nvenc::Api::load().expect("encoder runtime");
    let session = api
        .open_session(
            cuda.retain_primary(&device)
                .expect("a second context handle"),
        )
        .expect("session");
    let mut encoder = session
        .initialize(&cuda, nvenc_run_config(codec, run))
        .expect("initialize");
    let walked = match run {
        Run::Qualities => quality_walk(&mut encoder, 3, name),
        Run::Rates => rate_walk(&mut encoder, 3, name),
        Run::Spend => return held_to_share(name, &spends(&mut encoder, 3, name)),
    };
    let cuvid = lowlat_drivers::cuvid::Cuvid::load().expect("decode interface");
    let mut decoder = lowlat_decode::nvdec::Backend::new(&cuda, &cuvid, (4096, 4096), 1 << 20);
    check(name, codec, &mut decoder, |d| d.drain(), walked);
}

/// The parameter sets the first picture of an encoder built at `quality`
/// carries.
#[cfg(windows)]
fn first_sets(codec: Codec, quality: Quality) -> Vec<Vec<u8>> {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = adapter(&d3d11, NVIDIA);
    let api = nvenc::Api::load().expect("encoder runtime");
    let session = api
        .open_session(Arc::new(d3d11.open(adapter.luid).expect("a device")))
        .expect("session");
    let mut encoder = session
        .initialize(nvenc_config(codec, quality))
        .expect("initialize");
    let mut source = Synthetic::new(WIDTH, HEIGHT);
    encoder.submit(&source.acquire(), true).expect("submit");
    parameter_sets(&collect(&mut encoder).0, codec)
}

#[cfg(target_os = "linux")]
fn first_sets(codec: Codec, quality: Quality) -> Vec<Vec<u8>> {
    let cuda = lowlat_drivers::cuda::Cuda::load().expect("compute runtime");
    let device = cuda.any_device().expect("a device");
    let api = nvenc::Api::load().expect("encoder runtime");
    let session = api
        .open_session(cuda.retain_primary(&device).expect("context"))
        .expect("session");
    let mut encoder = session
        .initialize(&cuda, nvenc_config(codec, quality))
        .expect("initialize");
    let mut source = Synthetic::new(WIDTH, HEIGHT);
    encoder.submit(&source.acquire(), true).expect("submit");
    parameter_sets(&collect(&mut encoder).0, codec)
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn nvidia_h264_moves_live() {
    nvenc_walk(Codec::H264, "nvenc h264", Run::Qualities);
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn nvidia_hevc_moves_live() {
    nvenc_walk(Codec::H265, "nvenc hevc", Run::Qualities);
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn nvidia_h264_moves_its_frame_rate_live() {
    nvenc_walk(Codec::H264, "nvenc h264", Run::Rates);
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn nvidia_hevc_moves_its_frame_rate_live() {
    nvenc_walk(Codec::H265, "nvenc hevc", Run::Rates);
}

/// **A picture after a move costs the share of the rate it was moved to**,
/// which is what the move is for: an encoder that kept the rate it was
/// built at spends that rate's share, a quarter of it at 30 after 120.
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn nvidia_spends_the_share_of_the_rate_it_was_moved_to() {
    nvenc_walk(Codec::H264, "nvenc h264", Run::Spend);
    nvenc_walk(Codec::H265, "nvenc hevc", Run::Spend);
}

/// **What makes a move live: the parameter sets do not move.** A change that
/// resends no set is decoded against the sets already sent, so a setting
/// whose own would differ breaks every picture after it.
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn nvidia_writes_one_set_of_parameters_at_every_setting() {
    for codec in [Codec::H264, Codec::H265] {
        let mut seen: Option<(Quality, Vec<Vec<u8>>)> = None;
        for quality in [Quality::LowestLatency, Quality::Balanced, Quality::Highest] {
            let sets = first_sets(codec, quality);
            assert!(!sets.is_empty(), "{codec:?} {quality:?}: no parameter sets");
            match &seen {
                None => seen = Some((quality, sets)),
                Some((first, theirs)) => assert_eq!(
                    &sets, theirs,
                    "{codec:?}: {quality:?} writes other parameter sets than {first:?}"
                ),
            }
        }
    }
}

#[cfg(windows)]
fn amf_walk(codec: Codec, name: &str, run: Run) {
    let (fps, bitrate_bps) = start_of(run);
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .find(|a| a.decodes_here() && a.maker() == Some("AMD"))
        .expect("an AMD GPU");
    let device = d3d11.open(adapter.luid).expect("a device");
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    let mut encoder = amf::Encoder::new(
        &runtime,
        &context,
        amf::Config {
            codec: if codec == Codec::H264 {
                amf::Codec::H264
            } else {
                amf::Codec::H265
            },
            width: WIDTH,
            height: HEIGHT,
            fps,
            bitrate_bps,
            min_qp: WALK[0].min_qp(),
            thorough: WALK[0].thorough(),
            depth: amf::Depth::Eight,
            source: amf::Source::Yuv,
        },
    )
    .expect("an encoder");
    let decoding = d3d11.open(adapter.luid).expect("a second device");
    run_d3d11(&mut encoder, 3, name, codec, &decoding, run);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn amd_h264_moves() {
    amf_walk(Codec::H264, "amf h264", Run::Qualities);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn amd_hevc_moves() {
    amf_walk(Codec::H265, "amf hevc", Run::Qualities);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn amd_h264_moves_its_frame_rate() {
    amf_walk(Codec::H264, "amf h264", Run::Rates);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn amd_hevc_moves_its_frame_rate() {
    amf_walk(Codec::H265, "amf hevc", Run::Rates);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn amd_spends_the_share_of_the_rate_it_was_moved_to() {
    amf_walk(Codec::H264, "amf h264", Run::Spend);
    amf_walk(Codec::H265, "amf hevc", Run::Spend);
}

#[cfg(windows)]
fn vpl_walk(codec: Codec, name: &str, run: Run) {
    let (fps, bitrate_bps) = start_of(run);
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = adapter(&d3d11, INTEL);
    let index = d3d11
        .plain_index(adapter.luid)
        .expect("the enumeration")
        .expect("the adapter");
    let runtime = Vpl::for_adapter(&adapter).expect("Intel's runtime");
    let device = d3d11.open(adapter.luid).expect("a device");
    let session = runtime
        .encoder_session(&device, index)
        .expect("an encoder's session");
    let mut encoder = vpl::Encoder::new(
        &session,
        &device,
        vpl::Config {
            codec: if codec == Codec::H264 {
                vpl::Codec::H264
            } else {
                vpl::Codec::H265
            },
            width: WIDTH,
            height: HEIGHT,
            fps,
            bitrate_bps,
            min_qp: WALK[0].min_qp(),
            thorough: WALK[0].thorough(),
            chroma: vpl::Chroma::Yuv420,
            depth: vpl::Depth::Eight,
        },
    )
    .expect("an encoder");
    let decoding = d3d11.open(adapter.luid).expect("a second device");
    run_d3d11(&mut encoder, 2, name, codec, &decoding, run);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn intel_h264_moves_live() {
    vpl_walk(Codec::H264, "vpl h264", Run::Qualities);
}

#[cfg(windows)]
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn intel_hevc_moves() {
    vpl_walk(Codec::H265, "vpl hevc", Run::Qualities);
}
