//! What the conversion costs at a stream's cadence, with the source fixed or
//! live.
//!
//!   sudo cadence-probe [fixed|live] [sleep_ms] [frames]
//!
//! `fixed` converts one imported framebuffer round and round; `live` re-reads
//! the scanned-out framebuffer every iteration the way the stream does.
//! Neither encodes. The sleep between conversions is the knob: a figure that
//! grows with the gap has a cause in the gap (clock state, queue wakeup), and
//! one that grows only in `live` mode has a cause in what a fresh framebuffer
//! costs to read.

use std::io::Write;
use std::os::fd::IntoRawFd;

use lowlat_capture::convert::Converter;
use lowlat_capture::scanout::Card;
use lowlat_capture::vulkan::{Device, Imports, PlaneLayout};

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "fixed".to_string());
    let sleep_ms: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(16);
    let frames: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(240);

    let card = Card::open(std::path::Path::new("/dev/dri/card0"))
        .unwrap_or_else(|e| fail(&format!("open: {e}")));
    let layout = card.scan().unwrap_or_else(|e| fail(&format!("scan: {e}")));
    let plane = layout.primary_plane;
    let device = Device::for_display(std::path::Path::new("/dev/dri/card0"))
        .unwrap_or_else(|e| fail(&format!("device: {e}")));
    let mut converter = Converter::new(&device).unwrap_or_else(|e| fail(&format!("pipeline: {e}")));
    let mut targets = Vec::new();
    for _ in 0..4 {
        targets.push(
            device
                .allocate_nv12(layout.primary.width, layout.primary.height)
                .unwrap_or_else(|e| fail(&format!("target: {e}"))),
        );
    }
    let mut imports: std::collections::HashMap<u64, lowlat_capture::vulkan::Imported> =
        std::collections::HashMap::new();
    // The fixed source: one framebuffer imported once, whatever the display
    // does afterwards.
    let pinned = ensure(&card, plane, &device, &mut imports);

    let mut collects: Vec<f64> = Vec::with_capacity(frames);
    let mut seconds: Vec<f64> = Vec::with_capacity(frames);
    let mut exports = 0u64;
    let mut imported = 0u64;
    // `double` times two back-to-back conversions per wake: if the first is
    // slow and the second is fast, the cost is a wakeup or a clock ramp; if
    // both are slow, the device is genuinely slower at this phase.
    let double = std::env::args().any(|arg| arg == "--double");
    for at in 0..frames {
        let key = if mode == "fixed" {
            pinned
        } else if mode == "poked" {
            // A partial-conversion poke, then a short gap, then the timed
            // conversion. `LOWLAT_POKE` picks the size: 1 is one workgroup,
            // 8 is an eighth of the picture in each axis, full is the lot.
            let target = targets
                .get(at % 4)
                .unwrap_or_else(|| fail("missing target"));
            let gx = layout.primary.width.div_ceil(2).div_ceil(8);
            let gy = layout.primary.height.div_ceil(2).div_ceil(8);
            let groups = match std::env::var("LOWLAT_POKE").as_deref() {
                Ok("full") => (gx, gy),
                Ok("8") => (gx.div_ceil(8), gy.div_ceil(8)),
                _ => (1, 1),
            };
            converter
                .poke(
                    &device,
                    imports.get(&pinned).unwrap_or_else(|| fail("no source")),
                    &target.target(),
                    groups,
                )
                .unwrap_or_else(|e| fail(&format!("poke: {e}")));
            converter
                .collect(&device)
                .unwrap_or_else(|e| fail(&format!("poke collect: {e}")));
            std::thread::sleep(std::time::Duration::from_millis(2));
            pinned
        } else {
            let fb = card
                .framebuffer_on(plane)
                .unwrap_or_else(|e| fail(&format!("read: {e}")));
            let first = fb.planes().next().unwrap_or_else(|| fail("no buffers"));
            let fd = card
                .export(first)
                .unwrap_or_else(|e| fail(&format!("export: {e}")));
            exports += 1;
            let key = lowlat_capture::scanout::identity(&fd).unwrap_or(0);
            if let std::collections::hash_map::Entry::Vacant(entry) = imports.entry(key) {
                let planes: Vec<PlaneLayout> = fb
                    .planes()
                    .map(|b| PlaneLayout {
                        offset: b.offset,
                        pitch: b.pitch,
                    })
                    .collect();
                let source = Imports {
                    width: fb.width,
                    height: fb.height,
                    format: fb.format,
                    modifier: fb.modifier.map_or(0, u64::from),
                    fd: fd.into_raw_fd(),
                    planes: &planes,
                };
                let got = device
                    .import(&source)
                    .unwrap_or_else(|e| fail(&format!("import: {e}")));
                entry.insert(got);
                imported += 1;
            }
            key
        };
        let source = imports.get(&key).unwrap_or_else(|| fail("missing import"));
        let target = targets
            .get(at % 4)
            .unwrap_or_else(|| fail("missing target"));
        converter
            .submit(&device, source, &target.target(), false)
            .unwrap_or_else(|e| fail(&format!("submit: {e}")));
        let began = std::time::Instant::now();
        converter
            .collect(&device)
            .unwrap_or_else(|e| fail(&format!("collect: {e}")));
        collects.push(began.elapsed().as_secs_f64() * 1000.0);
        if double {
            let target = targets
                .get((at + 1) % 4)
                .unwrap_or_else(|| fail("missing target"));
            converter
                .submit(&device, source, &target.target(), false)
                .unwrap_or_else(|e| fail(&format!("submit: {e}")));
            let began = std::time::Instant::now();
            converter
                .collect(&device)
                .unwrap_or_else(|e| fail(&format!("collect: {e}")));
            seconds.push(began.elapsed().as_secs_f64() * 1000.0);
        }
        if sleep_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        }
    }

    collects.sort_by(f64::total_cmp);
    let rank = |list: &[f64], num: usize, den: usize| {
        list.get((list.len().saturating_sub(1)) * num / den)
            .copied()
            .unwrap_or(0.0)
    };
    println!(
        "{} sleep={sleep_ms}ms frames={frames} exports={exports} imports={imported}: \
         convert p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms  max {:.3} ms",
        mode,
        rank(&collects, 50, 100),
        rank(&collects, 95, 100),
        rank(&collects, 99, 100),
        rank(&collects, 1, 1)
    );
    if !seconds.is_empty() {
        seconds.sort_by(f64::total_cmp);
        println!(
            "second after the wake: p50 {:.3} ms  p95 {:.3} ms  max {:.3} ms",
            rank(&seconds, 50, 100),
            rank(&seconds, 95, 100),
            rank(&seconds, 1, 1)
        );
    }
    let _ = std::io::stdout().flush();
}

fn ensure(
    card: &Card,
    plane: drm::control::plane::Handle,
    device: &Device,
    imports: &mut std::collections::HashMap<u64, lowlat_capture::vulkan::Imported>,
) -> u64 {
    let fb = card
        .framebuffer_on(plane)
        .unwrap_or_else(|e| fail(&format!("read: {e}")));
    let first = fb.planes().next().unwrap_or_else(|| fail("no buffers"));
    let fd = card
        .export(first)
        .unwrap_or_else(|e| fail(&format!("export: {e}")));
    let key = lowlat_capture::scanout::identity(&fd).unwrap_or(0);
    if imports.contains_key(&key) {
        return key;
    }
    let planes: Vec<PlaneLayout> = fb
        .planes()
        .map(|b| PlaneLayout {
            offset: b.offset,
            pitch: b.pitch,
        })
        .collect();
    let source = Imports {
        width: fb.width,
        height: fb.height,
        format: fb.format,
        modifier: fb.modifier.map_or(0, u64::from),
        fd: fd.into_raw_fd(),
        planes: &planes,
    };
    let got = device
        .import(&source)
        .unwrap_or_else(|e| fail(&format!("import: {e}")));
    imports.insert(key, got);
    key
}

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(1)
}
