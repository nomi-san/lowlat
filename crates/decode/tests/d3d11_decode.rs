//! The system's decoding interface decodes every committed clip to the
//! pictures the reference decoder produced, on every adapter offered,
//! full chroma where the device builds its profile and refused as fatal
//! where it does not. Needs a GPU, so off by default: `cargo test -p
//! lowlat-decode --test d3d11_decode -- --ignored --nocapture`, with
//! `LOWLAT_D3D11_ADAPTER` naming one adapter (`luid:HIGH:LOW`, as the
//! drivers' walk prints it) where not every adapter is wanted.

// The interface exists on Windows alone.
#![cfg(windows)]
#![allow(
    clippy::type_complexity,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod common;

use std::collections::BTreeMap;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::d3d11::{Backend, caps, limits, plane_textures};
use lowlat_decode::{Caps, Decoder, Fault, Format};
use lowlat_drivers::d3d11::{D3d11, Device, Event, Luid, SharedTexture};

use common::reader::Reader;

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

/// Every adapter offered, or the one named.
fn adapters(d3d11: &D3d11) -> Vec<Luid> {
    let named = std::env::var("LOWLAT_D3D11_ADAPTER")
        .ok()
        .map(|s| Luid::parse(&s).expect("LOWLAT_D3D11_ADAPTER names no identity"));
    let all: Vec<Luid> = d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .filter(|a| a.decodes_here())
        .map(|a| a.luid)
        .filter(|l| named.is_none_or(|n| n == *l))
        .collect();
    assert!(!all.is_empty(), "no adapter to decode on");
    all
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

/// Decode one clip and compare it with the reference; `Err` says how it
/// differs.
fn check(
    device: &Device,
    clip: &str,
    sums: &str,
    codec: Codec,
    ten_bit: bool,
) -> Result<(), String> {
    let mut backend = Backend::new(device, (4096, 4096));
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
    compare(clip, sums, &ours, &times)
}

/// Decode one clip through the plane textures and compare what a second
/// device reads out of them with the reference; `copying` takes the copy
/// route even where the device would let the split read the surfaces.
fn check_textures(
    device: &Device,
    d3d11: &D3d11,
    clip: &str,
    sums: &str,
    codec: Codec,
    ten_bit: bool,
    copying: bool,
) -> Result<bool, String> {
    let mut backend = Backend::new(device, (4096, 4096));
    if copying {
        backend.force_copy();
    }
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
    let direct = core::cell::Cell::new(None);
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
            direct.set(b.reads_directly());
            let submitted = std::time::Instant::now();
            fence.notify_at(value, &event).expect("a notification");
            assert!(
                event.wait(std::time::Duration::from_secs(2)),
                "the fence never reached {value}"
            );
            let finished = submitted.elapsed().as_micros() as u32;
            timing.set((finished, b.readback_us));
            // From the take to the fence passing, as this thread saw it,
            // beside the device's own timing -- of the picture before this
            // one, by now.
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
            if picture.format.full_chroma() {
                reader.read(handle(2), rows, w * sample, planes.v, planes.v_pitch);
            }
            Ok(Some(picture))
        },
    );
    backend.destroy();
    compare(clip, sums, &ours, &times)?;
    // The submit is what this thread spends: it must never be a wait.
    let mut submits: Vec<u32> = times.iter().map(|t| t.1).collect();
    submits.sort_unstable();
    let at = |q: f64| submits[((submits.len() - 1) as f64 * q) as usize];
    let first = times.first().map_or(0, |t| t.1);
    println!(
        "    submit p50 {} us p95 {} p99 {} max {}; first {first}; over 1 ms {}",
        at(0.5),
        at(0.95),
        at(0.99),
        at(1.0),
        submits.iter().filter(|&&s| s > 1000).count()
    );
    // The device's own timing runs from the take reaching the device to the
    // fence being signalled: never nothing, and never more than this thread
    // saw of it. Less by the driver's telling a waiting thread, which is no
    // one figure across GPUs -- 0.05 ms on one here, 2 on another.
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
    direct
        .get()
        .ok_or_else(|| "no picture came out".to_string())
}

/// Compare what a clip decoded to with its reference sums, printing the
/// timings; `Err` says how it differs.
fn compare(
    clip: &str,
    sums: &str,
    ours: &[(u32, u32)],
    times: &[(u32, u32)],
) -> Result<(), String> {
    let theirs = common::sums(sums);
    // Output order is a presentation matter; the pictures themselves must
    // all be there and all be right.
    let mut expected: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &theirs {
        *expected.entry((s.y, s.uv)).or_default() += 1;
    }
    let mut got: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in ours {
        *got.entry(*s).or_default() += 1;
    }
    let wrong = ours.iter().filter(|s| !expected.contains_key(s)).count();
    let mean = |f: fn(&(u32, u32)) -> u32| {
        times.iter().map(|t| u64::from(f(t))).sum::<u64>() / times.len().max(1) as u64
    };
    println!(
        "  {clip}: {} of {} pictures, {wrong} wrong; wait mean {} us max {}; copy mean {} us max {}",
        ours.len(),
        theirs.len(),
        mean(|t| t.0),
        times.iter().map(|t| t.0).max().unwrap_or(0),
        mean(|t| t.1),
        times.iter().map(|t| t.1).max().unwrap_or(0),
    );
    if wrong > 0 {
        // Which plane is off, for the first few: luma and chroma compared
        // apart, which tells a read-back fault from a decode fault.
        for (n, s) in ours.iter().take(4).enumerate() {
            let y_ok = theirs.iter().any(|t| t.y == s.0);
            let c_ok = theirs.iter().any(|t| t.uv == s.1);
            println!(
                "    picture {n}: luma {} chroma {}",
                if y_ok { "matches" } else { "differs" },
                if c_ok { "matches" } else { "differs" }
            );
        }
    }
    if ours.len() != theirs.len() {
        return Err(format!("{} pictures of {}", ours.len(), theirs.len()));
    }
    if got != expected {
        return Err(format!("{wrong} pictures differ from the reference"));
    }
    Ok(())
}

/// A clip whose profile the device does not build: its first unit must be
/// refused as fatal, never decoded wrongly.
fn refused(device: &Device, clip: &str, codec: Codec, ten_bit: bool) -> Result<(), String> {
    let mut backend = Backend::new(device, (4096, 4096));
    match backend.build(&header(codec, ten_bit)) {
        Err(Fault::Fatal) => return Ok(()),
        Err(e) => return Err(format!("build: {e:?}")),
        Ok(()) => {}
    }
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

#[test]
#[ignore = "requires a GPU"]
fn every_clip_decodes_to_the_reference_pictures_on_every_adapter() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let clips = clips();
    let mut failures = Vec::new();
    for luid in adapters(&d3d11) {
        let device = d3d11.open(luid).expect("a device");
        let caps = caps(&device);
        println!(
            "{luid} {} driver {:?}: {caps:?}; largest H.264 {:?}, HEVC {:?}",
            device.adapter.description,
            device.adapter.driver,
            limits(&device, Codec::H264),
            limits(&device, Codec::H265),
        );
        // Every device this runs on decodes both codecs at eight bits; a
        // probe that stopped saying so would skip their clips, not fail them.
        assert!(caps.h264 && caps.hevc, "{luid}: {caps:?}");
        for (clip, sums, codec, ten_bit) in &clips {
            let result = if able(&caps, clip, *codec, *ten_bit) {
                check(&device, clip, sums, *codec, *ten_bit)
            } else {
                println!("  {clip}: the device builds no decoder for it, refused");
                refused(&device, clip, *codec, *ten_bit)
            };
            if let Err(e) = result {
                println!("  {clip}: FAILED: {e}");
                failures.push(format!("{luid} {clip}: {e}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// **Every clip through the plane textures, on every adapter, both routes.**
/// The split's planes, opened by handle on a second device once the fence
/// has passed and read back, are the reference pictures bit for bit: read
/// from the surfaces directly where the device allows it, and through a copy
/// of each slice, which every device is made to take once. The printed wait
/// is from the submit to the fence passing -- the decode and the split -- and
/// the copy is what the submit cost this thread.
#[test]
#[ignore = "requires a GPU"]
fn every_clip_decodes_through_the_plane_textures_on_every_adapter() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let clips = clips();
    let mut failures = Vec::new();
    for luid in adapters(&d3d11) {
        let device = d3d11.open(luid).expect("a device");
        let caps = caps(&device);
        for copying in [false, true] {
            println!(
                "{luid} {}: {}",
                device.adapter.description,
                if copying {
                    "through a copy"
                } else {
                    "direct where allowed"
                }
            );
            for (clip, sums, codec, ten_bit) in &clips {
                if !able(&caps, clip, *codec, *ten_bit) {
                    continue;
                }
                match check_textures(&device, &d3d11, clip, sums, *codec, *ten_bit, copying) {
                    Ok(direct) if copying && direct => {
                        failures.push(format!("{luid} {clip}: the copy route read directly"));
                    }
                    Ok(direct) => {
                        if !copying && !direct {
                            println!(
                                "    read through a copy: the device refused a shader the surfaces"
                            );
                        }
                    }
                    Err(e) => {
                        println!("  {clip}: FAILED: {e}");
                        failures.push(format!("{luid} {clip} copying={copying}: {e}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

unsafe extern "system" {
    fn GetCurrentThread() -> *mut core::ffi::c_void;
    fn QueryThreadCycleTime(thread: *mut core::ffi::c_void, cycles: *mut u64) -> i32;
}

/// The processor cycles this thread has run so far, counted exactly rather
/// than sampled at the scheduler's tick.
fn thread_cycles() -> u64 {
    let mut cycles = 0u64;
    // SAFETY: the pseudo-handle of this thread; the output is a live local.
    let ok = unsafe { QueryThreadCycleTime(GetCurrentThread(), &raw mut cycles) };
    assert_ne!(ok, 0);
    cycles
}

/// Cycles per microsecond of this thread running flat out, measured by a
/// spin of a known length.
fn cycles_per_us() -> f64 {
    let (cycles, started) = (thread_cycles(), std::time::Instant::now());
    while started.elapsed() < std::time::Duration::from_millis(200) {
        std::hint::spin_loop();
    }
    (thread_cycles() - cycles) as f64 / started.elapsed().as_micros() as f64
}

/// **The read-back's wait for the device sleeps rather than spins.** A
/// clip decoded over and over on each adapter, the thread's processor time
/// compared with the time the takes lasted: a spinning wait would spend
/// processor time for all of it, a sleeping one only for the copy. The
/// thread's time is its cycle count, converted at the rate a spin of known
/// length runs, since the scheduler's own figure moves in whole ticks.
#[test]
#[ignore = "requires a GPU"]
fn the_read_back_wait_sleeps() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let units = common::units("synthetic-720p-h264.bin");
    let pitch = 1280;
    let mut y = vec![0u8; pitch * 720];
    let mut uv = vec![0u8; pitch * 360];
    let rate = cycles_per_us();
    for luid in adapters(&d3d11) {
        let device = d3d11.open(luid).expect("a device");
        let mut backend = Backend::new(&device, (4096, 4096));
        backend.build(&header(Codec::H264, false)).expect("build");
        let (mut wall_us, mut cycles, mut wait_us, mut copy_us, mut pictures) = (0u64, 0, 0, 0, 0);
        for _ in 0..10 {
            for unit in &units {
                backend.feed(unit).expect("feed");
                loop {
                    let mut planes = lowlat_decode::Planes {
                        y: &mut y,
                        y_pitch: pitch,
                        uv: &mut uv,
                        uv_pitch: pitch,
                        v: &mut [],
                        v_pitch: 0,
                    };
                    let before = thread_cycles();
                    let started = std::time::Instant::now();
                    let taken = backend.take(&mut planes).expect("take");
                    wall_us += started.elapsed().as_micros() as u64;
                    cycles += thread_cycles() - before;
                    if taken.is_none() {
                        break;
                    }
                    wait_us += u64::from(backend.decode_us);
                    copy_us += u64::from(backend.readback_us);
                    pictures += 1;
                }
            }
        }
        backend.destroy();
        let cpu_us = (cycles as f64 / rate) as u64;
        println!(
            "{luid} {}: {pictures} pictures; takes {wall_us} us, of it the wait {wait_us} and the copy {copy_us}; the thread ran {cpu_us} us ({cycles} cycles at {rate:.0} a us)",
            device.adapter.description
        );
        assert!(pictures >= 1000, "{luid}: {pictures} pictures");
        // A wait that spun would put all of the takes' time on the thread;
        // half of it is a generous line between the two.
        assert!(
            cpu_us < wall_us / 2 + copy_us,
            "{luid}: the wait spent processor time"
        );
    }
}

/// **The read-back's wait, measured on a clip at a pace.** `LOWLAT_PROBE_CLIP`
/// names a clip by path (a 2560x1440 one, if there is one at hand; the
/// codec is read from the name), `LOWLAT_PROBE_PACE_MS` a sleep between
/// units that stands in for a live stream's gaps, `LOWLAT_D3D11_ADAPTER`
/// the adapter. Prints the wait (the decode and the copy) and the copy out
/// at the median and the 95th percentile, over 600 pictures.
#[test]
#[ignore = "requires a GPU"]
fn wait_probe() {
    let clip =
        std::env::var("LOWLAT_PROBE_CLIP").unwrap_or_else(|_| "synthetic-720p-hevc.bin".into());
    let codec = if clip.contains("hevc") {
        Codec::H265
    } else {
        Codec::H264
    };
    // The depth is read from the name as the codec is: "10" in it is ten
    // bits, so a clip at full chroma reads back through the unpacking too.
    let ten_bit = codec == Codec::H265 && clip.contains("10");
    let pace: u64 = std::env::var("LOWLAT_PROBE_PACE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let d3d11 = D3d11::load().expect("the system's libraries");
    let units = common::units(&clip);
    // Room for the largest picture at full chroma and sixteen bits.
    let pitch = 8192;
    let mut y = vec![0u8; pitch * 2160];
    let mut uv = vec![0u8; pitch * 2160];
    let mut v = vec![0u8; pitch * 2160];
    for luid in adapters(&d3d11) {
        let device = d3d11.open(luid).expect("a device");
        if clip.contains("444") && !caps(&device).hevc_444 {
            println!("{luid} {}: no full chroma", device.adapter.description);
            continue;
        }
        let mut backend = Backend::new(&device, (4096, 4096));
        backend.build(&header(codec, ten_bit)).expect("build");
        let (mut waits, mut copies) = (Vec::new(), Vec::new());
        while waits.len() < 600 {
            for unit in &units {
                backend.feed(unit).expect("feed");
                loop {
                    let mut planes = lowlat_decode::Planes {
                        y: &mut y,
                        y_pitch: pitch,
                        uv: &mut uv,
                        uv_pitch: pitch,
                        v: &mut v,
                        v_pitch: pitch,
                    };
                    if backend.take(&mut planes).expect("take").is_none() {
                        break;
                    }
                    waits.push(backend.decode_us);
                    copies.push(backend.readback_us);
                }
                if pace > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(pace));
                }
            }
        }
        backend.destroy();
        let pct = |v: &mut Vec<u32>, p: usize| {
            v.sort_unstable();
            v[(v.len() - 1) * p / 100]
        };
        println!(
            "{luid} {} {clip} pace {pace} ms: wait p50 {} us p95 {}; copy p50 {} us p95 {}",
            device.adapter.description,
            pct(&mut waits, 50),
            pct(&mut waits, 95),
            pct(&mut copies, 50),
            pct(&mut copies, 95),
        );
    }
}
