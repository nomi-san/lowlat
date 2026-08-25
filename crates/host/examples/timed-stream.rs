//! The stream loop against the real display, with a local seat.
//!
//!   sudo timed-stream [seconds]
//!
//! Starts the same encode loop the daemon runs, capturing the display named
//! in `LOWLAT_OUTPUT` (default `card0:HDMI-A-1`) and encoding on the backend
//! named in `LOWLAT_BACKEND` (default the open stack), and reads what the
//! seat receives for the duration. Prints the stage report the loop
//! publishes.

use lowlat::stream::{Backend, Codec, Config, Stream};
use lowlat::timing::Report;

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(15);
    let output = std::env::var("LOWLAT_OUTPUT").ok();
    let backend = match std::env::var("LOWLAT_BACKEND").as_deref() {
        Ok("vendor") => Backend::Vendor,
        _ => Backend::Open,
    };

    // **A watchdog, because a wedged display stack can hold a fence wait.**
    // A run that has not finished in twice its budget plus setup is not going
    // to; dying at a point of our choosing beats being killed mid-teardown
    // minutes later, while holding imported scanout buffers and an armed
    // vblank event. The alarm's default action ends the process.
    // SAFETY: nothing else in this process arms an alarm.
    unsafe { libc::alarm(u32::try_from(seconds).unwrap_or(30) * 2 + 30) };

    let stream = Stream::start(Config {
        audio: None,
        convert: None,
        prefer_vulkan: false,
        audio_on: false,
        accept_microphone: false,
        audio_kbps: 128,
        allow_raw_audio: false,
        output: output.clone(),
        display: true,
        width: 1920,
        height: 1080,
        fps: 60,
        cg_level: 1,
        full_fps: false,
        codec: Codec::H264,
        backend: Some(backend),
        configured_mbps: 10.0,
        min_mbps: 1.0,
        rotation: lowlat_core::video::Rotation::None,
        detail_rows: 0,
    });

    let wake = lowlat_net::Wake::new().expect("wake");
    let seat = stream
        .seats()
        .take(
            wake.handle().expect("handle"),
            wake.handle().expect("a second handle"),
        )
        .expect("a free seat");

    let began = std::time::Instant::now();
    let mut received = 0usize;
    while began.elapsed().as_secs() < seconds {
        while let Some(frame) = seat.next_frame() {
            let _ = frame.bytes().len();
            received += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let report: Report = stream.timings();
    println!(
        "received {received} frames in {seconds}s on {}",
        output.as_deref().unwrap_or("the default output")
    );
    let line = |name: &str, p50: f64, p95: f64, p99: f64| {
        println!("  {name:<9} p50 {p50:.3} ms  p95 {p95:.3} ms  p99 {p99:.3} ms");
    };
    line(
        "acquire",
        report.acquire.p50,
        report.acquire.p95,
        report.acquire.p99,
    );
    line(
        "convert",
        report.convert.p50,
        report.convert.p95,
        report.convert.p99,
    );
    line(
        "pointer",
        report.pointer.p50,
        report.pointer.p95,
        report.pointer.p99,
    );
    line(
        "encode",
        report.encode.p50,
        report.encode.p95,
        report.encode.p99,
    );
    line(
        "publish",
        report.publish.p50,
        report.publish.p95,
        report.publish.p99,
    );
    line(
        "interval",
        report.interval.p50,
        report.interval.p95,
        report.interval.p99,
    );
    println!(
        "host stages sum p50 {:.3} ms  p99 {:.3} ms",
        report.host_p50(),
        report.host_p99()
    );
}
