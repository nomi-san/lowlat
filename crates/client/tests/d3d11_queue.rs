//! The picture queue on Windows, end to end: pictures split into their
//! slots' textures -- decoded by the system's interface or by AMD's decoder
//! -- or copied there by the vendor's runtime, published before their device
//! work is finished, and handed out only once it is. A
//! consumer on another thread takes pictures as an application would and
//! reads each back through its handles on a device of its own; every one
//! must be a reference picture, whole. A picture handed out before its fence
//! passed, or written again while held, reads as one that is not. Needs a
//! GPU, so off by default: `cargo test -p lowlat-client --test d3d11_queue
//! -- --ignored --nocapture`, with `LOWLAT_D3D11_ADAPTER` naming one adapter.

#![cfg(windows)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lowlat_client::config::FrameKind;
use lowlat_client::frames::{Frame, Frames, Vendor};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::d3d11::Backend;
use lowlat_decode::{Decoder, Fed, Picture, amf, nvdec};
use lowlat_drivers::amf::Amf;
use lowlat_drivers::cuda::Cuda;
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::d3d11::{Com, D3d11, Device, Luid};
use lowlat_drivers::ffi::d3d11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Resource, ID3D11Texture2D,
};
use lowlat_drivers::vcall;

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../decode/tests/data")
        .join(name)
}

/// The access units of a clip, each behind its length.
fn units(name: &str) -> Vec<Vec<u8>> {
    let bytes = std::fs::read(data(name)).unwrap();
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        out.push(bytes[at..at + len].to_vec());
        at += len;
    }
    out
}

/// The reference pictures' luma and chroma checksums.
fn sums(name: &str) -> BTreeSet<(u32, u32)> {
    std::fs::read_to_string(data(name))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<u64> = l.split_whitespace().map(|v| v.parse().unwrap()).collect();
            (f[1] as u32, f[2] as u32)
        })
        .collect()
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// The application's device: opens a plane by its handle and reads it back.
struct Reader {
    device: Device,
}

impl Reader {
    /// `rows` rows of `row_bytes` of the plane behind `handle`.
    fn read(&self, handle: u64, rows: usize, row_bytes: usize) -> Vec<u8> {
        let texture = self.device.open_shared(handle).expect("the plane opened");
        // SAFETY: plain data the call fills whole.
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { core::mem::zeroed() };
        // SAFETY: a live texture; the output is a live local.
        unsafe { vcall!(texture.as_ptr(), GetDesc, &raw mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ as u32;
        desc.MiscFlags = 0;
        let mut raw: *mut ID3D11Texture2D = core::ptr::null_mut();
        // SAFETY: a live device; the description and output are live.
        let hr = unsafe {
            vcall!(
                self.device.device(),
                CreateTexture2D,
                &raw const desc,
                core::ptr::null(),
                &raw mut raw
            )
        }
        .unwrap();
        assert!(hr >= 0);
        // SAFETY: a texture whose reference the call handed over.
        let staging = unsafe { Com::from_raw(raw) }.unwrap();
        let context = self.device.context();
        let staging = staging.as_ptr().cast::<ID3D11Resource>();
        // SAFETY: a live context on this thread; both are this device's.
        unsafe {
            vcall!(
                context,
                CopyResource,
                staging,
                texture.as_ptr().cast::<ID3D11Resource>()
            )
        };
        // SAFETY: plain data the call fills.
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { core::mem::zeroed() };
        // SAFETY: as above; the output is live.
        let hr = unsafe { vcall!(context, Map, staging, 0, D3D11_MAP_READ, 0, &raw mut mapped) }
            .unwrap();
        assert!(hr >= 0, "map: 0x{hr:08x}");
        let mut out = Vec::with_capacity(rows * row_bytes);
        for row in 0..rows {
            // SAFETY: the mapping holds `RowPitch` bytes a row for every row.
            out.extend_from_slice(unsafe {
                core::slice::from_raw_parts(
                    mapped
                        .pData
                        .cast::<u8>()
                        .add(row * mapped.RowPitch as usize),
                    row_bytes,
                )
            });
        }
        // SAFETY: mapped above, unmapped once.
        unsafe { vcall!(context, Unmap, staging, 0) };
        out
    }
}

/// What a consumer saw.
#[derive(Debug, Default)]
struct Seen {
    pictures: usize,
    wrong: usize,
    /// Microseconds from each picture's publish to its being seen finished.
    ready: Vec<u32>,
}

/// When each picture was published, by the number it carries as its
/// generation: what the consumer times its being seen finished from.
type Published = Arc<Mutex<Vec<Instant>>>;

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

/// The application: takes pictures from `frames` as they come, on a thread
/// of its own, and reads each back on `reader`'s device against `expected`,
/// until `done` and nothing more comes.
fn consume(
    frames: &Arc<Frames>,
    reader: Reader,
    expected: BTreeSet<(u32, u32)>,
    done: &Arc<AtomicBool>,
    published: &Published,
) -> std::thread::JoinHandle<Seen> {
    let frames = Arc::clone(frames);
    let done = Arc::clone(done);
    let published = Arc::clone(published);
    std::thread::spawn(move || {
        let mut seen = Seen::default();
        let mut after = 0;
        loop {
            // Once everything is queued, what is still on the device is
            // waited for at length: a loaded device finishes a burst late.
            let finished = done.load(Ordering::Acquire);
            let wait = Duration::from_millis(if finished { 2000 } else { 50 });
            let Some(held) = frames.acquire(after, wait).unwrap() else {
                if finished {
                    break;
                }
                continue;
            };
            after = held.seq;
            let f = held.frame;
            let at = published.lock().unwrap()[f.generation as usize - 1];
            seen.ready.push(at.elapsed().as_micros() as u32);
            let handle = held.handle.expect("a picture of textures");
            let sample = f.format.sample();
            let (w, h) = (f.width as usize, f.height as usize);
            let luma = reader.read(handle.textures[0], h, w * sample);
            let rows = f.format.chroma_rows(h);
            let mut chroma = reader.read(handle.textures[1], rows, w * sample);
            if f.format.full_chroma() {
                chroma.extend(reader.read(handle.textures[2], rows, w * sample));
            }
            seen.pictures += 1;
            if !expected.contains(&(crc32(&luma), crc32(&chroma))) {
                seen.wrong += 1;
            }
            frames.release(held.index);
        }
        seen
    })
}

/// A picture's frame as the decode thread publishes it, numbered in its
/// generation by when it was published.
fn frame(picture: &Picture, published: &Published) -> Frame {
    let generation = {
        let mut published = published.lock().unwrap();
        published.push(Instant::now());
        published.len() as u32
    };
    Frame {
        format: picture.format,
        width: picture.width,
        height: picture.height,
        rotation: Rotation::None,
        generation,
        order: picture.order,
        full_range: picture.full_range,
        arrived: None,
        pitch: 0,
        uv_offset: 0,
        v_offset: 0,
        handle: None,
    }
}

/// Decode `clip` into a queue of the handle kind on `luid`, a picture every
/// `pace` (back to back without), while a consumer takes pictures and reads
/// them back.
fn run(
    d3d11: &D3d11,
    luid: Luid,
    clip: &str,
    codec: Codec,
    ten_bit: bool,
    pace: Option<Duration>,
) -> Seen {
    let expected = sums(&clip.replace(".bin", ".sums"));
    let device = Arc::new(d3d11.open(luid).expect("a device"));
    let frames = Arc::new(Frames::new((4096, 4096), FrameKind::Handle));
    let mut backend = Backend::new(&device, (4096, 4096));
    frames.open_device(Arc::clone(&device), backend.fence().expect("a fence"), None);
    backend.build(&header(codec, ten_bit)).expect("build");

    let done = Arc::new(AtomicBool::new(false));
    let reader = Reader {
        device: d3d11.open(luid).expect("the application's device"),
    };
    let published = Published::default();
    let consumer = consume(&frames, reader, expected, &done, &published);

    for unit in units(clip) {
        let started = Instant::now();
        if backend.feed(&unit).expect("feed") == Fed::Picture {
            while let Some((width, height, format)) = backend.output() {
                let Some(mut filling) = frames.fill() else {
                    break;
                };
                let planes = filling
                    .textures_for(width, height, format)
                    .expect("the slot's textures");
                let Some((picture, value)) = backend.take_to_textures(planes).expect("take") else {
                    break;
                };
                filling.publish_gated(frame(&picture, &published), value);
            }
        }
        if let Some(pace) = pace {
            std::thread::sleep(pace.saturating_sub(started.elapsed()));
        }
    }
    done.store(true, Ordering::Release);
    let seen = consumer.join().expect("the consumer");
    backend.destroy();
    seen
}

/// As [`run`], through the vendor's decoder on `luid`, its textures on a
/// device of the library's own there; `mapped` makes the decoder map its own
/// surfaces. With whether it decoded into the backend's own.
fn run_vendor(
    d3d11: &D3d11,
    luid: Luid,
    clip: &str,
    (codec, ten_bit): (Codec, bool),
    pace: Option<Duration>,
    mapped: bool,
) -> (Seen, bool) {
    let expected = sums(&clip.replace(".bin", ".sums"));
    let cuda = Cuda::load().expect("the vendor's runtime");
    let compute = cuda
        .device_for_luid(luid.value())
        .expect("the adapter's compute device");
    let context = cuda.retain_primary(&compute).expect("its context");
    context.make_current().expect("current here");
    let cuvid = Cuvid::load().expect("the decode interface");
    let cuda = Arc::new(cuda);
    let context = Arc::new(context);
    let device = Arc::new(d3d11.open(luid).expect("the textures' device"));
    let fence = Arc::new(device.fence(0).expect("a fence"));
    let frames = Arc::new(Frames::new((4096, 4096), FrameKind::Handle));
    let vendor = Vendor {
        context: Arc::clone(&context),
        cuda: Arc::clone(&cuda),
    };
    frames.open_device(Arc::clone(&device), Arc::clone(&fence), Some(vendor));
    let mut backend = nvdec::Backend::new(&cuda, &cuvid, (4096, 4096), 1 << 20);
    if mapped {
        backend.force_mapped();
    }
    backend.attach_textures(device, fence);
    backend.build(&header(codec, ten_bit)).expect("build");

    let done = Arc::new(AtomicBool::new(false));
    let reader = Reader {
        device: d3d11.open(luid).expect("the application's device"),
    };
    let published = Published::default();
    let consumer = consume(&frames, reader, expected, &done, &published);

    for unit in units(clip) {
        let started = Instant::now();
        if backend.feed(&unit).expect("feed") == Fed::Picture {
            while let Some((width, height, format)) = backend.output() {
                let Some(mut filling) = frames.fill() else {
                    break;
                };
                let planes = filling
                    .registered_for(width, height, format)
                    .expect("the slot's textures, registered");
                let Some((picture, value)) = backend.take_to_textures(planes).expect("take") else {
                    break;
                };
                filling.publish_gated(frame(&picture, &published), value);
            }
        }
        if let Some(pace) = pace {
            std::thread::sleep(pace.saturating_sub(started.elapsed()));
        }
    }
    done.store(true, Ordering::Release);
    let seen = consumer.join().expect("the consumer");
    let own = backend.decodes_into_own_surfaces();
    backend.destroy();
    (seen, own)
}

/// As [`run`], through AMD's decoder on `luid`, its pictures textures of a
/// device of the library's own there, split into the slots'.
fn run_amd(
    d3d11: &D3d11,
    luid: Luid,
    clip: &str,
    (codec, ten_bit): (Codec, bool),
    pace: Option<Duration>,
) -> Seen {
    let expected = sums(&clip.replace(".bin", ".sums"));
    let runtime = Amf::load().expect("AMD's runtime");
    let device = Arc::new(d3d11.open(luid).expect("a device"));
    let frames = Arc::new(Frames::new((4096, 4096), FrameKind::Handle));
    let context = runtime.context(&device).expect("a context");
    let mut backend = amf::Backend::new(&runtime, &context, (4096, 4096), 1 << 20).expect("new");
    frames.open_device(Arc::clone(&device), backend.fence().expect("a fence"), None);
    backend.build(&header(codec, ten_bit)).expect("build");

    let done = Arc::new(AtomicBool::new(false));
    let reader = Reader {
        device: d3d11.open(luid).expect("the application's device"),
    };
    let published = Published::default();
    let consumer = consume(&frames, reader, expected, &done, &published);

    for unit in units(clip) {
        let started = Instant::now();
        if backend.feed(&unit).expect("feed") == Fed::Picture {
            while let Some((width, height, format)) = backend.output() {
                let Some(mut filling) = frames.fill() else {
                    break;
                };
                let planes = filling
                    .textures_for(width, height, format)
                    .expect("the slot's textures");
                let Some((picture, value)) = backend.take_to_textures(planes).expect("take") else {
                    break;
                };
                filling.publish_gated(frame(&picture, &published), value);
            }
        }
        if let Some(pace) = pace {
            std::thread::sleep(pace.saturating_sub(started.elapsed()));
        }
    }
    done.store(true, Ordering::Release);
    let seen = consumer.join().expect("the consumer");
    backend.destroy();
    seen
}

fn percentile(values: &mut [u32], q: f64) -> u32 {
    values.sort_unstable();
    values
        .get(((values.len().max(1) - 1) as f64 * q) as usize)
        .copied()
        .unwrap_or(0)
}

/// **The queue never hands out an unfinished or overwritten picture**, on
/// every adapter, back to back and at 120 pictures a second: every picture a
/// consumer takes reads back as a reference picture, and each was seen
/// finished only after its submit.
#[test]
#[ignore = "requires a GPU"]
fn every_picture_handed_out_is_finished_and_whole() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let named = std::env::var("LOWLAT_D3D11_ADAPTER")
        .ok()
        .map(|s| Luid::parse(&s).expect("LOWLAT_D3D11_ADAPTER names no identity"));
    let adapters: Vec<Luid> = d3d11
        .adapters()
        .unwrap()
        .into_iter()
        .filter(|a| a.decodes_here())
        .map(|a| a.luid)
        .filter(|l| named.is_none_or(|n| n == *l))
        .collect();
    let mut failures = Vec::new();
    for luid in adapters {
        for (clip, codec, ten_bit) in [
            ("synthetic-720p-h264.bin", Codec::H264, false),
            ("synthetic-720p-hevc10.bin", Codec::H265, true),
        ] {
            for pace in [None, Some(Duration::from_millis(8))] {
                let mut seen = run(&d3d11, luid, clip, codec, ten_bit, pace);
                println!(
                    "{luid} {clip} {}: {} pictures handed out, {} wrong; seen finished after p50 {} us p99 {}",
                    if pace.is_some() {
                        "at 120/s"
                    } else {
                        "back to back"
                    },
                    seen.pictures,
                    seen.wrong,
                    percentile(&mut seen.ready, 0.5),
                    percentile(&mut seen.ready, 0.99),
                );
                if seen.wrong > 0 || seen.pictures == 0 {
                    failures.push(format!("{luid} {clip} {pace:?}: {seen:?}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// **As above, through the vendor's decoder**, on each of its GPUs, in both
/// of its modes -- into surfaces of its own where the driver can, and mapping
/// the decoder's, which every clip is made to take once -- back to back and at
/// 120 pictures a second: every picture a consumer takes reads back as a
/// reference picture.
#[test]
#[ignore = "requires the vendor's GPU"]
fn every_picture_the_vendor_hands_out_is_finished_and_whole() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let named = std::env::var("LOWLAT_D3D11_ADAPTER")
        .ok()
        .map(|s| Luid::parse(&s).expect("LOWLAT_D3D11_ADAPTER names no identity"));
    let adapters: Vec<Luid> = d3d11
        .adapters()
        .unwrap()
        .into_iter()
        .filter(|a| a.decodes_here() && a.maker() == Some("NVIDIA"))
        .map(|a| a.luid)
        .filter(|l| named.is_none_or(|n| n == *l))
        .collect();
    assert!(!adapters.is_empty(), "none of the vendor's GPUs here");
    let mut failures = Vec::new();
    for luid in adapters {
        for (clip, codec, ten_bit) in [
            ("synthetic-720p-h264.bin", Codec::H264, false),
            ("synthetic-720p-hevc10.bin", Codec::H265, true),
            ("fixtures/hevc-nvenc-444-10.bin", Codec::H265, true),
        ] {
            for mapped in [false, true] {
                for pace in [None, Some(Duration::from_millis(8))] {
                    let (mut seen, own) =
                        run_vendor(&d3d11, luid, clip, (codec, ten_bit), pace, mapped);
                    println!(
                        "{luid} {clip} {} {}: {} pictures handed out, {} wrong; seen finished after p50 {} us p99 {}",
                        if own { "own surfaces" } else { "mapped" },
                        if pace.is_some() {
                            "at 120/s"
                        } else {
                            "back to back"
                        },
                        seen.pictures,
                        seen.wrong,
                        percentile(&mut seen.ready, 0.5),
                        percentile(&mut seen.ready, 0.99),
                    );
                    if seen.wrong > 0 || seen.pictures == 0 {
                        failures.push(format!("{luid} {clip} {mapped} {pace:?}: {seen:?}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// **As above, through AMD's decoder** on its GPU, back to back and at 120
/// pictures a second: its pictures are handed out still decoding, the split
/// queued behind them, so every picture a consumer takes must read back as
/// a reference picture.
#[test]
#[ignore = "requires AMD's GPU and runtime"]
fn every_picture_amd_hands_out_is_finished_and_whole() {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let luid = d3d11
        .adapters()
        .unwrap()
        .into_iter()
        .find(|a| a.decodes_here() && a.maker() == Some("AMD"))
        .map(|a| a.luid)
        .expect("an AMD GPU");
    let mut failures = Vec::new();
    for (clip, codec, ten_bit) in [
        ("synthetic-720p-h264.bin", Codec::H264, false),
        ("synthetic-720p-hevc10.bin", Codec::H265, true),
    ] {
        for pace in [None, Some(Duration::from_millis(8))] {
            let mut seen = run_amd(&d3d11, luid, clip, (codec, ten_bit), pace);
            println!(
                "{luid} {clip} {}: {} pictures handed out, {} wrong; seen finished after p50 {} us p99 {}",
                if pace.is_some() {
                    "at 120/s"
                } else {
                    "back to back"
                },
                seen.pictures,
                seen.wrong,
                percentile(&mut seen.ready, 0.5),
                percentile(&mut seen.ready, 0.99),
            );
            if seen.wrong > 0 || seen.pictures == 0 {
                failures.push(format!("{luid} {clip} {pace:?}: {seen:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
