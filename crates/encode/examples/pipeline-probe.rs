//! The whole path, once: capture the display, import it, convert it, hand the
//! result to the encoder without a copy, and write what comes back.
//!
//! It lives here rather than beside the capture code because the dependency
//! between the two runs this way, and a development-only edge in the other
//! direction is still an edge.
//!
//!   sudo pipeline-probe [/dev/dri/card1] [/tmp/pipeline.h264]
//!
//! **The check is that the file decodes.** Everything up to here has been
//! verified against references we wrote ourselves; this is the first point
//! where something outside the project reads what the pipeline produced. It is
//! also the only thing that can confirm the frame layout, because the encoder
//! is handed one address and one row length and there is no way to ask it what
//! it made of them.
//!
//!   ffmpeg -i /tmp/pipeline.h264 -f null -
//!   ffmpeg -i /tmp/pipeline.h264 -frames:v 1 /tmp/pipeline.png

use std::io::Write;
use std::os::fd::IntoRawFd;
use std::path::PathBuf;

use lowlat_capture::convert::Converter;
use lowlat_capture::scanout::Card;
use lowlat_capture::vulkan::{Device, Imports, PlaneLayout};
use lowlat_encode::{Poll, cuda, nvenc};

/// How many pictures to encode. Enough that the encoder is past its first
/// picture and a decoder has something to be consistent about.
const FRAMES: usize = 30;

fn main() {
    let mut args = std::env::args().skip(1);
    let node = args
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let out = args
        .next()
        .map_or_else(|| PathBuf::from("/tmp/pipeline.h264"), PathBuf::from);

    let card = Card::open(&node).unwrap_or_else(|e| fail(&format!("open {node:?}: {e}")));
    let layout = card.scan().unwrap_or_else(|e| fail(&format!("scan: {e}")));
    let fb = &layout.primary;
    println!(
        "captured {}x{} {:?} modifier {:#018x}",
        fb.width,
        fb.height,
        fb.format,
        fb.modifier.map_or(0, u64::from)
    );

    let device = Device::for_display(&node).unwrap_or_else(|e| fail(&format!("device: {e}")));
    println!("converting on {}", device.name());

    let planes: Vec<PlaneLayout> = fb
        .planes()
        .map(|b| PlaneLayout {
            offset: b.offset,
            pitch: b.pitch,
        })
        .collect();
    let first = fb.planes().next().unwrap_or_else(|| fail("no buffers"));
    let fd = card
        .export(first)
        .unwrap_or_else(|e| fail(&format!("export capture: {e}")));
    let imported = device
        .import(&Imports {
            width: fb.width,
            height: fb.height,
            format: fb.format,
            modifier: fb.modifier.map_or(0, u64::from),
            fd: fd.into_raw_fd(),
            planes: &planes,
        })
        .unwrap_or_else(|e| fail(&format!("import capture: {e}")));

    let mut converter = Converter::new(&device).unwrap_or_else(|e| fail(&format!("pipeline: {e}")));
    let target = device
        .allocate_nv12(fb.width, fb.height)
        .unwrap_or_else(|e| fail(&format!("allocate: {e}")));

    // The vendor's compute interface has no name for a display-interface
    // descriptor, so the frame leaves the other way. The same allocation can
    // produce either.
    let (frame_fd, exported) = device
        .export_nv12(&target, false)
        .unwrap_or_else(|e| fail(&format!("export frame: {e}")));
    let bytes = u64::from(exported.pitch) * u64::from(exported.height) * 3 / 2;
    println!(
        "frame {}x{} pitch {}, colour at {}, {bytes} bytes",
        exported.width, exported.height, exported.pitch, exported.planes[1].offset
    );

    // The encoder runs on the same device the conversion did. Anything else is
    // the copy through system memory this path exists to avoid.
    let cuda = cuda::Cuda::load().unwrap_or_else(|e| fail(&format!("compute: {e}")));
    let compute_device = cuda
        .device(0)
        .unwrap_or_else(|e| fail(&format!("compute device: {e}")));
    let context = cuda
        .retain_primary(&compute_device)
        .unwrap_or_else(|e| fail(&format!("compute context: {e}")));
    context
        .make_current()
        .unwrap_or_else(|e| fail(&format!("make current: {e}")));

    // SAFETY: a context is current on this thread, the descriptor was exported
    // for the platform's opaque kind, and the size is the whole allocation.
    let external = unsafe { cuda.import(frame_fd, bytes) }
        .unwrap_or_else(|e| fail(&format!("take the frame: {e}")));
    let plane = external
        .plane(0, bytes, exported.pitch as usize)
        .unwrap_or_else(|e| fail(&format!("address the frame: {e}")));
    println!("frame taken by the encoder's runtime");

    let api = nvenc::Api::load().unwrap_or_else(|e| fail(&format!("encoder: {e}")));
    let session = api
        .open_session(context)
        .unwrap_or_else(|e| fail(&format!("session: {e}")));
    let mut encoder = session
        .initialize(
            &cuda,
            nvenc::Config {
                codec: nvenc::Codec::H264,
                width: exported.width,
                height: exported.height,
                fps: 60,
                bitrate_bps: 20_000_000,
                min_qp: nvenc::DEFAULT_MIN_QP,
            },
        )
        .unwrap_or_else(|e| fail(&format!("configure: {e}")));

    let input = encoder
        .register_ptr(plane.ptr(), plane.pitch())
        .unwrap_or_else(|e| fail(&format!("register: {e}")));
    println!("registered, encoding {FRAMES} pictures");

    let mut file = std::fs::File::create(&out).unwrap_or_else(|e| fail(&format!("create: {e}")));
    let mut written = 0usize;
    let mut collected = 0usize;
    let mut keyframes = 0usize;
    for at in 0..FRAMES {
        // Each picture is converted from the display as it is now, so the
        // stream is a real recording rather than one frame repeated.
        converter
            .run(&device, &imported, &target, false)
            .unwrap_or_else(|e| fail(&format!("convert: {e}")));
        if let Err(error) = encoder.submit_registered(&input, at == 0) {
            println!("submit refused at {at}: {error}");
            break;
        }
        loop {
            match encoder.poll() {
                Ok(Poll::Ready {
                    bitstream,
                    keyframe,
                }) => {
                    written += bitstream.len();
                    collected += 1;
                    if keyframe {
                        keyframes += 1;
                    }
                    file.write_all(bitstream)
                        .unwrap_or_else(|e| fail(&format!("write: {e}")));
                    break;
                }
                Ok(Poll::Pending) => std::thread::yield_now(),
                Err(error) => fail(&format!("collect: {error}")),
            }
        }
    }
    file.flush().ok();
    println!(
        "wrote {collected} pictures ({keyframes} of them standalone), {written} bytes, to {}",
        out.display()
    );

    converter.destroy(&device);
    device.release_nv12(target);
    device.release(imported);
}

fn fail(what: &str) -> ! {
    eprintln!("{what}");
    std::process::exit(1)
}
