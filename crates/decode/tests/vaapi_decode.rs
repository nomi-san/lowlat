//! The device decodes every committed clip to the pictures the reference
//! decoder produced. Needs a render node with the open-stack driver, so it
//! is off by default: `cargo test -p lowlat-decode --test vaapi_decode --
//! --ignored`, with `LOWLAT_VAAPI_NODE` naming the node (`renderD128`).

// The open stack exists on Linux alone.
#![cfg(target_os = "linux")]
#![allow(clippy::type_complexity)]

mod common;

use std::collections::BTreeMap;
use std::ffi::CString;

use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::vaapi::{Backend, caps};
use lowlat_decode::{Decoder, Planes};
use lowlat_drivers::va::Vaapi;

fn node() -> CString {
    let named = std::env::var("LOWLAT_VAAPI_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    CString::new(named).unwrap()
}

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

/// Decode a clip and return `(y, chroma)` checksums of every picture out,
/// in output order, plus the timings.
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

fn check(clip: &str, sums_name: &str, codec: Codec, ten_bit: bool) {
    let va = Vaapi::load().expect("runtime");
    let display = va.open(&node()).expect("render node");
    let caps = caps(&display).expect("caps");
    let full_chroma = clip.contains("444");
    let able = match (codec, ten_bit, full_chroma) {
        (Codec::H264, _, _) => caps.h264,
        (Codec::H265, false, false) => caps.hevc,
        (Codec::H265, true, false) => caps.hevc_10,
        (Codec::H265, false, true) => caps.hevc_444,
        (Codec::H265, true, true) => caps.hevc_444_10,
    };
    if !able {
        println!("{clip}: the device does not decode this profile, skipped");
        return;
    }
    let mut backend = Backend::new(&display, (4096, 4096));
    let (ours, times) = decode(&mut backend, clip, codec, ten_bit);
    let theirs = common::sums(sums_name);
    // Output order is a presentation matter (a stream that says nothing
    // about reordering may leave a picture late); the pictures themselves
    // must all be there and all be right.
    let mut expected: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &theirs {
        *expected.entry((s.y, s.uv)).or_default() += 1;
    }
    let mut got: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &ours {
        *got.entry(*s).or_default() += 1;
    }
    let wrong = ours.iter().filter(|s| !expected.contains_key(s)).count();
    let decode_max = times.iter().map(|t| t.0).max().unwrap_or(0);
    let readback_max = times.iter().map(|t| t.1).max().unwrap_or(0);
    let decode_mean = times.iter().map(|t| u64::from(t.0)).sum::<u64>() / times.len().max(1) as u64;
    let readback_mean =
        times.iter().map(|t| u64::from(t.1)).sum::<u64>() / times.len().max(1) as u64;
    println!(
        "{clip}: {} of {} pictures, {wrong} wrong; decode mean {decode_mean} us max {decode_max}; readback mean {readback_mean} us max {readback_max}",
        ours.len(),
        theirs.len()
    );
    assert_eq!(ours.len(), theirs.len(), "{clip}: picture count");
    assert_eq!(
        got, expected,
        "{clip}: the pictures differ from the reference decoder's"
    );
}

#[test]
#[ignore = "requires the open-stack driver"]
fn the_synthetic_clips_decode_to_the_reference_pictures() {
    check(
        "synthetic-720p-h264.bin",
        "synthetic-720p-h264.sums",
        Codec::H264,
        false,
    );
    check(
        "synthetic-720p-hevc.bin",
        "synthetic-720p-hevc.sums",
        Codec::H265,
        false,
    );
    check(
        "synthetic-720p-hevc10.bin",
        "synthetic-720p-hevc10.sums",
        Codec::H265,
        true,
    );
}

#[test]
#[ignore = "requires the open-stack driver"]
fn every_h264_fixture_decodes_to_the_reference_pictures() {
    for (clip, sums) in common::fixtures("h264") {
        check(&clip, &sums, Codec::H264, false);
    }
}

#[test]
#[ignore = "requires the open-stack driver"]
fn every_hevc_fixture_decodes_to_the_reference_pictures() {
    for (clip, sums) in common::fixtures("hevc") {
        if clip.contains("444") {
            // Full chroma has its own row below.
            continue;
        }
        let ten_bit = clip.contains("main10");
        check(&clip, &sums, Codec::H265, ten_bit);
    }
}

/// Decode a clip with every picture lent, held four deep -- the most a queue
/// holds at once -- and each lent surface read back twice: when it is lent,
/// and when its holder lets it go after four more pictures were decoded. The
/// two readings agree, so nothing was decoded into a held surface; the
/// pictures are the reference decoder's; a surface keeps its export's number
/// for as long as its context lives, and two surfaces never share one.
fn check_lent(clip: &str, sums_name: &str, codec: Codec, ten_bit: bool) -> usize {
    use std::collections::VecDeque;
    let va = Vaapi::load().expect("runtime");
    let display = va.open(&node()).expect("render node");
    if !lowlat_decode::vaapi::exports_surfaces(&display) {
        println!("{clip}: the display lends no surface, skipped");
        return 0;
    }
    let caps = caps(&display).expect("caps");
    let able = match (codec, ten_bit) {
        (Codec::H264, _) => caps.h264,
        (Codec::H265, false) => caps.hevc,
        (Codec::H265, true) => caps.hevc_10,
    };
    if !able {
        println!("{clip}: the device does not decode this profile, skipped");
        return 0;
    }
    let mut backend = Backend::new(&display, (4096, 4096));
    backend.build(&header(codec, ten_bit)).expect("build");
    let pitch = 1280 * 2 + 64;
    let (mut y, mut u, mut v) = (
        vec![0u8; pitch * 720],
        vec![0u8; pitch * 720],
        vec![0u8; pitch * 720],
    );
    let mut read = |backend: &mut Backend<'_>, lease, picture: &lowlat_decode::Picture| {
        let mut planes = Planes {
            y: &mut y,
            y_pitch: pitch,
            uv: &mut u,
            uv_pitch: pitch,
            v: &mut v,
            v_pitch: pitch,
        };
        backend
            .read_lent(lease, &mut planes)
            .unwrap_or_else(|e| panic!("{clip}: read lent: {e:?}"));
        common::picture_sums(picture, &y, &u, &v, pitch)
    };
    let mut held = VecDeque::new();
    let mut ours = Vec::new();
    let mut numbers = 0u32;
    let mut by_surface: BTreeMap<u32, u32> = BTreeMap::new();
    let all = common::units(clip);
    for (n, unit) in all.iter().enumerate() {
        backend
            .feed(unit)
            .unwrap_or_else(|e| panic!("{clip}: unit {n}: {e:?}"));
        if n + 1 == all.len() {
            backend.drain();
        }
        loop {
            let taken = backend
                .take_lent(|| {
                    numbers += 1;
                    numbers
                })
                .unwrap_or_else(|e| panic!("{clip}: take lent: {e:?}"));
            let Some((picture, lent)) = taken else {
                break;
            };
            assert!(lent.size > 0 && lent.chroma_offset > 0, "{clip}: {lent:?}");
            let row = picture.width * u32::try_from(picture.format.sample()).unwrap();
            assert!(
                lent.pitches[0] >= row && lent.pitches[1] >= row,
                "{clip}: pitches {:?} for rows of {row}",
                lent.pitches
            );
            assert!(
                u64::from(lent.chroma_offset)
                    >= u64::from(lent.pitches[0]) * u64::from(picture.height),
                "{clip}: chroma at {} inside the luma",
                lent.chroma_offset
            );
            let number = *by_surface
                .entry(lent.lease.buffer)
                .or_insert(lent.allocation);
            assert_eq!(number, lent.allocation, "{clip}: a surface's number moved");
            let sums = read(&mut backend, lent.lease, &picture);
            ours.push(sums);
            held.push_back((lent, picture, sums));
            if held.len() > 4 {
                let (old, picture, sums) = held.pop_front().unwrap();
                assert_eq!(
                    read(&mut backend, old.lease, &picture),
                    sums,
                    "{clip}: a held surface was written"
                );
                backend.lease_ended(old.lease);
            }
        }
    }
    for (old, picture, sums) in held.drain(..) {
        assert_eq!(
            read(&mut backend, old.lease, &picture),
            sums,
            "{clip}: a held surface was written"
        );
        backend.lease_ended(old.lease);
    }
    let numbers_used: std::collections::BTreeSet<u32> = by_surface.values().copied().collect();
    assert_eq!(
        numbers_used.len(),
        by_surface.len(),
        "{clip}: two surfaces shared a number"
    );
    let spares = backend.spares();
    assert!(
        spares <= lowlat_decode::vaapi::SPARES,
        "{clip}: {spares} spares"
    );
    backend.destroy();
    let theirs = common::sums(sums_name);
    let mut expected: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &theirs {
        *expected.entry((s.y, s.uv)).or_default() += 1;
    }
    let mut got: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for s in &ours {
        *got.entry(*s).or_default() += 1;
    }
    println!(
        "{clip}: {} of {} pictures lent, {} surfaces, {spares} spares",
        ours.len(),
        theirs.len(),
        by_surface.len()
    );
    assert_eq!(ours.len(), theirs.len(), "{clip}: picture count");
    assert_eq!(
        got, expected,
        "{clip}: the lent pictures differ from the reference decoder's"
    );
    spares
}

/// **A lent picture is never decoded into while held**: every half-chroma
/// fixture of both codecs, every picture lent and four held.
#[test]
#[ignore = "requires the open-stack driver"]
fn every_fixture_lent_and_held_is_never_written() {
    let mut spares = 0;
    for (clip, sums) in common::fixtures("h264") {
        spares = spares.max(check_lent(&clip, &sums, Codec::H264, false));
    }
    for (clip, sums) in common::fixtures("hevc") {
        if clip.contains("444") {
            continue;
        }
        let ten_bit = clip.contains("main10");
        spares = spares.max(check_lent(&clip, &sums, Codec::H265, ten_bit));
    }
    for (clip, sums, codec, ten_bit) in [
        (
            "synthetic-720p-h264.bin",
            "synthetic-720p-h264.sums",
            Codec::H264,
            false,
        ),
        (
            "synthetic-720p-hevc.bin",
            "synthetic-720p-hevc.sums",
            Codec::H265,
            false,
        ),
        (
            "synthetic-720p-hevc10.bin",
            "synthetic-720p-hevc10.sums",
            Codec::H265,
            true,
        ),
    ] {
        spares = spares.max(check_lent(clip, sums, codec, ten_bit));
    }
    println!("most spares any clip took: {spares}");
}

/// **Full chroma is decoded where the driver hands out a layout the
/// backend reads, and refused everywhere else -- never decoded wrongly.**
/// Where the capability says so, every full-chroma clip comes back bit for
/// bit through the range-extension structures and the unpacking of the
/// driver's surface layout; where it does not, the base parameters would
/// take the stream without a word and decode it to the wrong picture, so
/// the first unit is refused as fatal instead.
#[test]
#[ignore = "requires the open-stack driver"]
fn the_full_chroma_fixtures_decode_where_the_display_takes_them() {
    let va = Vaapi::load().expect("runtime");
    let display = va.open(&node()).expect("render node");
    let caps = caps(&display).expect("caps");
    println!(
        "full chroma: eight-bit {} ten-bit {}",
        caps.hevc_444, caps.hevc_444_10
    );
    for (clip, sums) in common::fixtures("hevc") {
        if !clip.contains("444") {
            continue;
        }
        let ten_bit = clip.contains("10");
        let able = if ten_bit {
            caps.hevc_444_10
        } else {
            caps.hevc_444
        };
        if able {
            check(&clip, &sums, Codec::H265, ten_bit);
            continue;
        }
        // A device with no HEVC at all refuses the stream at the build, which
        // is the refusal; there is no first unit to refuse after it.
        if !caps.hevc {
            println!("{clip}: the device decodes no HEVC, skipped");
            continue;
        }
        let mut backend = Backend::new(&display, (4096, 4096));
        backend.build(&header(Codec::H265, ten_bit)).expect("build");
        let first = &common::units(&clip)[0];
        assert_eq!(
            backend.feed(first),
            Err(lowlat_decode::Fault::Fatal),
            "{clip}: the first unit was not refused as fatal"
        );
        backend.destroy();
    }
}

/// One plane copied a row at a time, as the backend copies it.
fn copy_rows(src: &[u8], src_pitch: usize, dst: &mut [u8], dst_pitch: usize, w: usize, h: usize) {
    for row in 0..h {
        dst[row * dst_pitch..row * dst_pitch + w]
            .copy_from_slice(&src[row * src_pitch..row * src_pitch + w]);
    }
}

/// One plane copied with streaming loads, which read a whole line at a time
/// from memory the cache does not cover; the unaligned tail of a row goes
/// the plain way. `store` says whether the writes bypass the cache too.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn copy_rows_streaming(
    src: &[u8],
    src_pitch: usize,
    dst: &mut [u8],
    dst_pitch: usize,
    w: usize,
    h: usize,
    stream_store: bool,
) {
    use core::arch::x86_64::{
        __m256i, _mm256_storeu_si256, _mm256_stream_load_si256, _mm256_stream_si256,
    };
    for row in 0..h {
        let from = &src[row * src_pitch..row * src_pitch + w];
        let to = &mut dst[row * dst_pitch..row * dst_pitch + w];
        let mut at = 0;
        // Only an aligned source can be stream-loaded.
        if from.as_ptr() as usize % 32 == 0 {
            while at + 32 <= w {
                // SAFETY: 32 bytes in bounds on both sides, the source aligned.
                unsafe {
                    let v = _mm256_stream_load_si256(from.as_ptr().add(at).cast::<__m256i>());
                    let out = to.as_mut_ptr().add(at).cast::<__m256i>();
                    if stream_store && out as usize % 32 == 0 {
                        _mm256_stream_si256(out, v);
                    } else {
                        _mm256_storeu_si256(out, v);
                    }
                }
                at += 32;
            }
        }
        to[at..].copy_from_slice(&from[at..]);
    }
}

/// The `p` percentile, `p` in hundredths.
fn percentile(samples: &mut [u128], p: usize) -> u128 {
    samples.sort_unstable();
    let at = (samples.len().saturating_sub(1) * p).div_ceil(100);
    samples[at.min(samples.len().saturating_sub(1))]
}

/// **The read-back copy, three ways, timed on this device.** The row copy the
/// backend uses, a streaming-load copy, and a streaming-load copy with
/// streaming stores, over the same mapping of each decoded surface, so the
/// choice is a number from this driver's memory rather than what
/// write-combined memory usually does. `LOWLAT_PROBE_CLIP` names another
/// clip by path (a 1080p one, if there is one at hand).
#[test]
#[ignore = "requires the open-stack driver"]
fn read_back_copy_probe() {
    let clip =
        std::env::var("LOWLAT_PROBE_CLIP").unwrap_or_else(|_| "synthetic-720p-h264.bin".into());
    let va = Vaapi::load().expect("runtime");
    let display = va.open(&node()).expect("render node");
    let mut backend = Backend::new(&display, (4096, 4096));
    backend.build(&header(Codec::H264, false)).expect("build");
    let units = common::units(&clip);
    let pitch = 4096;
    let mut y = vec![0u8; pitch * 2160];
    let mut uv = vec![0u8; pitch * 1080];
    let mut dst_rows = vec![0u8; pitch * 3240];
    let mut dst_stream = vec![0u8; pitch * 3240];
    let (mut rows_us, mut stream_us, mut stream_store_us, mut taken_us, mut hook_us) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut size = (0usize, 0usize);
    let mut loops = 0;
    while rows_us.len() < 300 {
        for unit in &units {
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
                let Some(picture) = backend.take(&mut planes).expect("take") else {
                    break;
                };
                taken_us.push(u128::from(backend.readback_us));
                let started = std::time::Instant::now();
                backend.with_taken_mapped(|_, _| {}).expect("mapping");
                hook_us.push(started.elapsed().as_micros());
                let sample = picture.format.sample();
                let (dst_rows, dst_stream) = (&mut dst_rows, &mut dst_stream);
                backend
                    .with_taken_mapped(|image, mapped| {
                        let w = image.width as usize * sample;
                        let h = image.height as usize;
                        size = (w / sample, h);
                        // SAFETY: the driver mapped `data_size` bytes.
                        let src = unsafe {
                            core::slice::from_raw_parts(mapped, image.data_size as usize)
                        };
                        let (yo, uvo) = (image.offsets[0] as usize, image.offsets[1] as usize);
                        let (yp, uvp) = (image.pitches[0] as usize, image.pitches[1] as usize);
                        let (dy, duv) = dst_rows.split_at_mut(pitch * h);
                        let started = std::time::Instant::now();
                        copy_rows(&src[yo..], yp, dy, pitch, w, h);
                        copy_rows(&src[uvo..], uvp, duv, pitch, w, h / 2);
                        rows_us.push(started.elapsed().as_micros());
                        for stream_store in [false, true] {
                            let (sy, suv) = dst_stream.split_at_mut(pitch * h);
                            let started = std::time::Instant::now();
                            // SAFETY: this device has AVX2 (checked below).
                            unsafe {
                                copy_rows_streaming(&src[yo..], yp, sy, pitch, w, h, stream_store);
                                copy_rows_streaming(
                                    &src[uvo..],
                                    uvp,
                                    suv,
                                    pitch,
                                    w,
                                    h / 2,
                                    stream_store,
                                );
                            }
                            let took = started.elapsed().as_micros();
                            if stream_store {
                                stream_store_us.push(took);
                            } else {
                                stream_us.push(took);
                            }
                            for row in 0..h + h / 2 {
                                assert_eq!(
                                    &dst_rows[row * pitch..row * pitch + w],
                                    &dst_stream[row * pitch..row * pitch + w],
                                    "the streaming copy differs at row {row}"
                                );
                            }
                        }
                    })
                    .expect("mapping");
            }
        }
        loops += 1;
        assert!(loops < 20, "no pictures came out of {clip}");
    }
    assert!(is_x86_feature_detected!("avx2"));
    let n = rows_us.len();
    println!(
        "read-back copy probe: {clip} {}x{} NV12, {n} pictures; backend read-back p50 {} us p95 {}; \
         derive, map, unmap and destroy alone p50 {} us p95 {}; \
         row copy p50 {} us p95 {}; streaming loads p50 {} us p95 {}; streaming loads and stores p50 {} us p95 {}",
        size.0,
        size.1,
        percentile(&mut taken_us, 50),
        percentile(&mut taken_us, 95),
        percentile(&mut hook_us, 50),
        percentile(&mut hook_us, 95),
        percentile(&mut rows_us, 50),
        percentile(&mut rows_us, 95),
        percentile(&mut stream_us, 50),
        percentile(&mut stream_us, 95),
        percentile(&mut stream_store_us, 50),
        percentile(&mut stream_store_us, 95),
    );
}
