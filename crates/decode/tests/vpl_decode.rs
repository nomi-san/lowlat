//! Intel's own decoder decodes every committed clip to the pictures the
//! reference decoder produced **and in the readers' order** -- the order the
//! system's interface lets them out in on the same device, the runtime
//! handing its pictures out in decode order -- by planes and through the
//! plane textures, full chroma included; and through the older runtime's
//! calls, by planes. Needs an Intel GPU and its runtime, so off by default:
//! `cargo test -p lowlat-decode --test vpl_decode -- --ignored --nocapture`.

// The runtime is reached on Windows alone.
#![cfg(windows)]
#![allow(
    clippy::type_complexity,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod common;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::d3d11::plane_textures;
use lowlat_decode::vpl::{Backend, MOST_UNFINISHED, caps};
use lowlat_decode::{Caps, Decoder, Fed, Format};
use lowlat_drivers::d3d11::{D3d11, Device, Event, SharedTexture};
use lowlat_drivers::vpl::{Runtime, Session, Vpl};

use common::reader::Reader;

/// The largest unit a clip carries: room for every committed clip's.
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

/// A device on the Intel GPU, its runtime, and its place in the plain
/// enumeration.
fn intel(d3d11: &D3d11) -> (Device, Vpl, u32) {
    let adapter = d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .find(|a| a.decodes_here() && a.maker() == Some("Intel"))
        .expect("an Intel GPU");
    let vpl = Vpl::for_adapter(&adapter).expect("Intel's runtime");
    let index = d3d11
        .plain_index(adapter.luid)
        .expect("the enumeration")
        .expect("the adapter");
    (d3d11.open(adapter.luid).expect("a device"), vpl, index)
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
/// system's interface decoding it on the same device.
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
/// with the readers'; `Err` says how it differs.
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
    let mean = |f: fn(&(u32, u32)) -> u32| {
        times.iter().map(|t| u64::from(f(t))).sum::<u64>() / times.len().max(1) as u64
    };
    println!(
        "  {clip}: {} of {} pictures, {wrong} wrong, {misplaced} out of the readers' place; wait mean {} us max {}; copy mean {} us max {}",
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

/// Decode one clip by planes in `session` and compare it with the
/// reference and the readers' order on `device`.
fn check(
    session: &Session<'_>,
    runtime: Runtime,
    device: &Device,
    clip: &str,
    sums: &str,
    codec: Codec,
    ten_bit: bool,
) -> Result<(), String> {
    let decodes_on = (runtime == Runtime::Current).then_some(device);
    let mut backend = Backend::new(session, runtime, decodes_on, (4096, 4096), UNIT_BYTES)
        .map_err(|e| format!("new: {e}"))?;
    backend
        .build(&header(codec, ten_bit))
        .map_err(|e| format!("build: {e:?}"))?;
    let (ours, times) = common::decode_clip(
        &mut backend,
        clip,
        |b| b.drain(),
        |b| (b.decode_us, b.readback_us),
    );
    backend.destroy();
    let order = readers_order(device, clip, codec, ten_bit);
    compare(clip, sums, &ours, &times, &order)
}

/// Decode one clip through the plane textures and compare what a second
/// device reads out of them, once the fence has passed, with the reference.
fn check_textures(
    session: &Session<'_>,
    device: &Device,
    d3d11: &D3d11,
    clip: &str,
    sums: &str,
    codec: Codec,
    ten_bit: bool,
) -> Result<(), String> {
    let mut backend = Backend::new(
        session,
        Runtime::Current,
        Some(device),
        (4096, 4096),
        UNIT_BYTES,
    )
    .map_err(|e| format!("new: {e}"))?;
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
            timing.set((submitted.elapsed().as_micros() as u32, b.readback_us));
            let sample = picture.format.sample();
            let w = picture.width as usize;
            let h = picture.height as usize;
            let handle = |p: usize| made[p].as_ref().expect("a plane").handle;
            reader.read(handle(0), h, w * sample, planes.y, planes.y_pitch);
            let rows = picture.format.chroma_rows(h);
            reader.read(handle(1), rows, w * sample, planes.uv, planes.uv_pitch);
            if picture.format.full_chroma() {
                reader.read(handle(2), rows, w * sample, planes.v, planes.v_pitch);
            }
            Ok(Some(picture))
        },
    );
    backend.destroy();
    let order = readers_order(device, clip, codec, ten_bit);
    compare(clip, sums, &ours, &times, &order)
}

/// **Every clip by planes, in the readers' order**, full chroma included,
/// on the current runtime and the device of ours it was handed.
#[test]
#[ignore = "requires an Intel GPU and its runtime"]
fn every_clip_decodes_to_the_reference_pictures_in_order() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let (device, vpl, index) = intel(&d3d11);
    assert_eq!(vpl.runtime(), Runtime::Current);
    let session = vpl.session(&device, index).expect("a session");
    let caps = caps(&session, Runtime::Current);
    println!(
        "{} driver {:?}, runtime {:?}: {caps:?}",
        device.adapter.description,
        device.adapter.driver,
        session.version()
    );
    assert!(caps.h264 && caps.hevc && caps.hevc_10, "{caps:?}");
    let mut failures = Vec::new();
    for (clip, sums, codec, ten_bit) in clips() {
        if !able(&caps, &clip, codec, ten_bit) {
            println!("  {clip}: no decoder for it");
            continue;
        }
        if let Err(e) = check(
            &session,
            Runtime::Current,
            &device,
            &clip,
            &sums,
            codec,
            ten_bit,
        ) {
            println!("  {clip}: FAILED: {e}");
            failures.push(format!("{clip}: {e}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// **Every clip through the plane textures, in the readers' order**: the
/// split's planes, opened by handle on a second device once the fence has
/// passed and read back, are the reference pictures bit for bit.
#[test]
#[ignore = "requires an Intel GPU and its runtime"]
fn every_clip_decodes_through_the_plane_textures_in_order() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let (device, vpl, index) = intel(&d3d11);
    let session = vpl.session(&device, index).expect("a session");
    let caps = caps(&session, Runtime::Current);
    let mut failures = Vec::new();
    for (clip, sums, codec, ten_bit) in clips() {
        if !able(&caps, &clip, codec, ten_bit) {
            continue;
        }
        if let Err(e) = check_textures(&session, &device, &d3d11, &clip, &sums, codec, ten_bit) {
            println!("  {clip}: FAILED: {e}");
            failures.push(format!("{clip}: {e}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// **The older runtime's calls decode every four-two-zero clip by planes, in
/// the readers' order**, into surfaces of the backend's own memory -- reached
/// here through the system directory's loader, which answers with the
/// current runtime in its older role: the calls, not an older part.
#[test]
#[ignore = "requires an Intel GPU and the system directory's loader"]
fn the_older_runtime_decodes_every_clip_by_planes() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let (device, _, index) = intel(&d3d11);
    let system = std::env::var("SystemRoot").expect("the system root");
    let vpl = Vpl::open(
        &format!("{system}\\System32\\libmfxhw64.dll"),
        Runtime::Older,
    )
    .expect("the loader");
    let session = vpl.system_session(index).expect("a session");
    let caps = caps(&session, Runtime::Older);
    println!("older runtime {:?}: {caps:?}", session.version());
    assert!(caps.h264 && caps.hevc, "{caps:?}");
    assert!(!caps.hevc_444 && !caps.hevc_444_10, "{caps:?}");
    let mut failures = Vec::new();
    for (clip, sums, codec, ten_bit) in clips() {
        if !able(&caps, &clip, codec, ten_bit) {
            continue;
        }
        if let Err(e) = check(
            &session,
            Runtime::Older,
            &device,
            &clip,
            &sums,
            codec,
            ten_bit,
        ) {
            println!("  {clip}: FAILED: {e}");
            failures.push(format!("{clip}: {e}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// **A stream whose parameter sets travel in a unit of their own still
/// builds**: the unit of sets alone is kept, and the keyframe that follows,
/// carrying none, is decoded from them -- every picture of the clip comes
/// out.
#[test]
#[ignore = "requires an Intel GPU and its runtime"]
fn a_stream_whose_sets_travel_alone_still_builds() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let (device, vpl, index) = intel(&d3d11);
    let session = vpl.session(&device, index).expect("a session");
    let mut units = common::units("synthetic-720p-hevc.bin");
    // The first unit, split: its video, sequence and picture parameter sets
    // in one unit, its slices in the next.
    let first = units.remove(0);
    let (mut sets, mut slices) = (Vec::new(), Vec::new());
    for unit in lowlat_decode::nal::Units::new(&first) {
        let kind = (unit.bytes[0] >> 1) & 0x3f;
        let to = if (32..=34).contains(&kind) {
            &mut sets
        } else {
            &mut slices
        };
        to.extend_from_slice(&[0, 0, 0, 1]);
        to.extend_from_slice(unit.bytes);
    }
    assert!(
        !sets.is_empty() && !slices.is_empty(),
        "the first unit splits"
    );
    units.insert(0, slices);
    units.insert(0, sets);
    let mut backend = Backend::new(
        &session,
        Runtime::Current,
        Some(&device),
        (4096, 4096),
        UNIT_BYTES,
    )
    .expect("new");
    backend.build(&header(Codec::H265, false)).expect("build");
    let pitch = 1280;
    let (mut y, mut uv) = (vec![0u8; pitch * 720], vec![0u8; pitch * 360]);
    let mut pictures = 0usize;
    for unit in &units {
        if backend.feed(unit).expect("feed") != Fed::Picture {
            continue;
        }
        loop {
            let mut planes = lowlat_decode::Planes {
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
            pictures += 1;
        }
    }
    backend.destroy();
    assert_eq!(pictures, 120, "pictures out of the clip");
}

/// **A device fallen behind holds the next unit back.** A clip fed back to
/// back with every picture split into textures and never waited for: no more
/// than [`MOST_UNFINISHED`] are unfinished when a unit goes in, so one more
/// at most after its picture is split.
#[test]
#[ignore = "requires an Intel GPU and its runtime"]
fn a_device_fallen_behind_holds_the_next_unit_back() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let (device, vpl, index) = intel(&d3d11);
    let session = vpl.session(&device, index).expect("a session");
    let units = common::units("synthetic-720p-hevc.bin");
    let mut backend = Backend::new(
        &session,
        Runtime::Current,
        Some(&device),
        (4096, 4096),
        UNIT_BYTES,
    )
    .expect("new");
    let fence = backend.fence().expect("a split");
    backend.build(&header(Codec::H265, false)).expect("build");
    let planes = plane_textures(Format::Nv12, 1280, 720)
        .map(|p| p.map(|(f, w, h)| device.shared_texture(f, w, h).expect("a plane")));
    let targets = planes.each_ref().map(Option::as_ref);
    let (mut most, mut pictures) = (0u64, 0usize);
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
