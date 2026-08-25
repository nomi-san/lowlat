//! Does the encoder spend the bitrate it was given?
//!
//!   sudo rate-probe [/dev/dri/card0] [frames] [mbit] [fps]
//!   LOWLAT_CODEC=h265 sudo -E rate-probe ...
//!
//! **A rate is a budget per second, and the only way to check it is content
//! that costs something.** Every other probe here encodes a still picture,
//! which costs almost nothing whatever the rate control is told, so a target
//! that is being ignored looks exactly like one that is being met. This one
//! writes a different picture every frame and compares what came out against
//! what was asked for.
//!
//! **Run it long enough for the controller to settle.** The rate control on
//! this device takes seconds to converge, and a short run measures the ramp.

use std::path::PathBuf;

use lowlat_capture::convert::Converter;
use lowlat_capture::vulkan::Device as Capture;
use lowlat_encode::vulkan;

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(2);
}

/// A picture that costs something to code and differs every frame.
///
/// Detail that moves, rather than noise: pure noise is incompressible and
/// pins any encoder at its ceiling, which would pass this test for the wrong
/// reason. This is structure a codec can predict, displaced each frame.
fn draw(pixels: &mut [u8], width: u32, height: u32, at: u32) {
    // **Low frequency that moves**, which is what ordinary video is: smooth
    // ramps and a few large shapes panning across. High-frequency detail
    // everywhere is the worst case for any encoder and it pins one at its
    // coarsest quantiser, where a bitrate target cannot be met however
    // faithfully it is programmed -- so a probe drawing that measures the
    // content rather than the rate control.
    let phase = at.wrapping_mul(3);
    let bar = (phase / 2) % width.max(1);
    for y in 0..height {
        for x in 0..width {
            let Ok(i) = usize::try_from((y * width + x) * 4) else {
                return;
            };
            let u = x.wrapping_add(phase);
            let dx = x.abs_diff(bar);
            let blob = 200_u32.saturating_sub(dx);
            if let Some(slot) = pixels.get_mut(i..i + 4) {
                slot[0] = u8::try_from((u / 8) & 0xff).unwrap_or(0);
                slot[1] = u8::try_from((y / 8) & 0xff).unwrap_or(0);
                slot[2] = u8::try_from(blob & 0xff).unwrap_or(0);
                slot[3] = 255;
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let node = args
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);
    let frames: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(600);
    let mbit: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(3.0);
    let fps: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(120);

    let capture = Capture::for_display_and_encode(&node)
        .unwrap_or_else(|e| fail(&format!("{}: {e}", node.display())));
    let (queue, family) = capture
        .encode_queue()
        .unwrap_or_else(|| fail("that device opened without an encode queue"));
    let device = vulkan::Device::shared(capture.clone(), queue, family)
        .unwrap_or_else(|e| fail(&format!("encoder device: {e}")));
    let codec = match std::env::var("LOWLAT_CODEC").as_deref() {
        Ok("h265" | "hevc") => vulkan::Codec::H265,
        _ => vulkan::Codec::H264,
    };
    let caps = device
        .caps(codec)
        .unwrap_or_else(|e| fail(&format!("caps: {e}")));
    println!(
        "  rate control modes the device offers: {:?}",
        caps.rate_control
    );
    if !caps.shared_picture {
        fail("this device needs a copy between conversion and encode; not this probe's path");
    }

    let (width, height) = (1920u32, 1080u32);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a configured bitrate in megabits, well inside u32 as bits per second"
    )]
    let bitrate_bps = (mbit * 1_000_000.0) as u32;
    let mut encoder = device
        .encoder(&caps, width, height, bitrate_bps, fps, 2)
        .unwrap_or_else(|e| fail(&format!("encoder: {e}")));
    println!(
        "{}: {} {codec:?} {width}x{height} asked for {mbit:.2} Mbit/s at {fps} fps over {frames} \
         frames",
        node.display(),
        capture.name()
    );

    let mut converter =
        Converter::new(&capture).unwrap_or_else(|e| fail(&format!("converter: {e}")));
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let mut bytes = 0u64;
    // The first second is the ramp; measured separately so a settled figure
    // is not an average with the ramp in it.
    let mut settled_bytes = 0u64;
    let mut settled_frames = 0u32;

    for at in 0..frames {
        draw(&mut pixels, width, height, at);
        let source = capture
            .upload_rgba(width, height, &pixels)
            .unwrap_or_else(|e| fail(&format!("upload {at}: {e}")));
        let slot = (at % 2) as usize;
        let (Some(planes), Some(image)) = (encoder.planes(slot), encoder.source(slot)) else {
            fail("the encoder lends no planes");
        };
        let target = lowlat_capture::convert::TargetRef {
            luma_image: image,
            chroma_image: image,
            planes,
            final_layout: ash::vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
        };
        if let Err(e) = converter.run(&capture, &source, &target, false) {
            fail(&format!("convert {at}: {e}"));
        }
        if let Err(e) = encoder
            .submit_written(slot, at == 0)
            .and_then(|()| encoder.wait())
        {
            fail(&format!("encode {at}: {e}"));
        }
        match encoder.poll() {
            Ok(lowlat_encode::Poll::Ready { bitstream, .. }) => {
                bytes += bitstream.len() as u64;
                if at >= fps {
                    settled_bytes += bitstream.len() as u64;
                    settled_frames += 1;
                }
            }
            Ok(lowlat_encode::Poll::Pending) => fail(&format!("picture {at} reported nothing")),
            Err(e) => fail(&format!("poll {at}: {e}")),
        }
        capture.release(source);
    }
    converter.destroy(&capture);

    let seconds = f64::from(frames) / f64::from(fps.max(1));
    #[allow(
        clippy::cast_precision_loss,
        reason = "a byte count from one short run"
    )]
    let achieved = (bytes * 8) as f64 / seconds / 1_000_000.0;
    let settled_seconds = f64::from(settled_frames) / f64::from(fps.max(1));
    #[allow(
        clippy::cast_precision_loss,
        reason = "a byte count from one short run"
    )]
    let settled = if settled_seconds > 0.0 {
        (settled_bytes * 8) as f64 / settled_seconds / 1_000_000.0
    } else {
        0.0
    };
    println!("  asked {mbit:.2} Mbit/s, produced {achieved:.2} over the whole run");
    println!("  settled (after the first second): {settled:.2} Mbit/s");
    println!("  ratio produced/asked: {:.2}x", settled / mbit.max(0.001));
}
