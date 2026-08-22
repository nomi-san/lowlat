//! Sound capture diagnostic. Reads the desktop's own output through the
//! capture path the host uses, and reports what it cost.
//!
//! Run it on the machine under test, as the user or as the service:
//!
//!   audio-probe [device] [server] [seconds]
//!
//! `device` is a name from the sound server, or a dash for the default
//! output's monitor. `server` is that session's socket when this process is
//! outside the session, or a dash for whatever the environment names:
//!
//!   sudo audio-probe - unix:/run/user/1000/pulse/native 10
//!
//! Play something audible while it runs, or the content line says nothing.
//!
//! **What it established, before any of this crate existed**, on a workstation
//! whose sound server belongs to a logged-in session:
//!
//!   - **a service is admitted to that session's socket**, with or without the
//!     session's own authentication cookie, so audio capture needs no helper
//!   - **the source is a clock**: frames arrive on the server's graph period,
//!     p50 21.33 ms against the 20 ms asked for, p99 21.60, max 25.75
//!   - **the rate is exact even though the spacing is not** -- 300 seconds of
//!     reading came out 10 ms short of the wall clock, and the same figure
//!     appears at 10 and 60 seconds, so it is the connect and not a drift
//!   - **silence keeps flowing as zeros** rather than stopping, so skipping it
//!     is this host's decision and not the platform's
//!
//! **Two things it also found, both by accident, and both now rules:** a device
//! name that does not resolve is **substituted rather than refused**, so a
//! requested one is checked against what the stream landed on; and a capture
//! **does not follow the default output**, which is resolved once when the
//! stream connects, so following it is this host's work.

use std::sync::mpsc;

use lowlat_audio::{Capture, Config, FRAME, FRAME_BYTES, SAMPLE_RATE};
use lowlat_common::clock::{Time, elapsed_ms};

/// How long to read for when nobody says, in frames of 20 ms.
const FRAMES: usize = 10 * 50;

fn main() {
    let mut args = std::env::args().skip(1);
    let dash = |value: String| Some(value).filter(|v| v != "-" && !v.is_empty());
    let device = args.next().and_then(dash);
    let server = args.next().and_then(dash);
    let frames = args
        .next()
        .and_then(|seconds| seconds.parse::<usize>().ok())
        .map_or(FRAMES, |seconds| seconds * 50);

    lowlat_common::log::set_level(lowlat_common::log::Level::Info);

    let (sender, received) = mpsc::channel();
    let began = Time::now();
    let mut previous = Time::now();
    let capture = match Capture::open(
        Config {
            server: server.clone(),
            device: device.clone(),
        },
        move |frame: &[u8]| {
            let now = Time::now();
            let gap = elapsed_ms(previous);
            previous = now;
            let mut loudest = 0i32;
            let mut squares = 0f64;
            for pair in frame.chunks_exact(2) {
                let sample = i32::from(i16::from_le_bytes([pair[0], pair[1]]));
                squares += f64::from(sample) * f64::from(sample);
                loudest = loudest.max(sample.abs());
            }
            let _ = sender.send((gap, squares, loudest, frame.len()));
        },
    ) {
        Ok(capture) => capture,
        Err(error) => {
            println!("open FAILED device={device:?} server={server:?} error={error}");
            return;
        }
    };
    println!(
        "open ok device={} rate={SAMPLE_RATE} frame={FRAME} ({FRAME_BYTES} bytes)",
        capture.device()
    );

    let mut gaps = Vec::with_capacity(frames);
    let mut squares = 0f64;
    let mut peak = 0i32;
    let mut silent = 0usize;
    let mut short = 0usize;
    for _ in 0..frames {
        let Ok((gap, energy, loudest, len)) = received.recv() else {
            break;
        };
        gaps.push(gap);
        squares += energy;
        peak = peak.max(loudest);
        if loudest == 0 {
            silent += 1;
        }
        if len != FRAME_BYTES {
            short += 1;
        }
    }
    let elapsed = elapsed_ms(began);
    let on = capture.device();
    drop(capture);

    gaps.sort_by(f64::total_cmp);
    // Percent rather than a fraction, so the index is integer arithmetic and
    // the rounding is not a float cast.
    let at = |percent: usize| -> f64 {
        let index = gaps.len().saturating_sub(1) * percent / 100;
        gaps.get(index).copied().unwrap_or(0.0)
    };
    let count = gaps.len();
    let captured_ms = (count * FRAME) as f64 * 1000.0 / f64::from(SAMPLE_RATE);
    let samples = (count * FRAME * lowlat_audio::CHANNELS) as f64;

    println!("frames: {count} of {frames}, on {on}");
    println!("wrong size: {short}");
    println!(
        "cadence ms: min {:.2}  p50 {:.2}  p95 {:.2}  p99 {:.2}  max {:.2}",
        at(0),
        at(50),
        at(95),
        at(99),
        at(100)
    );
    println!(
        "clock: captured {captured_ms:.0} ms in {elapsed:.0} ms of wall time, \
         drift {:+.0} ms",
        captured_ms - elapsed
    );
    println!(
        "content: rms {:.0} peak {peak} silent frames {silent} of {count}{}",
        (squares / samples.max(1.0)).sqrt(),
        if silent == count {
            "  <- nothing was playing, so this says nothing about the path"
        } else {
            ""
        }
    );
}
