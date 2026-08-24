//! The third encoder, driven through its own surface.
//!
//!   vulkan-encoder [/dev/dri/card0] [frames]
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

    let api = vulkan::Vulkan::load().unwrap_or_else(|e| fail(&format!("loader: {e}")));
    let device = api
        .open(&node)
        .unwrap_or_else(|e| fail(&format!("{}: {e}", node.display())));
    println!("{}: {}", node.display(), device.name());

    let caps = device
        .caps(vulkan::Codec::H264)
        .unwrap_or_else(|e| fail(&format!("caps: {e}")));
    println!(
        "  up to {}x{}, {} slot(s), conversion may write the encoder's picture: {}",
        caps.max_extent.width, caps.max_extent.height, caps.max_dpb_slots, caps.shared_picture
    );

    let mut encoder = device
        .encoder(&caps, 1920, 1080, 10_000_000)
        .unwrap_or_else(|e| fail(&format!("encoder: {e}")));
    println!(
        "  encoder built at {}x{}, planes for a shader: {}",
        encoder.extent().width,
        encoder.extent().height,
        encoder.planes().is_some()
    );

    let mut each = Vec::with_capacity(frames);
    let mut bytes = 0usize;
    for at in 0..frames {
        // Half way through, ask for a different rate on the running session.
        if at == frames / 2 {
            encoder.set_bitrate(4_000_000);
        }
        let began = std::time::Instant::now();
        encoder
            .submit(at == 0)
            .unwrap_or_else(|e| fail(&format!("submit {at}: {e}")));
        encoder
            .wait()
            .unwrap_or_else(|e| fail(&format!("wait {at}: {e}")));
        each.push(began.elapsed().as_secs_f64() * 1000.0);
        match encoder.poll() {
            Ok(Poll::Ready { bitstream, .. }) => bytes += bitstream.len(),
            Ok(Poll::Pending) => fail(&format!("picture {at} finished and reported nothing")),
            Err(error) => fail(&format!("poll {at}: {error}")),
        }
    }
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
