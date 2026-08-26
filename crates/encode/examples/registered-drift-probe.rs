//! Drift on the path the stream loop actually uses: surfaces we allocated,
//! exported and registered, rather than bytes the driver copied in for us.
//!
//!   registered-drift-probe [/dev/dri/card0] [pictures] [ring] [codec]
//!
//! **Every input picture is the same picture**, because one source is
//! converted into every target in the ring and nothing writes them again.
//!
//! **The obvious reading of that is wrong, and this is the probe's real
//! lesson.** Identical input does not mean identical output: the rate
//! controller moves the quantizer over the run, so the pictures differ from
//! each other for a reason that is not drift. Measured here, every
//! combination of card and codec rises and then sits flat -- the shape of a
//! controller settling, not of a reconstruction diverging. A run that stops
//! inside the ramp reads that as drift and is wrong. Read the trajectory in
//! tenths and treat the plateau as the answer.
//!
//! So what this probe bounds is drift *on the registered path* under an
//! unchanging picture, and the bound it reports is: none, on either card.
//! It cannot see the fault a live stream shows, because **ghosting needs the
//! picture to change** -- it is stale content surviving where new content
//! belongs, and there is no new content here. For a changing source use
//! `drift-probe`, which submits from ordinary memory and compares against
//! what it submitted.
//!
//!   ffmpeg -i /tmp/rd.265 -pix_fmt nv12 -f rawvideo /tmp/rd.nv12
//!   (then compare each picture against the first)

use std::os::fd::AsFd;
use std::path::PathBuf;

use lowlat_capture::convert::{Converter, Nv12};
use lowlat_capture::vulkan::Device;
use lowlat_encode::{Poll, vaapi};

fn fail(what: &str) -> ! {
    eprintln!("registered-drift-probe: {what}");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let node = args
        .first()
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);
    let pictures: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(60);
    let ring: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(4);
    let hevc = args.get(3).is_none_or(|c| c == "h265" || c == "hevc");
    let (width, height) = (1920u32, 1080u32);

    let device = Device::for_display(&node).unwrap_or_else(|e| fail(&format!("device: {e}")));
    let mut converter = Converter::new(&device).unwrap_or_else(|e| fail(&format!("pipeline: {e}")));

    // **A picture with detail in it**, because a flat one hides a reconstruction
    // error and this probe exists to expose one.
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let at = (y * width as usize + x) * 4;
            pixels[at] = u8::try_from((x * 7 + y * 3) % 251).unwrap_or(0);
            pixels[at + 1] = u8::try_from((x ^ y) % 253).unwrap_or(0);
            pixels[at + 2] = u8::try_from((x / 3 + y * 5) % 249).unwrap_or(0);
            pixels[at + 3] = 255;
        }
    }
    let source = device
        .upload_rgba(width, height, &pixels)
        .unwrap_or_else(|e| fail(&format!("upload: {e}")));

    // One source into every target, so the ring holds one picture N times.
    let targets: Vec<Nv12> = (0..ring)
        .map(|_| {
            device
                .allocate_nv12(width, height)
                .unwrap_or_else(|e| fail(&format!("allocate: {e}")))
        })
        .collect();
    for target in &targets {
        converter
            .run(&device, &source, &target.target(), false)
            .unwrap_or_else(|e| fail(&format!("convert: {e}")));
    }

    let render = node
        .to_str()
        .map(|p| p.replace("card", "renderD"))
        .map(|p| {
            // card0 -> renderD128, card1 -> renderD129
            let n: u32 = p
                .rsplit("renderD")
                .next()
                .and_then(|d| d.parse().ok())
                .unwrap_or(0);
            format!("/dev/dri/renderD{}", 128 + n)
        })
        .unwrap_or_else(|| "/dev/dri/renderD128".into());
    let va = vaapi::Vaapi::load().unwrap_or_else(|e| fail(&format!("runtime: {e:?}")));
    let display = va
        .open(&std::ffi::CString::new(render.clone()).unwrap())
        .unwrap_or_else(|e| fail(&format!("render node {render}: {e:?}")));
    let codec = if hevc {
        vaapi::Codec::H265
    } else {
        vaapi::Codec::H264
    };
    let caps = display
        .caps(codec)
        .unwrap_or_else(|e| fail(&format!("caps: {e:?}")));
    let context = display
        .create_context(caps, width, height, ring)
        .unwrap_or_else(|e| fail(&format!("context: {e:?}")));

    let mut registered = Vec::with_capacity(targets.len());
    let mut _held = Vec::with_capacity(targets.len());
    for target in &targets {
        let (fd, exported) = device
            .export_nv12(target, true)
            .unwrap_or_else(|e| fail(&format!("export: {e}")));
        registered.push(
            display
                .import(fd.as_fd(), &exported)
                .unwrap_or_else(|e| fail(&format!("register: {e:?}"))),
        );
        _held.push(fd);
    }

    let params = if hevc {
        vaapi::Params::H265(lowlat_encode::h265::Params {
            width,
            height,
            fps: 60,
            level_idc: 123,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
            transform_depth: lowlat_encode::h265::TRANSFORM_HIERARCHY_DEPTH,
        })
    } else {
        vaapi::Params::H264(lowlat_encode::h264::Params {
            width,
            height,
            fps: 60,
            level_idc: 42,
            log2_max_frame_num_minus4: 4,
            log2_max_poc_lsb_minus4: 4,
            max_num_ref_frames: 1,
        })
    };
    let mut encoder = context
        .encoder(params, 20_000_000)
        .unwrap_or_else(|e| fail(&format!("encoder: {e:?}")));

    let ext = if hevc { "265" } else { "264" };
    let path = format!("/tmp/rd.{ext}");
    let mut out = std::fs::File::create(&path).unwrap_or_else(|e| fail(&format!("create: {e}")));
    use std::io::Write;

    let (mut submitted, mut collected) = (0usize, 0usize);
    while collected < pictures {
        if submitted < pictures && encoder.in_flight() < ring.min(2) {
            encoder
                .submit_registered(registered[submitted % registered.len()], submitted == 0)
                .unwrap_or_else(|e| fail(&format!("submit: {e:?}")));
            submitted += 1;
        }
        match encoder
            .poll()
            .unwrap_or_else(|e| fail(&format!("poll: {e:?}")))
        {
            Poll::Ready { bitstream, .. } => {
                out.write_all(bitstream).unwrap();
                collected += 1;
            }
            Poll::Pending => std::hint::spin_loop(),
        }
    }
    println!("{path}: {pictures} identical pictures of {width}x{height} on {render}");
}
