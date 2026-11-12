//! The display's path on Linux without a display, through the open stack: a
//! moving picture converted on the card behind the render node into the open
//! encoder's own input, imported without a copy, encoded, and decoded by the
//! open decoder, each picture compared with what the conversion wrote. Once
//! untiled, and again in Intel's Y tiling where the encoder is the one that
//! reads it and the card writes it; and twice on one context where the driver
//! keeps its contexts, as a stream switching size and back takes one again.
//! Needs the open-stack driver, so off by
//! default: `cargo test -p lowlat-decode --test vaapi_round_trip -- --ignored
//! --nocapture`, with `LOWLAT_VAAPI_NODE` naming the render node.

#![cfg(target_os = "linux")]
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::ffi::CString;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use lowlat_capture::convert::{Converter, Depth, Nv12, Y_TILED};
use lowlat_capture::vulkan::Device;
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::vaapi::Backend;
use lowlat_decode::{Decoder, Planes};
use lowlat_drivers::va::Vaapi;
use lowlat_encode::vaapi::{self, Display, Params};
use lowlat_encode::{Poll, h264};

/// 1080 rows is not a whole number of tile rows, so a descriptor that put the
/// colour one picture-height in, or stopped at the last row of pixels, cannot
/// pass.
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// Long enough that a reference read wrongly has pictures to carry it into.
const PICTURES: usize = 30;
/// The worst picture allowed, in dB against what was fed. A picture read in
/// the wrong layout is noise, far below this; one read right and coded at this
/// rate is above it on every encoder measured, the lowest a first picture at
/// 38 dB.
const FLOOR_DB: f64 = 30.0;

fn node() -> CString {
    let named = std::env::var("LOWLAT_VAAPI_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    CString::new(named).unwrap()
}

/// The display node of the card a render node belongs to, as the kernel lists
/// the two side by side.
fn card_of(render: &CString) -> Option<PathBuf> {
    let name = Path::new(render.to_str().ok()?).file_name()?;
    std::fs::read_dir(Path::new("/sys/class/drm").join(name).join("device/drm"))
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|node| node.starts_with("card"))
        .map(|node| Path::new("/dev/dri").join(node))
}

/// Saturated blocks that move a pixel a picture, with grey nowhere: every
/// block carries colour, and the motion gives the predicted pictures
/// something to describe.
fn pattern(frame: usize) -> Vec<u8> {
    const COLOURS: [[u8; 3]; 6] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [0, 255, 255],
        [255, 0, 255],
    ];
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let mut pixels = vec![255u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let colour = COLOURS[((x + frame) / 64 + (y / 64) * 5) % COLOURS.len()];
            pixels[(y * w + x) * 4..(y * w + x) * 4 + 3].copy_from_slice(&colour);
        }
    }
    pixels
}

/// Peak signal to noise of one plane against what was fed, in dB.
fn psnr(fed: &[u8], decoded: &[u8], pitch: usize, width: usize, rows: usize) -> f64 {
    let mut squared = 0u64;
    for row in 0..rows {
        let got = &decoded[row * pitch..row * pitch + width];
        let want = &fed[row * width..row * width + width];
        for (a, b) in got.iter().zip(want) {
            let d = u64::from(a.abs_diff(*b));
            squared += d * d;
        }
    }
    if squared == 0 {
        return f64::INFINITY;
    }
    let mean = squared as f64 / (width * rows) as f64;
    10.0 * (255.0 * 255.0 / mean).log10()
}

/// Convert, import, encode on `context` and decode every picture through one
/// target, and return each decoded picture's luma and colour figures against
/// what the conversion wrote.
fn round_trip(
    device: &Device,
    display: &Display<'_>,
    decoding: &lowlat_drivers::va::Display<'_>,
    context: &vaapi::Context<'_>,
    target: &Nv12,
) -> Vec<(f64, f64)> {
    let mut converter = Converter::new(device).expect("a pipeline");
    let (fd, exported) = device.export_nv12(target, true).expect("export the frame");
    let surface = display
        .import(fd.as_fd(), &exported)
        .expect("import the frame");
    let mut encoder = context
        .encoder(
            Params::H264(h264::Params {
                width: WIDTH,
                height: HEIGHT,
                fps: 60,
                level_idc: 42,
                log2_max_frame_num_minus4: 4,
                log2_max_poc_lsb_minus4: 4,
                max_num_ref_frames: 1,
            }),
            20_000_000,
        )
        .expect("encoder");

    let mut fed = Vec::with_capacity(PICTURES);
    let mut units = Vec::with_capacity(PICTURES);
    for frame in 0..PICTURES {
        let source = device
            .upload_rgba(WIDTH, HEIGHT, &pattern(frame))
            .expect("upload the pattern");
        converter
            .run(device, &source, &target.target(), false)
            .expect("convert");
        device.release(source);
        fed.push(device.read_nv12(target).expect("read back what was fed"));
        encoder
            .submit_registered(surface, frame == 0)
            .expect("submit");
        loop {
            match encoder.poll().expect("poll") {
                Poll::Ready { bitstream, .. } => {
                    units.push(bitstream.to_vec());
                    break;
                }
                Poll::Pending => std::hint::spin_loop(),
            }
        }
    }
    drop(encoder);
    display.release(surface);
    converter.destroy(device);

    let mut backend = Backend::new(decoding, (4096, 4096));
    backend
        .build(&VideoHeader {
            frame_id: 1,
            width: 0,
            height: 0,
            codec: Codec::H264,
            rotation: Rotation::None,
            ten_bit: false,
            locked: false,
            announced: false,
            metadata: false,
        })
        .expect("build");
    let pitch = WIDTH as usize + 64;
    let (mut y, mut uv, mut v) = (
        vec![0u8; pitch * HEIGHT as usize],
        vec![0u8; pitch * HEIGHT as usize],
        vec![0u8; pitch * HEIGHT as usize],
    );
    let mut figures = Vec::with_capacity(PICTURES);
    for (n, unit) in units.iter().enumerate() {
        backend
            .feed(unit)
            .unwrap_or_else(|e| panic!("unit {n}: {e:?}"));
        if n + 1 == units.len() {
            backend.drain();
        }
        loop {
            let mut planes = Planes {
                y: &mut y,
                y_pitch: pitch,
                uv: &mut uv,
                uv_pitch: pitch,
                v: &mut v,
                v_pitch: pitch,
            };
            let Some(picture) = backend.take(&mut planes).expect("take") else {
                break;
            };
            assert_eq!(
                (picture.width, picture.height),
                (WIDTH, HEIGHT),
                "a picture of the wrong size"
            );
            // Nothing reorders here, so the n-th picture out is the n-th fed.
            let (luma, colour) = &fed[figures.len()];
            figures.push((
                psnr(luma, &y, pitch, WIDTH as usize, HEIGHT as usize),
                psnr(colour, &uv, pitch, WIDTH as usize, HEIGHT as usize / 2),
            ));
        }
    }
    backend.destroy();
    assert_eq!(figures.len(), PICTURES, "pictures went missing");
    figures
}

/// **What was converted is what comes back**, picture for picture, whichever
/// layout the encoder was handed.
#[test]
#[ignore = "requires the open-stack driver and its display interface"]
fn the_converted_picture_comes_back_through_the_open_encoder() {
    let va = Vaapi::load().expect("runtime");
    let display = Display::open(&va, &node()).expect("render node");
    let decoding = va.open(&node()).expect("render node");
    let caps = match display.caps(vaapi::Codec::H264) {
        Err(vaapi::Error::NoEncoder) => {
            println!("this device offers no H.264 encoder; nothing to round-trip");
            return;
        }
        caps => caps.expect("caps"),
    };
    let card = card_of(&node()).expect("the card behind the render node");
    let device = Device::for_display(&card).expect("the card behind the render node opens");

    let mut legs = vec![("untiled", None)];
    if display.wants_y_tiled() && device.tiles(Depth::Eight, Y_TILED) {
        legs.push(("Y-tiled", Some(Y_TILED)));
    } else {
        println!(
            "{}: the tiled leg not run (the encoder reads it: {}, the card writes it: {})",
            device.name(),
            display.wants_y_tiled(),
            device.tiles(Depth::Eight, Y_TILED)
        );
    }
    for (name, tiling) in legs {
        let target = match tiling {
            Some(modifier) => device.allocate_planar_tiled(WIDTH, HEIGHT, Depth::Eight, modifier),
            None => device.allocate_planar(WIDTH, HEIGHT, Depth::Eight),
        }
        .expect("a target");
        let context = display
            .create_context(caps, WIDTH, HEIGHT, 2)
            .expect("context");
        let figures = round_trip(&device, &display, &decoding, &context, &target);
        drop(context);
        device.release_nv12(target);
        judge(
            &format!("{} ({}) {name}", device.name(), card.display()),
            &figures,
        );
    }
}

/// Print a leg's figures and fail it on any picture under the floor.
fn judge(leg: &str, figures: &[(f64, f64)]) {
    let worst = figures.iter().fold((f64::INFINITY, f64::INFINITY), |w, f| {
        (w.0.min(f.0), w.1.min(f.1))
    });
    let mean = figures.iter().map(|f| f.0.min(99.0)).sum::<f64>() / figures.len() as f64;
    println!(
        "{leg}: {} pictures, luma worst {:.1} dB mean {mean:.1}, colour worst {:.1} dB",
        figures.len(),
        worst.0,
        worst.1
    );
    for (n, (luma, colour)) in figures.iter().enumerate() {
        assert!(
            *luma >= FLOOR_DB && *colour >= FLOOR_DB,
            "{leg}: picture {n} came back at {luma:.1} dB luma, {colour:.1} dB colour"
        );
    }
}

/// **A kept context codes its next stream as well as its first**, with the
/// first stream's encoder and imported picture gone and a context of another
/// size built in between, as a switch of the display's size and back leaves
/// it. Run where the driver keeps its contexts; elsewhere nothing is taken
/// again.
#[test]
#[ignore = "requires the open-stack driver and its display interface"]
fn a_kept_context_codes_its_next_stream_as_well_as_its_first() {
    let va = Vaapi::load().expect("runtime");
    let display = Display::open(&va, &node()).expect("render node");
    let decoding = va.open(&node()).expect("render node");
    let caps = match display.caps(vaapi::Codec::H264) {
        Err(vaapi::Error::NoEncoder) => {
            println!("this device offers no H.264 encoder; nothing to round-trip");
            return;
        }
        caps => caps.expect("caps"),
    };
    if !display.keeps_contexts(caps) {
        println!("this driver destroys its contexts; none is kept to take again");
        return;
    }
    let card = card_of(&node()).expect("the card behind the render node");
    let device = Device::for_display(&card).expect("the card behind the render node opens");
    let target = device
        .allocate_planar(WIDTH, HEIGHT, Depth::Eight)
        .expect("a target");

    let context = display
        .create_context(caps, WIDTH, HEIGHT, 2)
        .expect("context");
    assert!(context.kept(), "a context the driver keeps reports it");
    let first = round_trip(&device, &display, &decoding, &context, &target);
    let other = display
        .create_context(caps, 1280, 720, 2)
        .expect("a context of another size");
    assert!(context.fits(caps, WIDTH, HEIGHT, 2));
    assert!(
        !other.fits(caps, WIDTH, HEIGHT, 2),
        "a size is part of the fit"
    );
    let second = round_trip(&device, &display, &decoding, &context, &target);
    device.release_nv12(target);

    judge(&format!("{} first stream", device.name()), &first);
    judge(
        &format!("{} the same context again", device.name()),
        &second,
    );
}
