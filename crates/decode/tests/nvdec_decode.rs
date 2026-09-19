//! The vendor's device decodes every committed clip to the pictures the
//! reference decoder produced, full chroma included. Needs the vendor's
//! runtime and decode interface, so it is off by default:
//! `cargo test -p lowlat-decode --test nvdec_decode -- --ignored`.

#![allow(clippy::type_complexity, clippy::too_many_arguments)]

mod common;

use std::collections::BTreeMap;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::nvdec::{Backend, DevicePlanes, caps};
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

fn decode(
    backend: &mut Backend<'_>,
    clip: &str,
    codec: Codec,
    ten_bit: bool,
) -> (Vec<(u32, u32)>, Vec<(u32, u32)>) {
    backend.build(&header(codec, ten_bit)).expect("build");
    let out = common::decode_clip(
        backend,
        clip,
        |b| b.drain(),
        |b| (b.decode_us, b.readback_us),
    );
    backend.destroy();
    out
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
    let mut backend = Backend::new(cuda, cuvid, (4096, 4096), MAX_UNIT);
    let (ours, times) = decode(&mut backend, clip, codec, ten_bit);
    let theirs = common::sums(sums_name);
    let mut expected: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &theirs {
        *expected.entry((s.y, s.uv)).or_default() += 1;
    }
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
                "{clip}: picture {n}: luma {} chroma {}",
                if y_ok { "matches" } else { "differs" },
                if c_ok { "matches" } else { "differs" }
            );
        }
    }
    let decode_max = times.iter().map(|t| t.0).max().unwrap_or(0);
    let readback_max = times.iter().map(|t| t.1).max().unwrap_or(0);
    let decode_mean = times.iter().map(|t| u64::from(t.0)).sum::<u64>() / times.len().max(1) as u64;
    let readback_mean =
        times.iter().map(|t| u64::from(t.1)).sum::<u64>() / times.len().max(1) as u64;
    println!(
        "{clip}: {} of {} pictures, {wrong} wrong; map mean {decode_mean} us max {decode_max}; copy mean {readback_mean} us max {readback_max}",
        ours.len(),
        theirs.len()
    );
    assert_eq!(ours.len(), theirs.len(), "{clip}: picture count");
    assert_eq!(
        got, expected,
        "{clip}: the pictures differ from the reference decoder's"
    );
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
                let target = DevicePlanes {
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
