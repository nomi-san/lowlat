//! AMD's own decoder decodes every committed 4:2:0 clip to the pictures the
//! reference decoder produced **and in the readers' order** -- the order the
//! system's interface lets them out in on the same device, the runtime
//! handing its pictures out in decode order -- by planes and through the
//! plane textures, and refuses full chroma as fatal. Needs AMD's GPU and
//! runtime, so off by default: `cargo test -p lowlat-decode --test
//! amf_decode -- --ignored --nocapture`.

// The runtime is reached on Windows alone.
#![cfg(windows)]
#![allow(
    clippy::type_complexity,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod common;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::amf::{Backend, MOST_UNFINISHED, caps};
use lowlat_decode::d3d11::plane_textures;
use lowlat_decode::{Caps, Decoder, Fault, Fed, Format};
use lowlat_drivers::amf::Amf;
use lowlat_drivers::d3d11::{D3d11, Device, Event, SharedTexture};

use common::reader::Reader;

/// The buffer a unit is handed over in: room for every committed clip's.
const UNIT_BYTES: usize = 1 << 22;

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

/// Every clip: the synthetics, then every fixture of both codecs, with the
/// codec and depth each is decoded as.
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
        let ten_bit = clip.contains("main10") || clip.contains("444-10");
        out.push((clip, sums, Codec::H265, ten_bit));
    }
    out
}

fn able(caps: &Caps, clip: &str, codec: Codec, ten_bit: bool) -> bool {
    match (codec, ten_bit, clip.contains("444")) {
        (Codec::H264, _, _) => caps.h264,
        (Codec::H265, false, false) => caps.hevc,
        (Codec::H265, true, false) => caps.hevc_10,
        (Codec::H265, false, true) => caps.hevc_444,
        (Codec::H265, true, true) => caps.hevc_444_10,
    }
}

/// The order the readers let a clip's pictures out in, taken from the
/// system's interface decoding it on the same device: the order the decoder
/// here must keep, which is the readers' and not the runtime's. A stream
/// that declares nothing about its reordering is held back only once it
/// proves it must, so this can differ from the reference decoder's order.
fn readers_order(device: &Device, clip: &str, codec: Codec, ten_bit: bool) -> Vec<(u32, u32)> {
    let mut backend = lowlat_decode::d3d11::Backend::new(device, (4096, 4096));
    backend
        .build(&header(codec, ten_bit))
        .expect("the system's interface builds");
    let (order, _) = common::decode_clip(&mut backend, clip, |b| b.drain(), |_| (0, 0));
    backend.destroy();
    order
}

/// Compare what a clip decoded to with its reference sums, and its order
/// with the readers', printing the timings; `Err` says how it differs. A
/// picture right but out of the readers' place is a failure here: the order
/// is the readers' work on top of a runtime that hands pictures out in
/// decode order, and a surface put in the wrong slot shows only so.
fn compare(
    clip: &str,
    sums: &str,
    ours: &[(u32, u32)],
    times: &[(u32, u32)],
    order: &[(u32, u32)],
) -> Result<(), String> {
    let mut theirs = common::sums(sums);
    theirs.sort_by_key(|s| s.picture);
    let wrong = ours
        .iter()
        .filter(|s| !theirs.iter().any(|t| (t.y, t.uv) == **s))
        .count();
    let misplaced = ours.iter().zip(order).filter(|(o, r)| o != r).count();
    let unlike_reference = ours
        .iter()
        .zip(&theirs)
        .filter(|(o, t)| **o != (t.y, t.uv))
        .count();
    let mean = |f: fn(&(u32, u32)) -> u32| {
        times.iter().map(|t| u64::from(f(t))).sum::<u64>() / times.len().max(1) as u64
    };
    println!(
        "  {clip}: {} of {} pictures, {wrong} wrong, {misplaced} out of the readers' place ({unlike_reference} of the reference's); wait mean {} us max {}; copy mean {} us max {}",
        ours.len(),
        theirs.len(),
        mean(|t| t.0),
        times.iter().map(|t| t.0).max().unwrap_or(0),
        mean(|t| t.1),
        times.iter().map(|t| t.1).max().unwrap_or(0),
    );
    if ours.len() != theirs.len() || order.len() != theirs.len() {
        return Err(format!(
            "{} pictures of {}, the readers' {}",
            ours.len(),
            theirs.len(),
            order.len()
        ));
    }
    if wrong > 0 || misplaced > 0 {
        return Err(format!(
            "{wrong} pictures differ from the reference, {misplaced} are not at the readers' place"
        ));
    }
    Ok(())
}

/// Decode one clip by planes and compare it with the reference.
fn check(
    amf: &Amf,
    device: &Device,
    clip: &str,
    sums: &str,
    codec: Codec,
    ten_bit: bool,
) -> Result<(), String> {
    let context = amf.context(device).map_err(|e| format!("context: {e:?}"))?;
    let mut backend =
        Backend::new(amf, &context, (4096, 4096), UNIT_BYTES).map_err(|e| format!("new: {e}"))?;
    backend
        .build(&header(codec, ten_bit))
        .map_err(|e| format!("build: {e:?}"))?;
    let (ours, times) = common::decode_clip(
        &mut backend,
        clip,
        |b| b.drain(),
        |b| (b.decode_us, b.readback_us),
    );
    let low_latency = backend.low_latency();
    backend.destroy();
    let order = readers_order(device, clip, codec, ten_bit);
    compare(clip, sums, &ours, &times, &order)?;
    if low_latency == Some(true) {
        Ok(())
    } else {
        Err(format!("the decoder's low-latency mode: {low_latency:?}"))
    }
}

/// Decode one clip through the plane textures and compare what a second
/// device reads out of them, once the fence has passed, with the reference.
fn check_textures(
    amf: &Amf,
    device: &Device,
    d3d11: &D3d11,
    clip: &str,
    sums: &str,
    codec: Codec,
    ten_bit: bool,
) -> Result<(), String> {
    let context = amf.context(device).map_err(|e| format!("context: {e:?}"))?;
    let mut backend =
        Backend::new(amf, &context, (4096, 4096), UNIT_BYTES).map_err(|e| format!("new: {e}"))?;
    let fence = backend.fence().ok_or("the device has no split")?;
    backend
        .build(&header(codec, ten_bit))
        .map_err(|e| format!("build: {e:?}"))?;
    let event = Event::new().expect("an event");
    let mut reader = Reader {
        device: d3d11.open(device.adapter.luid).expect("a second device"),
        opened: Vec::new(),
    };
    let mut textures: Option<((u32, u32, Format), [Option<SharedTexture>; 3])> = None;
    let timing = core::cell::Cell::new((0u32, 0u32));
    let spans = core::cell::RefCell::new(Vec::new());
    let (ours, times) = common::decode_clip_with(
        &mut backend,
        clip,
        |b| b.drain(),
        |_| timing.get(),
        |b, planes| {
            let Some(layout @ (width, height, format)) = b.output() else {
                return Ok(None);
            };
            if textures.as_ref().is_none_or(|(l, _)| *l != layout) {
                let made = plane_textures(format, width, height).map(|p| {
                    p.map(|(f, w, h)| device.shared_texture(f, w, h).expect("a shared plane"))
                });
                textures = Some((layout, made));
                reader.opened.clear();
            }
            let (_, made) = textures.as_ref().expect("made");
            let began = std::time::Instant::now();
            let Some((picture, value)) = b.take_to_textures(made.each_ref().map(Option::as_ref))?
            else {
                return Ok(None);
            };
            let submitted = std::time::Instant::now();
            fence.notify_at(value, &event).expect("a notification");
            assert!(
                event.wait(std::time::Duration::from_secs(2)),
                "the fence never reached {value}"
            );
            let finished = submitted.elapsed().as_micros() as u32;
            timing.set((finished, b.readback_us));
            spans
                .borrow_mut()
                .push((began.elapsed().as_micros() as u32, b.decode_us));
            let sample = picture.format.sample();
            let w = picture.width as usize;
            let h = picture.height as usize;
            let handle = |p: usize| made[p].as_ref().expect("a plane").handle;
            reader.read(handle(0), h, w * sample, planes.y, planes.y_pitch);
            let rows = picture.format.chroma_rows(h);
            reader.read(handle(1), rows, w * sample, planes.uv, planes.uv_pitch);
            Ok(Some(picture))
        },
    );
    backend.destroy();
    let order = readers_order(device, clip, codec, ten_bit);
    compare(clip, sums, &ours, &times, &order)?;
    // The submit is what this thread spends: the runtime's reading of the
    // unit and the split's queueing, never a wait for the decode.
    let mut submits: Vec<u32> = times.iter().map(|t| t.1).collect();
    submits.sort_unstable();
    let at = |q: f64| submits[((submits.len() - 1) as f64 * q) as usize];
    println!(
        "    submit p50 {} us p95 {} p99 {} max {}",
        at(0.5),
        at(0.95),
        at(0.99),
        at(1.0)
    );
    // The device's own timing of a picture runs from its take to the fence:
    // never nothing, and never more than this thread saw of it.
    let every = lowlat_decode::d3d11::TIMED_EVERY as usize;
    let spans = spans.into_inner();
    let pairs: Vec<(u32, u32)> = spans
        .iter()
        .zip(spans.iter().skip(1))
        .step_by(every)
        .map(|(this, next)| (this.0, next.1))
        .collect();
    let mut seen: Vec<u32> = pairs.iter().map(|p| p.0).collect();
    let mut timed: Vec<u32> = pairs.iter().map(|p| p.1).collect();
    seen.sort_unstable();
    timed.sort_unstable();
    let median = |v: &[u32]| v.get(v.len() / 2).copied().unwrap_or(0);
    let (seen, timed) = (median(&seen), median(&timed));
    println!("    timed pictures: take to fence p50 {seen} us; on the device p50 {timed} us");
    if timed == 0 || timed > seen + 100 {
        return Err(format!(
            "the device's timing p50 {timed} us against {seen} from the take to the fence"
        ));
    }
    Ok(())
}

/// A clip the decoder takes no profile for: its first picture must be
/// refused as fatal, never decoded wrongly.
fn refused(
    amf: &Amf,
    device: &Device,
    clip: &str,
    codec: Codec,
    ten_bit: bool,
) -> Result<(), String> {
    let context = amf.context(device).map_err(|e| format!("context: {e:?}"))?;
    let mut backend =
        Backend::new(amf, &context, (4096, 4096), UNIT_BYTES).map_err(|e| format!("new: {e}"))?;
    backend
        .build(&header(codec, ten_bit))
        .map_err(|e| format!("build: {e:?}"))?;
    let first = &common::units(clip)[0];
    let fed = backend.feed(first);
    backend.destroy();
    match fed {
        Err(Fault::Fatal) => Ok(()),
        other => Err(format!(
            "the first unit was not refused as fatal: {other:?}"
        )),
    }
}

/// **Every clip by planes, in the reference's order**, in the runtime's
/// low-latency mode; full chroma, which the AMD has no decoder for, refused.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn every_clip_decodes_to_the_reference_pictures_in_order() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let amf = Amf::load().expect("AMD's runtime");
    let (caps, low_latency) = caps(&amf, &device);
    println!(
        "{} driver {:?}, runtime {:#x}: {caps:?}, low latency {low_latency}",
        device.adapter.description,
        device.adapter.driver,
        amf.version()
    );
    assert!(caps.h264 && caps.hevc && caps.hevc_10, "{caps:?}");
    assert!(!caps.hevc_444 && !caps.hevc_444_10, "{caps:?}");
    assert!(low_latency, "the runtime has no low-latency mode here");
    let mut failures = Vec::new();
    for (clip, sums, codec, ten_bit) in clips() {
        let result = if able(&caps, &clip, codec, ten_bit) {
            check(&amf, &device, &clip, &sums, codec, ten_bit)
        } else {
            println!("  {clip}: no decoder for it, refused");
            refused(&amf, &device, &clip, codec, ten_bit)
        };
        if let Err(e) = result {
            println!("  {clip}: FAILED: {e}");
            failures.push(format!("{clip}: {e}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// **Every clip through the plane textures, in the reference's order**: the
/// split's planes, opened by handle on a second device once the fence has
/// passed and read back, are the reference pictures bit for bit. The
/// printed wait is from the submit to the fence passing -- the decode and
/// the split -- and the copy is what the take cost this thread.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn every_clip_decodes_through_the_plane_textures_in_order() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let amf = Amf::load().expect("AMD's runtime");
    let (caps, _) = caps(&amf, &device);
    let mut failures = Vec::new();
    for (clip, sums, codec, ten_bit) in clips() {
        if !able(&caps, &clip, codec, ten_bit) {
            continue;
        }
        if let Err(e) = check_textures(&amf, &device, &d3d11, &clip, &sums, codec, ten_bit) {
            println!("  {clip}: FAILED: {e}");
            failures.push(format!("{clip}: {e}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// **A device fallen behind holds the next unit back.** A clip fed back to
/// back -- faster than the engine decodes it -- with every picture split
/// into textures and never waited for: the runtime would take some thirty
/// units ahead, and the pictures would run a hundred milliseconds behind;
/// here no more than [`MOST_UNFINISHED`] are unfinished when a unit goes
/// in, so one more at most after its picture is split.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn a_device_fallen_behind_holds_the_next_unit_back() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = amd(&d3d11);
    let amf = Amf::load().expect("AMD's runtime");
    let units = common::units("synthetic-720p-hevc.bin");
    let context = amf.context(&device).expect("a context");
    let mut backend = Backend::new(&amf, &context, (4096, 4096), UNIT_BYTES).expect("new");
    let fence = backend.fence().expect("a split");
    backend.build(&header(Codec::H265, false)).expect("build");
    let planes = plane_textures(Format::Nv12, 1280, 720)
        .map(|p| p.map(|(f, w, h)| device.shared_texture(f, w, h).expect("a plane")));
    let targets = planes.each_ref().map(Option::as_ref);
    let (mut most, mut pictures) = (0u64, 0usize);
    // The clip over and over: each pass begins at its keyframe.
    for _ in 0..5 {
        for unit in &units {
            if backend.feed(unit).expect("feed") != Fed::Picture {
                continue;
            }
            while let Some((_, value)) = backend.take_to_textures(targets).expect("take") {
                most = most.max(value.saturating_sub(fence.completed()));
                pictures += 1;
            }
        }
    }
    backend.destroy();
    println!("{pictures} pictures, at most {most} unfinished after a take");
    assert!(pictures >= 500, "{pictures} pictures");
    assert!(
        most <= MOST_UNFINISHED + 1,
        "{most} pictures unfinished after a take"
    );
}
