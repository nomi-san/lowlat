//! The vendor's device decodes every committed clip to the pictures the
//! reference decoder produced, full chroma included. Needs the vendor's
//! runtime and decode interface, so it is off by default:
//! `cargo test -p lowlat-decode --test nvdec_decode -- --ignored`.

#![allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

mod common;

use std::collections::BTreeMap;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::nvdec::{Backend, caps};
use lowlat_decode::{Caps, Decoder};
use lowlat_drivers::cuda::{Context, Cuda};
use lowlat_drivers::cuvid::Cuvid;
use lowlat_drivers::ffi::cuvid::{
    CUVIDDECODECAPS, cudaVideoChromaFormat_420, cudaVideoCodec_H264, cudaVideoCodec_HEVC,
};

/// A unit no committed clip exceeds.
const MAX_UNIT: usize = 1 << 20;

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

/// The runtimes, with the first device's context current on this thread.
fn open() -> (Cuda, Context, Cuvid) {
    let cuda = Cuda::load().expect("compute runtime");
    let device = cuda.any_device().expect("a device");
    let context = cuda.retain_primary(&device).expect("context");
    context.make_current().expect("current");
    let cuvid = Cuvid::load().expect("decode interface");
    (cuda, context, cuvid)
}

/// The clip's pictures' sums and timings, and whether the decoder decoded
/// into the backend's own surfaces.
fn decode(
    backend: &mut Backend<'_>,
    clip: &str,
    codec: Codec,
    ten_bit: bool,
) -> (Vec<(u32, u32)>, Vec<(u32, u32)>, bool) {
    backend.build(&header(codec, ten_bit)).expect("build");
    let (sums, times) = common::decode_clip(
        backend,
        clip,
        |b| b.drain(),
        |b| (b.decode_us, b.readback_us),
    );
    let own = backend.decodes_into_own_surfaces();
    backend.destroy();
    (sums, times, own)
}

fn check(
    cuda: &Cuda,
    cuvid: &Cuvid,
    able: &Caps,
    clip: &str,
    sums_name: &str,
    codec: Codec,
    ten_bit: bool,
    full_chroma: bool,
) {
    let can = match (codec, ten_bit, full_chroma) {
        (Codec::H264, _, _) => able.h264,
        (Codec::H265, false, false) => able.hevc,
        (Codec::H265, true, false) => able.hevc_10,
        (Codec::H265, false, true) => able.hevc_444,
        (Codec::H265, true, true) => able.hevc_444_10,
    };
    if !can {
        println!("{clip}: the device does not decode this combination, skipped");
        return;
    }
    // **The device has a floor on the coded size**, which a small fixture
    // may sit under; that is a compatibility fact this test reports, not a
    // defect, and the clip is skipped rather than failed.
    let (floor_w, floor_h) = floor(cuvid, codec);
    let (w, h) = clip_size(clip, codec);
    if w < floor_w || h < floor_h {
        println!("{clip}: {w}x{h} is under the device's floor of {floor_w}x{floor_h}, skipped");
        return;
    }
    let theirs = common::sums(sums_name);
    let mut expected: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &theirs {
        *expected.entry((s.y, s.uv)).or_default() += 1;
    }
    // Both modes: into the backend's own surfaces where the driver can,
    // and mapping the decoder's, which is all an older driver does.
    for mapped in [false, true] {
        let mut backend = Backend::new(cuda, cuvid, (4096, 4096), MAX_UNIT);
        if mapped {
            backend.force_mapped();
        }
        let (ours, times, own) = decode(&mut backend, clip, codec, ten_bit);
        if mapped {
            assert!(
                !own,
                "{clip}: a decoder made to map decoded into own surfaces"
            );
        } else if cuvid.asynchronous() && !full_chroma {
            assert!(
                own,
                "{clip}: the driver decodes into own surfaces and this decoder did not"
            );
        }
        let mode = if own { "own surfaces" } else { "mapped" };
        let mut got: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for s in &ours {
            *got.entry(*s).or_default() += 1;
        }
        let wrong = ours.iter().filter(|s| !expected.contains_key(s)).count();
        if wrong > 0 {
            // Which plane is off, for the first few: the luma sums and the
            // chroma sums are compared apart, which is what localises a
            // read-back fault against a decode fault.
            for (n, s) in ours.iter().take(4).enumerate() {
                let y_ok = theirs.iter().any(|t| t.y == s.0);
                let c_ok = theirs.iter().any(|t| t.uv == s.1);
                println!(
                    "{clip} ({mode}): picture {n}: luma {} chroma {}",
                    if y_ok { "matches" } else { "differs" },
                    if c_ok { "matches" } else { "differs" }
                );
            }
        }
        let decode_max = times.iter().map(|t| t.0).max().unwrap_or(0);
        let readback_max = times.iter().map(|t| t.1).max().unwrap_or(0);
        let decode_mean =
            times.iter().map(|t| u64::from(t.0)).sum::<u64>() / times.len().max(1) as u64;
        let readback_mean =
            times.iter().map(|t| u64::from(t.1)).sum::<u64>() / times.len().max(1) as u64;
        println!(
            "{clip} ({mode}): {} of {} pictures, {wrong} wrong; wait mean {decode_mean} us max {decode_max}; copy mean {readback_mean} us max {readback_max}",
            ours.len(),
            theirs.len()
        );
        assert_eq!(ours.len(), theirs.len(), "{clip} ({mode}): picture count");
        assert_eq!(
            got, expected,
            "{clip} ({mode}): the pictures differ from the reference decoder's"
        );
    }
}

/// The smallest coded picture the device decodes for a codec.
fn floor(cuvid: &Cuvid, codec: Codec) -> (u32, u32) {
    // SAFETY: plain data.
    let mut query: CUVIDDECODECAPS = unsafe { core::mem::zeroed() };
    query.eCodecType = match codec {
        Codec::H264 => cudaVideoCodec_H264,
        Codec::H265 => cudaVideoCodec_HEVC,
    };
    query.eChromaFormat = cudaVideoChromaFormat_420;
    cuvid.caps(&mut query).expect("caps");
    (u32::from(query.nMinWidth), u32::from(query.nMinHeight))
}

/// A clip's coded size, from its first unit's parameter set.
fn clip_size(clip: &str, codec: Codec) -> (u32, u32) {
    let first = &common::units(clip)[0];
    match codec {
        Codec::H264 => {
            let mut stream = lowlat_decode::h264::Stream::new();
            stream.read(first).expect("read");
            let sps = stream.job().expect("job").sps;
            (sps.coded_width(), sps.coded_height())
        }
        Codec::H265 => {
            let mut stream = lowlat_decode::hevc::Stream::new();
            stream.read(first).expect("read");
            let sps = stream.job().expect("job").sps;
            (sps.width, sps.height)
        }
    }
}

/// **The probe builds a real decoder per combination.** On the device this
/// was written against every row is true; a device with fewer says so here
/// and the declaration follows.
#[test]
#[ignore = "requires the vendor's decode interface"]
fn the_probe_reports_what_the_device_builds() {
    let (_cuda, _context, cuvid) = open();
    let able = caps(&cuvid);
    println!("{able:?}");
    assert!(able.h264 && able.hevc, "the device decodes neither codec");
}

#[test]
#[ignore = "requires the vendor's decode interface"]
fn the_synthetic_clips_decode_to_the_reference_pictures() {
    let (cuda, _context, cuvid) = open();
    let able = caps(&cuvid);
    for (clip, codec, ten_bit) in [
        ("synthetic-720p-h264", Codec::H264, false),
        ("synthetic-720p-hevc", Codec::H265, false),
        ("synthetic-720p-hevc10", Codec::H265, true),
    ] {
        check(
            &cuda,
            &cuvid,
            &able,
            &format!("{clip}.bin"),
            &format!("{clip}.sums"),
            codec,
            ten_bit,
            false,
        );
    }
}

/// The device route: the picture copied into an exportable allocation
/// instead of read back, then read back from there by the test. The same
/// pictures, bit for bit, and the copy's cost beside the read-back's.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the vendor's decode interface"]
fn the_device_route_produces_the_same_pictures() {
    let (cuda, _context, cuvid) = open();
    let device = cuda.any_device().expect("a device");
    let able = caps(&cuvid);
    // One allocation at the harness's odd pitch for the largest fixture,
    // three planes deep.
    let pitch = 1280 * 2 + 64;
    let slot = cuda
        .alloc_exportable(&device, pitch * 720 * 3)
        .expect("an exportable allocation");
    for (clip, sums, codec, ten_bit, full_chroma) in [
        (
            "synthetic-720p-h264.bin",
            "synthetic-720p-h264.sums",
            Codec::H264,
            false,
            false,
        ),
        (
            "synthetic-720p-hevc10.bin",
            "synthetic-720p-hevc10.sums",
            Codec::H265,
            true,
            false,
        ),
        (
            "fixtures/hevc-nvenc-444-10.bin",
            "fixtures/hevc-nvenc-444-10.sums",
            Codec::H265,
            true,
            true,
        ),
    ] {
        let can = match (ten_bit, full_chroma) {
            _ if codec == Codec::H264 => able.h264,
            (false, false) => able.hevc,
            (true, false) => able.hevc_10,
            (false, true) => able.hevc_444,
            (true, true) => able.hevc_444_10,
        };
        if !can {
            println!("{clip}: not decoded here, skipped");
            continue;
        }
        let mut backend = Backend::new(&cuda, &cuvid, (4096, 4096), MAX_UNIT);
        backend.build(&header(codec, ten_bit)).expect("build");
        let (ours, times) = common::decode_clip_with(
            &mut backend,
            clip,
            |b| b.drain(),
            |b| (b.decode_us, b.readback_us),
            |b, planes| {
                let base = slot.ptr();
                let plane = u64::try_from(pitch * 720).unwrap();
                let target = lowlat_decode::nvdec::DevicePlanes {
                    y: base,
                    y_pitch: pitch,
                    uv: base + plane,
                    uv_pitch: pitch,
                    v: base + 2 * plane,
                    v_pitch: pitch,
                };
                let picture = b.take_to_device(&target)?;
                if let Some(p) = picture {
                    let rows = p.height as usize;
                    let row_bytes = p.width as usize * p.format.sample();
                    let chroma_rows = p.format.chroma_rows(rows);
                    // SAFETY: the allocation covers three planes of
                    // `pitch` x 720 and the pictures are no larger.
                    unsafe {
                        cuda.read_rows(base, pitch, planes.y, pitch, row_bytes, rows)
                            .expect("luma");
                        cuda.read_rows(
                            base + plane,
                            pitch,
                            planes.uv,
                            pitch,
                            row_bytes,
                            chroma_rows,
                        )
                        .expect("chroma");
                        if p.format.full_chroma() {
                            cuda.read_rows(
                                base + 2 * plane,
                                pitch,
                                planes.v,
                                pitch,
                                row_bytes,
                                rows,
                            )
                            .expect("chroma");
                        }
                    }
                }
                Ok(picture)
            },
        );
        backend.destroy();
        let theirs = common::sums(sums);
        let mut expected: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for s in &theirs {
            *expected.entry((s.y, s.uv)).or_default() += 1;
        }
        let mut got: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for s in &ours {
            *got.entry(*s).or_default() += 1;
        }
        let copy_mean =
            times.iter().map(|t| u64::from(t.1)).sum::<u64>() / times.len().max(1) as u64;
        let copy_max = times.iter().map(|t| t.1).max().unwrap_or(0);
        println!(
            "{clip}: {} pictures by the device route; device copy mean {copy_mean} us max {copy_max}",
            ours.len()
        );
        assert_eq!(got, expected, "{clip}: the device route differs");
    }
}

/// One clip through textures of a device of the library's own on the
/// adapter `luid`, read back on a second device once the fence has passed,
/// compared with the reference; `mapped` makes the decoder map its own
/// surfaces. Where it decoded from, or why it failed.
#[cfg(windows)]
fn check_textures(
    cuda: &Cuda,
    context: &Context,
    cuvid: &Cuvid,
    luid: lowlat_drivers::d3d11::Luid,
    clip: &str,
    sums: &str,
    (codec, ten_bit): (Codec, bool),
    mapped: bool,
) -> Result<&'static str, String> {
    use std::sync::Arc;

    use lowlat_decode::Format;
    use lowlat_decode::d3d11::plane_textures;
    use lowlat_drivers::cuda::Registered;
    use lowlat_drivers::d3d11::{D3d11, Event, SharedTexture};

    let d3d11 = D3d11::load().expect("the system's libraries");
    let device = Arc::new(d3d11.open(luid).expect("the textures' device"));
    let fence = Arc::new(device.fence(0).expect("a fence"));
    let mut reader = common::reader::Reader {
        device: d3d11.open(luid).expect("a second device"),
        opened: Vec::new(),
    };
    let event = Event::new().expect("an event");
    let mut backend = Backend::new(cuda, cuvid, (4096, 4096), MAX_UNIT);
    if mapped {
        backend.force_mapped();
    }
    backend.attach_textures(Arc::clone(&device), Arc::clone(&fence));
    if !backend.exports_textures() {
        return Err("the runtime writes no textures".to_string());
    }
    backend
        .build(&header(codec, ten_bit))
        .map_err(|e| format!("build: {e:?}"))?;
    // The registrations before the textures, so each goes first.
    let mut textures: Option<(
        (u32, u32, Format),
        [Option<Registered>; 3],
        [Option<SharedTexture>; 3],
    )> = None;
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
            if textures.as_ref().is_none_or(|(l, _, _)| *l != layout) {
                textures = None;
                let made = plane_textures(format, width, height).map(|p| {
                    p.map(|(f, w, h)| device.shared_texture(f, w, h).expect("a shared plane"))
                });
                let registered = made.each_ref().map(|t| {
                    t.as_ref().map(|t| {
                        // SAFETY: a live texture of a device on the context's
                        // adapter, kept past the registration, which drops
                        // first.
                        unsafe { cuda.register_texture(context, t.texture().cast()) }
                            .expect("registered")
                    })
                });
                textures = Some((layout, registered, made));
                reader.opened.clear();
            }
            let (_, registered, made) = textures.as_ref().expect("made");
            let Some((picture, value)) =
                b.take_to_textures(registered.each_ref().map(Option::as_ref))?
            else {
                return Ok(None);
            };
            let submitted = std::time::Instant::now();
            fence.notify_at(value, &event).expect("a notification");
            assert!(
                event.wait(std::time::Duration::from_secs(2)),
                "the fence never reached {value}"
            );
            let waited = submitted.elapsed().as_micros() as u32;
            timing.set((waited, b.readback_us));
            let sample = picture.format.sample();
            let w = picture.width as usize;
            let h = picture.height as usize;
            let handle = |p: usize| made[p].as_ref().expect("a plane").handle;
            let rows = picture.format.chroma_rows(h);
            if picture.format.full_chroma() {
                reader.read(handle(0), h, w * sample, planes.y, planes.y_pitch);
                reader.read(handle(1), rows, w * sample, planes.uv, planes.uv_pitch);
                reader.read(handle(2), rows, w * sample, planes.v, planes.v_pitch);
            } else {
                reader.read(handle(0), h, w * sample, planes.y, planes.y_pitch);
                let bytes = w.div_ceil(2) * 2 * sample;
                reader.read(handle(1), rows, bytes, planes.uv, planes.uv_pitch);
            }
            Ok(Some(picture))
        },
    );
    let own = backend.decodes_into_own_surfaces();
    backend.destroy();
    drop(textures);
    let theirs = common::sums(sums);
    let mut expected: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &theirs {
        *expected.entry((s.y, s.uv)).or_default() += 1;
    }
    let mut got: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &ours {
        *got.entry(*s).or_default() += 1;
    }
    let mode = if own { "own surfaces" } else { "mapped" };
    let wrong = ours.iter().filter(|s| !expected.contains_key(s)).count();
    let mut waits: Vec<u32> = times.iter().map(|t| t.0).collect();
    let mut submits: Vec<u32> = times.iter().map(|t| t.1).collect();
    waits.sort_unstable();
    submits.sort_unstable();
    let at = |v: &[u32], q: f64| v.get(((v.len().max(1) - 1) as f64 * q) as usize).copied();
    println!(
        "  {clip} ({mode}): {} of {} pictures, {wrong} wrong; to the fence p50 {:?} us p95 {:?}; submit p50 {:?} us p95 {:?} max {:?}",
        ours.len(),
        theirs.len(),
        at(&waits, 0.5),
        at(&waits, 0.95),
        at(&submits, 0.5),
        at(&submits, 0.95),
        at(&submits, 1.0),
    );
    if ours.len() != theirs.len() {
        return Err(format!("{} pictures of {}", ours.len(), theirs.len()));
    }
    if got != expected {
        return Err(format!("{wrong} pictures differ from the reference"));
    }
    Ok(mode)
}

/// **Every clip through the textures, both modes** (Windows): each picture
/// copied into textures of a device of the library's own on the vendor's
/// adapter, then opened by handle on a second device once the fence has
/// passed and read back, is the reference picture bit for bit -- out of the
/// backend's own surfaces where the driver decodes into them, and out of the
/// decoder's mapped picture, which every clip is made to take once. The
/// wait printed runs from the take to the fence passing; the submit is what
/// the take cost this thread past any wait for the decode.
#[cfg(windows)]
#[test]
#[ignore = "requires the vendor's decode interface"]
fn every_clip_decodes_through_the_textures() {
    let (cuda, context, cuvid) = open();
    let able = caps(&cuvid);
    let device = cuda.any_device().expect("a device");
    let value = cuda.luid(&device).expect("the device's adapter");
    let luid = lowlat_drivers::d3d11::D3d11::load()
        .expect("the system's libraries")
        .adapters()
        .expect("the walk")
        .into_iter()
        .map(|a| a.luid)
        .find(|l| l.value() == value)
        .expect("an adapter that is the device");
    let mut clips: Vec<(String, String, Codec, bool, bool)> = [
        ("synthetic-720p-h264", Codec::H264, false),
        ("synthetic-720p-hevc", Codec::H265, false),
        ("synthetic-720p-hevc10", Codec::H265, true),
    ]
    .into_iter()
    .map(|(n, c, t)| (format!("{n}.bin"), format!("{n}.sums"), c, t, false))
    .collect();
    for (clip, sums) in common::fixtures("h264") {
        clips.push((clip, sums, Codec::H264, false, false));
    }
    for (clip, sums) in common::fixtures("hevc") {
        let ten_bit = clip.contains("10");
        let full_chroma = clip.contains("444");
        clips.push((clip, sums, Codec::H265, ten_bit, full_chroma));
    }
    let mut failures = Vec::new();
    for mapped in [false, true] {
        println!(
            "{}",
            if mapped {
                "mapped"
            } else {
                "as the driver can"
            }
        );
        for (clip, sums, codec, ten_bit, full_chroma) in &clips {
            let can = match (codec, ten_bit, full_chroma) {
                (Codec::H264, _, _) => able.h264,
                (Codec::H265, false, false) => able.hevc,
                (Codec::H265, true, false) => able.hevc_10,
                (Codec::H265, false, true) => able.hevc_444,
                (Codec::H265, true, true) => able.hevc_444_10,
            };
            let (floor_w, floor_h) = floor(&cuvid, *codec);
            let (w, h) = clip_size(clip, *codec);
            if !can || w < floor_w || h < floor_h {
                continue;
            }
            match check_textures(
                &cuda,
                &context,
                &cuvid,
                luid,
                clip,
                sums,
                (*codec, *ten_bit),
                mapped,
            ) {
                Ok(mode) => {
                    let own_expected = !mapped && cuvid.asynchronous() && !full_chroma;
                    if (mode == "own surfaces") != own_expected {
                        failures.push(format!("{clip}: decoded {mode}"));
                    }
                }
                Err(e) => {
                    println!("  {clip}: FAILED: {e}");
                    failures.push(format!("{clip} mapped={mapped}: {e}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
#[ignore = "requires the vendor's decode interface"]
fn every_h264_fixture_decodes_to_the_reference_pictures() {
    let (cuda, _context, cuvid) = open();
    let able = caps(&cuvid);
    for (clip, sums) in common::fixtures("h264") {
        check(
            &cuda,
            &cuvid,
            &able,
            &clip,
            &sums,
            Codec::H264,
            false,
            false,
        );
    }
}

#[test]
#[ignore = "requires the vendor's decode interface"]
fn every_hevc_fixture_decodes_to_the_reference_pictures() {
    let (cuda, _context, cuvid) = open();
    let able = caps(&cuvid);
    for (clip, sums) in common::fixtures("hevc") {
        let ten_bit = clip.contains("10");
        let full_chroma = clip.contains("444");
        check(
            &cuda,
            &cuvid,
            &able,
            &clip,
            &sums,
            Codec::H265,
            ten_bit,
            full_chroma,
        );
    }
}
