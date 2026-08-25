//! What an unchanged picture costs the open-stack encoder on the path the
//! stream loop actually uses.
//!
//!   sudo registered-probe [/dev/dri/card0] [pictures] [depth] [kbps] [coded-height]
//!
//! **The difference from the unit probe is the surface, and it is the whole
//! point.** Submitting a frame from ordinary memory hands the driver its own
//! input surfaces, allocated at whatever alignment it wants. The loop instead
//! registers surfaces we allocated, exported and handed over, and rotates
//! through a ring of them. If a duplicate is cheap through the first path and
//! expensive through the second, the difference is in what the driver reads
//! out of our allocation rather than in what the picture contains.
//!
//! So: capture the display once, convert that one source into every target in
//! the ring -- which is byte-identical content, established separately -- and
//! then submit the ring round and round without touching the display again.
//! Every picture after the first is a duplicate by construction. What it costs
//! is what this reports.
//!
//! **Run it long, and read the trajectory rather than the mean.** The rate
//! controller takes about twelve seconds to settle on this device, and a run
//! that ends inside the ramp measures a picture getting cheaper rather than
//! what a duplicate really costs. A 240-picture run read 627 bytes where 4000
//! pictures read 7406.
//!
//! **The coded height is a knob because the device codes at its own
//! alignment**, and the rows between the picture and that alignment are never
//! written by the conversion. Passing a height above the captured one
//! reproduces that deliberately. Two things were measured through it and both
//! are worth not re-deriving:
//!
//!   - The device **refuses** a surface allocated smaller than its aligned
//!     size ("input surface size doesn't match aligned size"), so those rows
//!     are always inside the allocation and never past it.
//!   - Coding 16 rows nobody wrote appeared to cost **nothing**: 619 bytes a
//!     duplicate against 627 with no override, at 2560x1440. **Both figures
//!     are transients** -- see the warning below -- so the conclusion wants
//!     re-measuring at length before it is relied on.

use std::os::fd::{AsFd, IntoRawFd};
use std::path::PathBuf;

use lowlat_capture::convert::{Converter, Nv12};
use lowlat_capture::scanout::Card;
use lowlat_capture::vulkan::{Device, Imports, PlaneLayout};
use lowlat_encode::{Poll, vaapi};

/// The render node the open stack is reached through.
const RENDER: &std::ffi::CStr = c"/dev/dri/renderD128";

/// Pictures ignored before the tail is measured, so the rate control has
/// settled and the opening refresh is not averaged in.
const SETTLE: usize = 60;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let node = args
        .first()
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);
    let pictures: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(240);
    let depth: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(4);
    // **Kilobits, not megabits.** The rate reaches the encoder as an integer
    // and taking it as one here keeps the argument exact.
    let kbps: u32 = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(5500);
    let bps = kbps.saturating_mul(1000);
    let coded: Option<u32> = args.get(4).and_then(|a| a.parse().ok());

    let card = Card::open(&node).unwrap_or_else(|error| fail(&format!("open {node:?}: {error}")));
    let layout = card
        .scan()
        .unwrap_or_else(|error| fail(&format!("scan: {error}")));
    let fb = &layout.primary;
    println!(
        "registered-probe: {node:?} {}x{}, {pictures} pictures, ring of {depth}, {kbps} kbit/s",
        fb.width, fb.height
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

    // **Allocated for the coded height, filled only to the captured one.**
    // The device refuses a surface smaller than its own alignment, so the rows
    // it codes past the picture are inside the allocation rather than past it
    // -- and nothing ever writes them.
    let coded_height = coded.unwrap_or(fb.height).max(fb.height);
    let targets: Vec<Nv12> = (0..depth)
        .map(|_| {
            device
                .allocate_nv12(fb.width, coded_height)
                .unwrap_or_else(|error| fail(&format!("allocate: {error}")))
        })
        .collect();
    for target in &targets {
        converter
            .run(&device, &source, &target.target(), false)
            .unwrap_or_else(|error| fail(&format!("convert: {error}")));
    }
    println!(
        "converted one source into {} target(s) of {}x{} on {}",
        targets.len(),
        targets[0].width,
        targets[0].height,
        device.name()
    );

    let va = vaapi::Vaapi::load().unwrap_or_else(|error| fail(&format!("runtime: {error:?}")));
    let display = va
        .open(RENDER)
        .unwrap_or_else(|error| fail(&format!("render node: {error:?}")));
    let caps = display
        .caps(vaapi::Codec::H265)
        .unwrap_or_else(|error| fail(&format!("caps: {error:?}")));
    if coded_height != fb.height {
        println!(
            "coding {} rows of which the conversion fills {}: {} row(s) are never written",
            coded_height,
            fb.height,
            coded_height - fb.height
        );
    }
    let context = display
        .create_context(caps, targets[0].width, coded_height, depth)
        .unwrap_or_else(|error| fail(&format!("context: {error:?}")));

    // Registered exactly as the loop registers them: exported for the display
    // interface, handed to the encoder, and kept alive for the run.
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
    println!("registered {} surface(s)", registered.len());

    let params = lowlat_encode::h265::Params {
        width: targets[0].width,
        height: coded_height,
        fps: 60,
        level_idc: 123,
        log2_max_poc_lsb_minus4: 4,
        max_num_ref_frames: 1,
    };
    let mut encoder = context
        .encoder(vaapi::Params::H265(params), bps)
        .unwrap_or_else(|error| fail(&format!("encoder: {error:?}")));

    let mut sizes: Vec<usize> = Vec::with_capacity(pictures);
    let mut submitted = 0usize;
    while sizes.len() < pictures {
        if submitted < pictures && encoder.in_flight() < depth {
            let surface = registered[submitted % registered.len()];
            encoder
                .submit_registered(surface, false)
                .unwrap_or_else(|error| fail(&format!("submit: {error:?}")));
            submitted += 1;
        }
        match encoder
            .poll()
            .unwrap_or_else(|error| fail(&format!("poll: {error:?}")))
        {
            Poll::Ready { bitstream, .. } => sizes.push(bitstream.len()),
            Poll::Pending => std::hint::spin_loop(),
        }
    }

    // **In tenths, because an average hides a ramp.** A rate controller that
    // starts high and drives the quantizer down to fill its budget on an
    // unchanging picture looks identical to a cheap duplicate if the run ends
    // before it settles.
    let step = sizes.len() / 10;
    if step > 0 {
        let mut line = String::new();
        for chunk in sizes.chunks(step).take(10) {
            let mean = chunk.iter().sum::<usize>() as f64 / chunk.len() as f64;
            line.push_str(&format!("{mean:.0} "));
        }
        println!("trajectory by tenths of the run, mean bytes: {line}");
    }

    let tail = &sizes[SETTLE.min(sizes.len().saturating_sub(1))..];
    let mean = tail.iter().sum::<usize>() as f64 / tail.len() as f64;
    let mut sorted = tail.to_vec();
    sorted.sort_unstable();
    println!();
    println!(
        "refresh {} bytes, then {} duplicates: mean {mean:.0} B, min {} B, median {} B, max {} B",
        sizes[0],
        tail.len(),
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1]
    );
    println!(
        "implied wire rate at 60 fps: {:.3} mbit/s",
        mean * 8.0 * 60.0 / 1e6
    );

    // A picture the encoder had nothing to predict costs a slice header and
    // its skip flags. Anything far above that is bits spent on a difference
    // that is not in the picture.
    println!();
    println!(
        "read the trajectory, not the mean: the rate controller needs about twelve seconds to \
         settle, and a run that stops inside the ramp reports the opposite of the truth. \
         Measured here at 1920x1080: a duplicate settles at 7406 B, which is 3.4 mbit/s."
    );
}

fn fail(message: &str) -> ! {
    eprintln!("registered-probe: {message}");
    std::process::exit(1)
}
