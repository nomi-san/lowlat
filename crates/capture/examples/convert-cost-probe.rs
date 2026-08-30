//! What one conversion costs, at a chosen size and depth.
//!
//!   convert-cost-probe [frames] [width] [height]
//!
//! **Written to answer whether making the shader depth-aware cost the
//! eight-bit path anything.** The branches it gained are uniform -- every
//! invocation in a dispatch takes the same one -- so in principle a device
//! predicates them away and the answer is nothing. In principle is not a
//! measurement, and a hot path is exactly where that distinction is expensive
//! to get wrong.
//!
//! **Serialized on purpose.** Each conversion is submitted and waited on
//! before the next is built, so what is timed is one dispatch rather than how
//! well a queue keeps busy. That makes it a worse throughput figure and a
//! better comparison, which is what this is for.
//!
//! The first conversions are discarded: the first touches a cold pipeline and
//! this device raises its own clocks under load, so an early sample measures
//! the ramp.

use lowlat_capture::convert::{Converter, Depth};
use lowlat_capture::vulkan::Device;

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(2);
}

/// Ignored samples at the head of a run.
const WARMUP: usize = 32;

fn main() {
    let mut args = std::env::args().skip(1);
    let frames: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(600);
    let width: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1920);
    let height: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1080);

    let device = Device::any().unwrap_or_else(|e| fail(&format!("no device that converts: {e}")));

    // Content with detail in every channel, so nothing is optimised away for
    // being flat. It never changes between frames: the conversion reads every
    // pixel whatever it holds, and redrawing would time the host instead.
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let at = (y * width as usize + x) * 4;
            pixels[at] = u8::try_from((x / 3) & 0xff).unwrap_or(0);
            pixels[at + 1] = u8::try_from((y / 5) & 0xff).unwrap_or(0);
            pixels[at + 2] = u8::try_from((x + y) & 0xff).unwrap_or(0);
            pixels[at + 3] = 255;
        }
    }

    let source = device
        .upload_rgba(width, height, &pixels)
        .unwrap_or_else(|e| fail(&format!("upload: {e}")));
    let mut converter = Converter::new(&device).unwrap_or_else(|e| fail(&format!("pipeline: {e}")));

    println!("{width}x{height}, {frames} conversions each, serialized");
    for depth in [Depth::Eight, Depth::Ten] {
        let target = match device.allocate_planar(width, height, depth) {
            Ok(target) => target,
            Err(error) => {
                // A refusal is an answer: this device does not offer the
                // layout, and saying so beats reporting a time for nothing.
                println!("  {depth:?}: no target on this device ({error})");
                continue;
            }
        };
        let mut samples = Vec::with_capacity(frames);
        for frame in 0..frames + WARMUP {
            let started = std::time::Instant::now();
            converter
                .run(&device, &source, &target.target(), false)
                .unwrap_or_else(|e| fail(&format!("convert: {e}")));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            if frame >= WARMUP {
                samples.push(took);
            }
        }
        samples.sort_by(f64::total_cmp);
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "an index into a sample count that fits a float exactly, \
                      from a quantile that is never negative"
        )]
        let at = |q: f64| samples[(((samples.len() - 1) as f64) * q).round() as usize];
        println!(
            "  {depth:?}: p50 {:.3} ms  p99 {:.3} ms  min {:.3} ms",
            at(0.50),
            at(0.99),
            samples[0]
        );
        device.release_nv12(target);
    }
    device.release(source);
    converter.destroy(&device);
}
