//! Does the open-stack encode run while the loop sleeps, or does it start when
//! the collect asks?
//!
//!   sudo collect-wait-probe [/dev/dri/card0] [/dev/dri/renderD128] [pictures] [kbps]
//!
//! `LOWLAT_CODEC=h264` selects the first codec; the second is the default.
//! **Both are worth running**: the wait is codec-independent by construction,
//! and a claim that says so without having run the other one is an assumption.
//!
//! **The stream loop sleeps a millisecond between submitting a picture and
//! polling for it, and the collect now blocks on the surface.** Whether that
//! sleep costs anything depends on something no code here can state: if the
//! device begins the encode when the picture is submitted, the sleep overlaps
//! work and removing it saves nothing, because the block that follows simply
//! grows by what the sleep covered. If the work instead begins when the
//! surface is synchronised, the sleep is added latency in full.
//!
//! The two are indistinguishable from one delay. They separate under a sweep:
//! submit, wait `S`, then time the block, for several `S` including ones
//! **longer than the encode itself**.
//!
//!   overlapped  ->  block falls as S rises and reaches zero past the encode;
//!                   total is flat at the encode time.
//!   deferred    ->  block is flat whatever S is; total is S plus the encode.
//!
//! The delays are interleaved picture by picture rather than run in blocks, so
//! the rate controller's ramp -- twelve seconds on this device -- lands on
//! every delay equally instead of on whichever went first.
//!
//! One capture, converted once, submitted again and again: the content is a
//! duplicate by construction, so what changes between pictures is the delay
//! and nothing else.

use std::os::fd::{AsFd, IntoRawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lowlat_capture::convert::{Converter, Nv12};
use lowlat_capture::scanout::Card;
use lowlat_capture::vulkan::{Device, Imports, PlaneLayout};
use lowlat_encode::{Poll, vaapi};

/// The delays swept, in microseconds. Zero is the tight poll the report's
/// section 2 measured; 1000 is the loop's own sleep; the last two are past any
/// encode time seen on this hardware, which is what makes the answer visible
/// rather than a matter of reading two close numbers.
const DELAYS_US: [u64; 5] = [0, 500, 1000, 4000, 8000];

/// How long the encoder is left alone before a picture is handed to it.
///
/// **A gap before the submit is a different question from a delay after it**,
/// and the delay cannot answer it. A device that powers its encode block down
/// between pictures pays the wakeup inside the encode, and a delay longer than
/// the encode hides exactly that: the wait returns immediately because the
/// sleep already covered it, so the block reads zero whether the picture took
/// two milliseconds or fourteen. Idling here instead and then polling tight
/// puts the whole cost back into the measurement.
fn gap() -> std::time::Duration {
    std::time::Duration::from_micros(
        std::env::var("LOWLAT_GAP_US")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
    )
}

/// The sweep, overridable so one delay can be dwelt on.
///
/// **A tail needs samples, and a sweep spends them on delays that are not the
/// question.** Set `LOWLAT_DELAYS_US` to a comma-separated list -- one value
/// puts the whole run into that regime, which is what reproducing a device's
/// power gating needs: the gap between pictures is the variable, and the cost
/// shows up in a percentile rather than a median.
fn delays() -> Vec<u64> {
    match std::env::var("LOWLAT_DELAYS_US") {
        Ok(named) => named
            .split(',')
            .filter_map(|value| value.trim().parse().ok())
            .collect(),
        Err(_) => DELAYS_US.to_vec(),
    }
}

/// Pictures dropped before anything is counted, so the rate control has
/// settled and the opening refresh is not averaged in.
const SETTLE: usize = 120;

/// Surfaces in the ring. Two, not one: the picture is submitted and collected
/// serially, so one would do, and a second costs nothing and keeps the driver
/// from being handed the buffer it has only just released.
const RING: usize = 2;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let node = args
        .first()
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);
    let render = args.get(1).map_or_else(
        || c"/dev/dri/renderD128".to_owned(),
        |a| std::ffi::CString::new(a.as_str()).expect("a render node path"),
    );
    let pictures: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(1200);
    let kbps: u32 = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(5500);
    let codec = match std::env::var("LOWLAT_CODEC").as_deref() {
        Ok("h264" | "avc") => vaapi::Codec::H264,
        _ => vaapi::Codec::H265,
    };

    let card = Card::open(&node).unwrap_or_else(|error| fail(&format!("open {node:?}: {error}")));
    let layout = card
        .scan()
        .unwrap_or_else(|error| fail(&format!("scan: {error}")));
    let fb = &layout.primary;
    println!(
        "collect-wait-probe: {node:?} {}x{} through {render:?}, {codec:?}, {pictures} pictures, \
         {kbps} kbit/s, gap {:.1} ms",
        fb.width,
        fb.height,
        gap().as_secs_f64() * 1000.0
    );

    let device =
        Device::for_display(&node).unwrap_or_else(|error| fail(&format!("device: {error}")));
    let mut converter =
        Converter::new(&device).unwrap_or_else(|error| fail(&format!("pipeline: {error}")));

    // One capture, imported once. Nothing reads the display again.
    let planes: Vec<PlaneLayout> = fb
        .planes()
        .map(|buffer| PlaneLayout {
            offset: buffer.offset,
            pitch: buffer.pitch,
        })
        .collect();
    let first = fb.planes().next().unwrap_or_else(|| fail("no buffers"));
    let fd = card
        .export(first)
        .unwrap_or_else(|error| fail(&format!("export: {error}")));
    let source = device
        .import(&Imports {
            width: fb.width,
            height: fb.height,
            format: fb.format,
            modifier: fb.modifier.map_or(0, u64::from),
            fd: fd.into_raw_fd(),
            planes: &planes,
        })
        .unwrap_or_else(|error| fail(&format!("import: {error}")));

    let targets: Vec<Nv12> = (0..RING)
        .map(|_| {
            device
                .allocate_nv12(fb.width, fb.height)
                .unwrap_or_else(|error| fail(&format!("allocate: {error}")))
        })
        .collect();
    for target in &targets {
        converter
            .run(&device, &source, &target.target(), false)
            .unwrap_or_else(|error| fail(&format!("convert: {error}")));
    }

    let va = vaapi::Vaapi::load().unwrap_or_else(|error| fail(&format!("runtime: {error:?}")));
    let display = va
        .open(&render)
        .unwrap_or_else(|error| fail(&format!("render node: {error:?}")));
    let caps = display
        .caps(codec)
        .unwrap_or_else(|error| fail(&format!("caps: {error:?}")));
    let context = display
        .create_context(caps, targets[0].width, targets[0].height, RING)
        .unwrap_or_else(|error| fail(&format!("context: {error:?}")));

    // Registered exactly as the loop registers them.
    let mut registered = Vec::with_capacity(targets.len());
    let mut _held = Vec::with_capacity(targets.len());
    for target in &targets {
        let (fd, exported) = device
            .export_nv12(target, true)
            .unwrap_or_else(|error| fail(&format!("export target: {error}")));
        let surface = display
            .import(fd.as_fd(), &exported)
            .unwrap_or_else(|error| fail(&format!("register: {error:?}")));
        registered.push(surface);
        _held.push(fd);
    }

    let params = match codec {
        vaapi::Codec::H264 => vaapi::Params::H264(lowlat_encode::h264::Params {
            width: targets[0].width,
            height: targets[0].height,
            fps: 60,
            level_idc: 42,
            log2_max_frame_num_minus4: 4,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
        }),
        vaapi::Codec::H265 => vaapi::Params::H265(lowlat_encode::h265::Params {
            width: targets[0].width,
            height: targets[0].height,
            fps: 60,
            level_idc: 123,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
            transform_depth: lowlat_encode::h265::TRANSFORM_HIERARCHY_DEPTH,
        }),
    };
    let mut encoder = context
        .encoder(params, kbps.saturating_mul(1000))
        .unwrap_or_else(|error| fail(&format!("encoder: {error:?}")));
    if let Ok(floor) = std::env::var("LOWLAT_MIN_QP")
        && let Ok(floor) = floor.parse::<u32>()
    {
        encoder.set_min_qp(floor);
    }
    if let Ok(level) = std::env::var("LOWLAT_QUALITY")
        && let Ok(level) = level.parse::<u32>()
    {
        encoder.set_quality(level);
    }
    println!(
        "device offers {} effort level(s), asking for {}, quantiser floor {}",
        caps.quality_range,
        encoder.quality(),
        encoder.min_qp()
    );

    // Per delay: the sleep as it really landed, the block, and the whole span
    // a stream loop would charge the picture.
    let sweep = delays();
    let gap = gap();
    let convert_between = std::env::var("LOWLAT_CONVERT_BETWEEN").is_ok();
    let mut slept: Vec<Vec<f64>> = vec![Vec::new(); sweep.len()];
    let mut blocked: Vec<Vec<f64>> = vec![Vec::new(); sweep.len()];
    let mut total: Vec<Vec<f64>> = vec![Vec::new(); sweep.len()];
    let mut bytes: Vec<usize> = Vec::with_capacity(pictures);

    for index in 0..pictures {
        let slot = index % sweep.len();
        let delay = sweep[slot];
        let surface = registered[index % registered.len()];
        if !gap.is_zero() {
            std::thread::sleep(gap);
        }
        let submitted = Instant::now();
        encoder
            .submit_registered(surface, false)
            .unwrap_or_else(|error| fail(&format!("submit: {error:?}")));
        if delay > 0 {
            std::thread::sleep(Duration::from_micros(delay));
        }
        // **What the loop does on a pass that runs late.** Its tick normally
        // collects the previous picture on a pass with nothing else to do, but
        // once a frame overruns, the pass that collects is also the pass that
        // reads the display and submits the next conversion -- and both land
        // between the submit and the wait, on the same device the encode is
        // running on. Set `LOWLAT_CONVERT_BETWEEN` to reproduce that ordering.
        if convert_between {
            let other = targets
                .get((index + 1) % targets.len())
                .unwrap_or_else(|| fail("missing target"));
            converter
                .run(&device, &source, &other.target(), false)
                .unwrap_or_else(|error| fail(&format!("convert between: {error}")));
        }
        let asked = Instant::now();
        let len = match encoder
            .poll()
            .unwrap_or_else(|error| fail(&format!("poll: {error:?}")))
        {
            Poll::Ready { bitstream, .. } => bitstream.len(),
            // The collect blocks, so this cannot happen with one in flight;
            // if it ever does, the run is measuring something else.
            Poll::Pending => fail("the collect reported pending with a picture in flight"),
        };
        let collected = Instant::now();
        if index >= SETTLE {
            slept[slot].push(ms(submitted, asked));
            blocked[slot].push(ms(asked, collected));
            total[slot].push(ms(submitted, collected));
        }
        bytes.push(len);
    }

    // In tenths, because a mean hides the rate controller's ramp and a run
    // that ended inside it reports a picture getting cheaper.
    let step = bytes.len() / 10;
    if step > 0 {
        let mut line = String::new();
        for chunk in bytes.chunks(step).take(10) {
            #[allow(
                clippy::cast_precision_loss,
                reason = "a byte count of a coded picture"
            )]
            let mean = chunk.iter().sum::<usize>() as f64 / chunk.len() as f64;
            line.push_str(&format!("{mean:.0} "));
        }
        println!("trajectory by tenths of the run, mean bytes: {line}");
    }

    println!();
    println!(
        "  delay asked   slept p50   block p50   block p95   block p99   block max   total p50      n"
    );
    for (slot, delay) in sweep.iter().enumerate() {
        println!(
            "{:>9.1} ms {:>9.2} ms {:>9.2} ms {:>9.2} ms {:>9.2} ms {:>9.2} ms {:>9.2} ms {:>6}",
            f64::from(u32::try_from(*delay).unwrap_or(u32::MAX)) / 1000.0,
            pct(&slept[slot], 50),
            pct(&blocked[slot], 50),
            pct(&blocked[slot], 95),
            pct(&blocked[slot], 99),
            pct(&blocked[slot], 100),
            pct(&total[slot], 50),
            blocked[slot].len(),
        );
    }

    println!();
    println!(
        "read the block column: falling as the delay rises means the device encodes while the \
         loop sleeps, so the sleep costs nothing and the total is flat. A block that does not \
         move means the work starts at the synchronise and the sleep is added latency in full, \
         which is what the total column will then show."
    );
}

/// The p'th percentile in milliseconds, or zero from nothing.
fn pct(values: &[f64], p: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(sorted.len() - 1) * p / 100]
}

fn ms(from: Instant, to: Instant) -> f64 {
    to.duration_since(from).as_secs_f64() * 1000.0
}

fn fail(message: &str) -> ! {
    eprintln!("collect-wait-probe: {message}");
    std::process::exit(1)
}
