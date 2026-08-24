//! Conversion diagnostic for the other display interface.
//!
//!   gl-probe [/dev/dri/card1]
//!
//! **Runs the whole conversion without a display.** A picture with known
//! colours goes up, comes back through the conversion, and is compared against
//! the same transform computed here from the definitions. That is the check a
//! desktop cannot give: a real desktop is nearly all grey, and every luma
//! matrix agrees on grey.
//!
//! What it cannot check is the import of a real scanout buffer, which is the
//! one question left and needs a display and the elevated capability.

use std::path::PathBuf;

use lowlat_capture::gl::{Converter, Device};

/// The colour transform, computed here from the rules rather than transcribed
/// from the shader, so a mistake cannot be shared by both.
fn reference(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (rf, gf, bf) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let kr = 0.2126_f64;
    let kb = 0.0722_f64;
    let kg = 1.0 - kr - kb;
    let y = kr * rf + kg * gf + kb * bf;
    let u = (bf - y) / (2.0 - 2.0 * kb);
    let v = (rf - y) / (2.0 - 2.0 * kr);
    let quantise = |value: f64| -> u8 {
        let scaled = (value * 255.0).round().clamp(0.0, 255.0);
        (0..=u8::MAX)
            .find(|candidate| f64::from(*candidate) >= scaled)
            .unwrap_or(u8::MAX)
    };
    (
        quantise(y * (219.0 / 255.0) + 16.0 / 255.0),
        quantise(u * (224.0 / 255.0) + 128.0 / 255.0),
        quantise(v * (224.0 / 255.0) + 128.0 / 255.0),
    )
}

/// Saturated colours, each filling a whole 2x2 block so subsampling has nothing
/// to average. Grey is deliberately not the whole list: it carries no chroma
/// and passes with the wrong coefficients.
const PATTERN: [[u8; 3]; 8] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 255, 255],
    [0, 0, 0],
];

fn main() {
    let node = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);

    let device = match Device::for_display(&node) {
        Ok(device) => device,
        Err(error) => {
            eprintln!("{}: {error}", node.display());
            std::process::exit(2);
        }
    };
    println!("{}: {}", node.display(), device.name());

    let width = u32::try_from(PATTERN.len()).unwrap_or(1) * 2;
    let height = 2u32;
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    for (at, colour) in PATTERN.iter().enumerate() {
        for y in 0..2usize {
            for x in 0..2usize {
                let base = ((y * (width as usize)) + at * 2 + x) * 4;
                pixels[base] = colour[0];
                pixels[base + 1] = colour[1];
                pixels[base + 2] = colour[2];
                pixels[base + 3] = 255;
            }
        }
    }

    let converter = Converter::new(&device).expect("a pipeline");
    let source = device.upload_rgba(width, height, &pixels).expect("upload");
    let target = device.allocate_nv12(width, height).expect("a target");
    let digest = converter
        .run(&device, &source, &target, false)
        .expect("convert");
    let (luma, chroma) = device.read_nv12(&target).expect("read back");

    println!("digest {:#018x}", digest.0);
    let mut worst = 0i32;
    for (at, colour) in PATTERN.iter().enumerate() {
        let (y, u, v) = reference(colour[0], colour[1], colour[2]);
        let got_y = luma[at * 2];
        let got_u = chroma[at * 2];
        let got_v = chroma[at * 2 + 1];
        let off = [
            i32::from(got_y) - i32::from(y),
            i32::from(got_u) - i32::from(u),
            i32::from(got_v) - i32::from(v),
        ];
        worst = worst.max(off.iter().map(|d| d.abs()).max().unwrap_or(0));
        println!(
            "  {:>3},{:>3},{:>3}  want {:>3},{:>3},{:>3}  got {:>3},{:>3},{:>3}",
            colour[0], colour[1], colour[2], y, u, v, got_y, got_u, got_v
        );
    }
    println!("worst channel error {worst}");

    device.release(source);
    device.release_nv12(target);
    converter.destroy(&device);
}
