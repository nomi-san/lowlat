//! The third encoder, driven through its own surface.
//!
//!   vulkan-encoder [/dev/dri/card0] [frames] [width] [height]
//!   LOWLAT_CODEC=h265 vulkan-encoder [/dev/dri/card0] [frames]
//!
//! **A size that is not a whole number of coding blocks is the interesting
//! one**, because the sets declare the rounded size and a conformance window
//! carries the remainder. A decoder that reports the rounded size back has
//! been given a window that does not match its own set.
//!
//! **What the probe beside this proved by hand, through the module a stream
//! would use.** It says what the device will do, encodes, changes the bitrate
//! under the running session, and reports what a picture costs.

use std::path::PathBuf;

use lowlat_encode::{Poll, vulkan};

fn main() {
    let mut args = std::env::args().skip(1);
    let node = args
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);
    let frames: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(300);
    let width: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1920);
    let height: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1080);

    let api = vulkan::Vulkan::load().unwrap_or_else(|e| fail(&format!("loader: {e}")));
    let device = api
        .open(&node)
        .unwrap_or_else(|e| fail(&format!("{}: {e}", node.display())));
    println!("{}: {}", node.display(), device.name());

    let codec = match std::env::var("LOWLAT_CODEC").as_deref() {
        Ok("h265" | "hevc") => vulkan::Codec::H265,
        _ => vulkan::Codec::H264,
    };
    let caps = device
        .caps(codec)
        .unwrap_or_else(|e| fail(&format!("caps: {e}")));
    println!("  codec {codec:?}");
    println!(
        "  up to {}x{}, {} slot(s), conversion may write the encoder's picture: {}",
        caps.max_extent.width, caps.max_extent.height, caps.max_dpb_slots, caps.shared_picture
    );
    println!(
        "  pictures are accessed {}x{} at a time",
        caps.picture_granularity.width, caps.picture_granularity.height
    );

    let mut encoder = device
        .encoder(&caps, width, height, 10_000_000, 60, 2)
        .unwrap_or_else(|e| fail(&format!("encoder: {e}")));
    println!(
        "  encoder built at {}x{}, planes for a shader: {}",
        encoder.extent().width,
        encoder.extent().height,
        encoder.planes(0).is_some()
    );

    let mut each = Vec::with_capacity(frames);
    let mut bytes = 0usize;
    // A refresh access unit opens with the parameter sets on its own, as on
    // every other backend, so the stream needs no manual head.
    let mut stream = Vec::new();
    let mut keyframes = 0usize;
    // The refresh a caller forces mid-run, which is what the recovery path
    // sends: a predicted picture after it must still decode.
    let forced = frames / 3;
    for at in 0..frames {
        // Half way through, ask for a different rate on the running session.
        if at == frames / 2 {
            encoder.set_bitrate(4_000_000);
        }
        let began = std::time::Instant::now();
        // The slot alternates, which is what a stream does: the next picture
        // is written while the previous one encodes.
        encoder
            .submit(at % 2, at == 0 || at == forced)
            .unwrap_or_else(|e| fail(&format!("submit {at}: {e}")));
        encoder
            .wait()
            .unwrap_or_else(|e| fail(&format!("wait {at}: {e}")));
        each.push(began.elapsed().as_secs_f64() * 1000.0);
        match encoder.poll() {
            Ok(Poll::Ready {
                bitstream,
                keyframe,
            }) => {
                bytes += bitstream.len();
                stream.extend_from_slice(bitstream);
                if keyframe {
                    keyframes += 1;
                }
                if keyframe != (at == 0 || at == forced) {
                    fail(&format!("picture {at} reported keyframe={keyframe}"));
                }
            }
            Ok(Poll::Pending) => fail(&format!("picture {at} finished and reported nothing")),
            Err(error) => fail(&format!("poll {at}: {error}")),
        }
    }
    // Escaping makes a three-byte start code impossible inside a payload, so
    // counting unit types over the whole stream is exact.
    //
    // **The two codecs put the type in different bits of different bytes.**
    // One reads five bits of the byte after the start code; the other has a
    // two-byte header and reads six bits above the low one, so a reader
    // written for the first lands on a plausible wrong value rather than
    // failing.
    let units_of = |kinds: &[u8]| {
        let kinds = kinds.to_vec();
        stream
            .windows(4)
            .filter(|window| {
                window[..3] == [0, 0, 1] && {
                    let kind = match codec {
                        vulkan::Codec::H264 => window[3] & 0x1F,
                        vulkan::Codec::H265 => (window[3] >> 1) & 0x3F,
                    };
                    kinds.contains(&kind)
                }
            })
            .count()
    };
    // A refresh with no leading pictures, or one that allows them: which of
    // the two a device emits is the device's choice and both are refreshes.
    let (coded_refreshes, predicted) = match codec {
        vulkan::Codec::H264 => (units_of(&[5]), units_of(&[1])),
        vulkan::Codec::H265 => (units_of(&[19, 20]), units_of(&[0, 1])),
    };
    println!(
        "  {coded_refreshes} coded refreshes ({keyframes} reported), {predicted} predicted \
         pictures"
    );
    if coded_refreshes != 2 || predicted != frames - 2 {
        fail("the stream does not carry the picture kinds that were asked for");
    }
    let out = std::env::temp_dir().join(match codec {
        vulkan::Codec::H264 => "lowlat-vulkan.h264",
        vulkan::Codec::H265 => "lowlat-vulkan.h265",
    });
    std::fs::write(&out, &stream).unwrap_or_else(|e| fail(&format!("write: {e}")));
    println!("  wrote {}", out.display());
    each.sort_by(f64::total_cmp);
    let rank = |num: usize, den: usize| {
        each.get((each.len().saturating_sub(1)) * num / den)
            .copied()
            .unwrap_or(0.0)
    };
    println!(
        "  {frames} pictures, {bytes} bytes, p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms",
        rank(50, 100),
        rank(95, 100),
        rank(99, 100)
    );
    println!("  the bitrate changed under the session and nothing was rebuilt");
}

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(2);
}
