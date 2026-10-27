//! AMD's encoder on a D3D11 device, fed the synthetic source and an RGB picture
//! as a capture hands one over, and every access unit it writes decoded by the
//! library's own D3D11 decoder: each picture must be the frame it was made
//! from, in both codecs and at ten bits; a keyframe comes when asked and only
//! then, each with its parameter sets and no delimiter, across a rate changed
//! on every picture; the wait wakes on the picture; and nothing is allocated
//! per picture on the loop's side. Needs AMD's GPU and runtime, so off by
//! default: `cargo test -p lowlat-decode --test amf_round_trip -- --ignored
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
use lowlat_decode::{Decoder, Format, Planes};
use lowlat_drivers::amf::Amf;
use lowlat_drivers::d3d11::{Com, D3d11, Device};
use lowlat_drivers::ffi::d3d11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC, ID3D11Resource,
    ID3D11Texture2D,
};
use lowlat_drivers::vcall;
use lowlat_encode::{Poll, amf};

#[global_allocator]
static ALLOC: Counting = Counting;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
/// Long enough for drift to show: a reference chain that carried one wrong
/// picture carries it on.
const PICTURES: usize = 60;

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

/// A device on the AMD GPU.
fn amd(d3d11: &D3d11) -> Device {
    let adapter = d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .find(|a| a.decodes_here() && a.maker() == Some("AMD"))
        .expect("an AMD GPU");
    d3d11.open(adapter.luid).expect("a device")
}

/// Whether AMD's HEVC encoder here codes ten bits, asked of its caps: an
/// engine without the ten-bit profile refuses it only when it is set.
fn ten_bit_hevc() -> bool {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    let engine = lowlat_drivers::amf::Codec::Hevc;
    let names = engine.names();
    let encoder = runtime
        .encoder(&context, engine)
        .expect("AMD's HEVC encoder");
    let most = encoder
        .caps()
        .and_then(|caps| caps.get_int(names.cap_max_profile))
        .expect("its highest profile");
    names.profile_10.is_some_and(|ten| most >= ten)
}

fn config(codec: Codec, ten_bit: bool, thorough: bool, source: amf::Source) -> amf::Config {
    amf::Config {
        codec: if codec == Codec::H264 {
            amf::Codec::H264
        } else {
            amf::Codec::H265
        },
        width: WIDTH,
        height: HEIGHT,
        fps: 60,
        bitrate_bps: 20_000_000,
        min_qp: lowlat_encode::DEFAULT_MIN_QP,
        thorough,
        depth: if ten_bit {
            amf::Depth::Ten
        } else {
            amf::Depth::Eight
        },
        source,
    }
}

/// The next unit, waited for on the encoder: its bytes and whether it is a
/// keyframe.
fn next_unit(encoder: &mut amf::Encoder<'_>) -> (Vec<u8>, bool) {
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

/// Encode the generator's pictures, decode each access unit as it comes, and
/// answer the worst luma and chroma either saw.
fn round_trip(codec: Codec, ten_bit: bool, thorough: bool) -> (f64, f64) {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    let mut encoder = amf::Encoder::new(
        &runtime,
        &context,
        config(codec, ten_bit, thorough, amf::Source::Yuv),
    )
    .expect("an encoder");
    // A device of the decoder's own, as a client's is.
    let decoding = amd(&d3d11);
    let mut decoder = Backend::new(&decoding, (4096, 4096));
    decoder.build(&header(codec, ten_bit)).expect("build");

    let mut source = Synthetic::new(WIDTH, HEIGHT);
    let mut reference = Synthetic::new(WIDTH, HEIGHT);
    // An odd pitch, so a pitch mistake shows.
    let pitch = WIDTH as usize * 2 + 64;
    let mut y = vec![0u8; pitch * HEIGHT as usize];
    let mut uv = vec![0u8; pitch * HEIGHT as usize];
    let mut v = vec![0u8; pitch * HEIGHT as usize];
    let (mut worst_luma, mut worst_chroma) = (f64::INFINITY, f64::INFINITY);
    let mut decoded = 0usize;
    for index in 0..PICTURES {
        encoder
            .submit(&source.acquire(), index == 0)
            .expect("submit");
        let (unit, _) = next_unit(&mut encoder);
        decoder.feed(&unit).expect("feed");
        if index + 1 == PICTURES {
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
    assert_eq!(decoded, PICTURES, "pictures went missing on the way");
    (worst_luma, worst_chroma)
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
#[ignore = "requires AMD's GPU and runtime"]
fn h264_comes_back_as_it_went_in() {
    check("h264", Codec::H264, false, false);
}

/// The streams this backend writes, for an external parser to say what their
/// sequence sets declare -- above all the reorder depth, which a hardware
/// decoder that is not told holds pictures back for. Written beside each
/// other in `LOWLAT_DUMP`, the temporary directory otherwise:
///
/// ```text
/// ffmpeg -hide_banner -loglevel trace -i amf.h264 -c copy -bsf:v trace_headers -f null -
/// ```
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn it_encodes_for_an_external_parser() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    let dir = std::env::var_os("LOWLAT_DUMP").map_or_else(std::env::temp_dir, Into::into);
    for (codec, name) in [(Codec::H264, "amf.h264"), (Codec::H265, "amf.h265")] {
        let mut encoder = amf::Encoder::new(
            &runtime,
            &context,
            config(codec, false, false, amf::Source::Yuv),
        )
        .expect("an encoder");
        let mut source = Synthetic::new(WIDTH, HEIGHT);
        let mut stream = Vec::new();
        for index in 0..8 {
            encoder
                .submit(&source.acquire(), index == 0)
                .expect("submit");
            stream.extend_from_slice(&next_unit(&mut encoder).0);
        }
        let path = dir.join(name);
        std::fs::write(&path, &stream).expect("write");
        println!("wrote {}", path.display());
    }
}

#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn hevc_comes_back_as_it_went_in() {
    check("hevc", Codec::H265, false, false);
}

#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn ten_bit_hevc_comes_back_as_it_went_in() {
    if !ten_bit_hevc() {
        println!("hevc10: no ten-bit profile on this engine, skipped");
        return;
    }
    check("hevc10", Codec::H265, true, false);
}

/// The highest quality setting's encoder: the most thorough preset and its
/// pass ahead of the encode, built and carrying the picture as the default's.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn a_thorough_encoder_comes_back_as_it_went_in() {
    check("hevc thorough", Codec::H265, false, true);
}

/// The rate changed before every picture across 2 to 40 Mbit/s, the
/// collecting thread waiting in the runtime meanwhile: one keyframe in the
/// stream, the first, reported and coded alike.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn no_keyframe_crosses_a_rate_change_on_every_picture() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    for codec in [Codec::H264, Codec::H265] {
        let mut encoder = amf::Encoder::new(
            &runtime,
            &context,
            config(codec, false, false, amf::Source::Yuv),
        )
        .expect("an encoder");
        let mut source = Synthetic::new(WIDTH, HEIGHT);
        let (mut reported, mut coded) = (0, 0);
        for index in 0..120u32 {
            if index > 0 {
                encoder
                    .reconfigure(2_000_000 + (index % 20) * 2_000_000)
                    .expect("reconfigure");
            }
            encoder
                .submit(&source.acquire(), index == 0)
                .expect("submit");
            let (unit, keyframe) = next_unit(&mut encoder);
            reported += usize::from(keyframe);
            coded += unit_types(&unit, codec)
                .iter()
                .filter(|&&kind| match codec {
                    Codec::H264 => kind == 5,
                    _ => kind == 19 || kind == 20,
                })
                .count();
        }
        println!("{codec:?}: {reported} keyframes reported, {coded} coded");
        assert_eq!(
            (reported, coded),
            (1, 1),
            "{codec:?}: a keyframe crossed a rate change"
        );
    }
}

/// Keyframes forced at 0 and 30, then none asked for past 300, where the
/// usage's own schedule puts one: those two and no others, each led by its
/// parameter sets, and no delimiter anywhere.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn a_forced_keyframe_carries_its_sets_and_none_comes_unasked() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    for codec in [Codec::H264, Codec::H265] {
        let mut encoder = amf::Encoder::new(
            &runtime,
            &context,
            config(codec, false, false, amf::Source::Yuv),
        )
        .expect("an encoder");
        let mut source = Synthetic::new(WIDTH, HEIGHT);
        let (set, delimiter) = match codec {
            Codec::H264 => (7, 9),
            _ => (32, 35),
        };
        for index in 0..400 {
            let forced = index == 0 || index == 30;
            encoder.submit(&source.acquire(), forced).expect("submit");
            let (unit, keyframe) = next_unit(&mut encoder);
            let types = unit_types(&unit, codec);
            assert_eq!(keyframe, forced, "{codec:?} picture {index}: {types:?}");
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
    }
}

/// One picture in the engine at a time, 2 ms apart: the wait returns as it
/// finishes, far inside its limit, and the collect finds it at once -- a
/// collecting thread the submit did not wake would sleep out most of its own
/// 20 ms first. With nothing in flight the wait answers at once that there
/// was nothing to wait for.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn the_wait_wakes_on_the_picture() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    let mut encoder = amf::Encoder::new(
        &runtime,
        &context,
        config(Codec::H265, false, false, amf::Source::Yuv),
    )
    .expect("an encoder");
    assert!(!encoder.wait_finished(Duration::from_millis(100)));
    let engine = encoder.engine();
    assert!(
        !engine.busy(),
        "the engine busy before anything was handed over"
    );
    let mut source = Synthetic::new(WIDTH, HEIGHT);
    let mut waits = Vec::new();
    for index in 0..30 {
        encoder
            .submit(&source.acquire(), index == 0)
            .expect("submit");
        // **Busy from the submit**, before the engine could have finished:
        // what a reader in place waits on before letting the texture go.
        assert!(engine.busy(), "the engine idle with a picture just in it");
        let began = Instant::now();
        // The thread may have it already; otherwise the wait sleeps on it.
        while !matches!(encoder.poll().expect("poll"), Poll::Ready { .. }) {
            assert!(encoder.wait_finished(Duration::from_millis(500)));
            assert!(
                began.elapsed() < Duration::from_millis(200),
                "the wait ran on"
            );
        }
        assert!(!engine.busy(), "the engine busy with its one picture out");
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
}

/// Nothing allocated per picture on the loop's side, once warm: the frame
/// written into an input, its properties written, the submit, the wait, the
/// collect and a rate change.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn a_picture_allocates_nothing_on_the_loops_side() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    for (codec, ten_bit) in [(Codec::H264, false), (Codec::H265, true)] {
        if ten_bit && !ten_bit_hevc() {
            println!("{codec:?} ten-bit: no ten-bit profile on this engine, skipped");
            continue;
        }
        let mut encoder = amf::Encoder::new(
            &runtime,
            &context,
            config(codec, ten_bit, false, amf::Source::Yuv),
        )
        .expect("an encoder");
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
    }
}

/// The triangle wave the RGB picture is drawn from: smooth, so the codec
/// keeps it, and with no wrap for a filter to smear.
fn triangle(value: u32) -> u8 {
    let step = value % 512;
    (if step < 256 { step } else { 511 - step }) as u8
}

/// The levels at `column`, `row` of the RGB picture at `time` -- red, green
/// and blue -- each a wave of its own.
fn rgb_at(column: u32, row: u32, time: u32) -> [u8; 3] {
    [
        triangle(column / 2 + time * 3),
        triangle(row / 2 + time * 2),
        triangle((column + row) / 4 + time),
    ]
}

/// BT.709 in the video range, from its definitions: luma and the two
/// differences of one colour, given in levels.
fn bt709(rgb: [f64; 3]) -> [f64; 3] {
    let [r, g, b] = rgb.map(|level| level / 255.0);
    let (kr, kb) = (0.2126, 0.0722);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let u = (b - y) / (2.0 - 2.0 * kb);
    let v = (r - y) / (2.0 - 2.0 * kr);
    [16.0 + 219.0 * y, 128.0 + 224.0 * u, 128.0 + 224.0 * v]
}

/// An eight-bit level, rounded.
fn level(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

/// A texture of `device` in the capture's layout, BGRA.
fn rgb_texture(device: &Device, width: u32, height: u32) -> Com<ID3D11Texture2D> {
    device
        .texture(&D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        })
        .expect("an RGB texture")
}

/// Thirty pictures drawn into `texture`, each submitted through the one input
/// made over it and decoded back: how many came back, and the worst luma and
/// chroma against BT.709 in the video range, computed from the colours drawn.
fn rgb_round_trip(
    d3d11: &D3d11,
    device: &Device,
    encoder: &mut amf::Encoder<'_>,
    texture: &Com<ID3D11Texture2D>,
    codec: Codec,
) -> (u32, f64, f64) {
    let config = encoder.config();
    let (width, height) = (config.width, config.height);
    let decoding = amd(d3d11);
    let mut decoder = Backend::new(&decoding, (4096, 4096));
    decoder.build(&header(codec, false)).expect("build");
    // An odd pitch, so a pitch mistake shows.
    let pitch = width as usize + 64;
    let mut y = vec![0u8; pitch * height as usize];
    let mut uv = vec![0u8; pitch * height as usize];
    let mut v = vec![0u8; pitch * height as usize];
    let mut rgb = vec![0u8; (width * height * 4) as usize];
    let (mut got, mut want) = (Vec::new(), Vec::new());
    let (mut worst_luma, mut worst_chroma) = (f64::INFINITY, f64::INFINITY);
    let mut taken = 0u32;
    for index in 0..30u32 {
        for row in 0..height {
            for column in 0..width {
                let at = ((row * width + column) * 4) as usize;
                let [r, g, b] = rgb_at(column, row, index);
                rgb[at..at + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        // SAFETY: the device's context on this thread; a live texture and a
        // buffer of its whole size, rows `width * 4` bytes apart. The last
        // picture made from the texture was collected before this.
        unsafe {
            vcall!(
                device.context(),
                UpdateSubresource,
                texture.as_ptr().cast::<ID3D11Resource>(),
                0,
                core::ptr::null(),
                rgb.as_ptr().cast(),
                width * 4,
                0
            );
        }
        encoder.submit_texture(texture, index == 0).expect("submit");
        let (unit, _) = next_unit(encoder);
        decoder.feed(&unit).expect("feed");
        if index + 1 == 30 {
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
            assert_eq!((picture.width, picture.height), (width, height));
            // Pictures come out in order, so the count is the time the
            // picture was drawn at.
            let time = taken;
            taken += 1;
            got.clear();
            want.clear();
            for row in 0..height {
                for column in 0..width {
                    let [luma, _, _] = bt709(rgb_at(column, row, time).map(f64::from));
                    want.push(level(luma));
                    got.push(y[row as usize * pitch + column as usize]);
                }
            }
            worst_luma = worst_luma.min(psnr(&got, &want));
            got.clear();
            want.clear();
            for row in 0..height / 2 {
                for column in 0..width / 2 {
                    let mut mean = [0.0; 3];
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let colour = rgb_at(column * 2 + dx, row * 2 + dy, time);
                        for (sum, level) in mean.iter_mut().zip(colour) {
                            *sum += f64::from(level) / 4.0;
                        }
                    }
                    let [_, u, v] = bt709(mean);
                    want.extend([level(u), level(v)]);
                    let at = row as usize * pitch + column as usize * 2;
                    got.extend_from_slice(&uv[at..at + 2]);
                }
            }
            worst_chroma = worst_chroma.min(psnr(&got, &want));
        }
    }
    decoder.destroy();
    (taken, worst_luma, worst_chroma)
}

/// The captured picture as it is: an RGB texture of the encoder's device,
/// made an input once and submitted for every picture it is redrawn into,
/// decoded back as BT.709 in the video range -- below 720 lines too, where a
/// conversion chosen by the picture's size would be BT.601's. Eight bits
/// only: ten are refused, since the runtime writes ten-bit levels from RGB at
/// the eight-bit scale.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn rgb_as_captured_comes_back_as_bt709() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let runtime = Amf::load().expect("AMD's runtime");
    let context = runtime.context(&device).expect("a context");
    let ten = amf::Encoder::new(
        &runtime,
        &context,
        config(Codec::H265, true, false, amf::Source::Rgb),
    );
    assert!(
        matches!(ten, Err(amf::Error::TenBitRgb)),
        "ten bits from RGB were built"
    );
    for (width, height) in [(WIDTH, HEIGHT), (960, 540)] {
        let texture = rgb_texture(&device, width, height);
        for codec in [Codec::H264, Codec::H265] {
            let mut encoder = amf::Encoder::new(
                &runtime,
                &context,
                amf::Config {
                    width,
                    height,
                    ..config(codec, false, false, amf::Source::Rgb)
                },
            )
            .expect("an encoder");
            let (taken, luma, chroma) =
                rgb_round_trip(&d3d11, &device, &mut encoder, &texture, codec);
            let name = format!("{codec:?} {width}x{height}");
            println!(
                "{name}: {taken} pictures, worst luma {luma:.1} dB, worst chroma {chroma:.1} dB"
            );
            assert_eq!(taken, 30, "{name}: pictures went missing on the way");
            assert!(luma > 40.0, "{name}: luma at {luma:.1} dB is not BT.709");
            assert!(
                chroma > 35.0,
                "{name}: chroma at {chroma:.1} dB is not BT.709"
            );
        }
    }
}
