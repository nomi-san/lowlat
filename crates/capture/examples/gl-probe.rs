//! Conversion diagnostic for the other display interface.
//!
//!   gl-probe [/dev/dri/card1]
//!   sudo gl-probe --capture [/dev/dri/card1] [/tmp/gl.ppm]
//!
//! **Runs the whole conversion without a display.** A picture with known
//! colours goes up, comes back through the conversion, and is compared against
//! the same transform computed here from the definitions. That is the check a
//! desktop cannot give: a real desktop is nearly all grey, and every luma
//! matrix agrees on grey.
//!
//! **`--capture` is the other half, and the one that decides everything.** It
//! imports what the display is actually scanning out -- tiled or compressed,
//! under a vendor modifier, possibly several buffers, and usually ten bit -- and
//! converts that. A colour transform that is exact on an uploaded picture says
//! nothing about whether this interface can read a real framebuffer, which is
//! the only question this backend exists to answer. It needs a display and the
//! elevated capability.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use lowlat_capture::gl::{Converter, Device, Nv12};
use lowlat_capture::scanout::Card;
use lowlat_capture::vulkan::{Imports, PlaneLayout};

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let capture = args.iter().any(|arg| arg == "--capture");
    let mut positional = args.iter().filter(|arg| !arg.starts_with("--"));
    let node = positional
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let out = positional
        .next()
        .map_or_else(|| PathBuf::from("/tmp/gl.ppm"), PathBuf::from);

    let device = match Device::for_display(&node) {
        Ok(device) => device,
        Err(error) => {
            eprintln!("{}: {error}", node.display());
            std::process::exit(2);
        }
    };
    println!("{}: {}", node.display(), device.name());
    let converter = Converter::new(&device).expect("a pipeline");

    if capture {
        from_display(&device, &converter, &node, &out);
    } else {
        from_a_known_picture(&device, &converter);
    }
    converter.destroy(&device);
}

/// Convert what the display is scanning out, and write it where it can be
/// looked at.
///
/// **The picture is the check.** An import whose tiling is described wrongly
/// succeeds at every call and returns a buffer of the right size; only the
/// result looking like a desktop rather than like diagonal smears says it was
/// read correctly.
fn from_display(device: &Device, converter: &Converter, node: &Path, out: &Path) {
    let card = match Card::open(node) {
        Ok(card) => card,
        Err(error) => {
            eprintln!("cannot open {}: {error}", node.display());
            std::process::exit(2);
        }
    };
    let layout = match card.scan() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("cannot scan {}: {error}", node.display());
            std::process::exit(2);
        }
    };
    let fb = &layout.primary;
    println!(
        "scanout {}x{} {:?} modifier {:#018x}, {} buffer(s)",
        fb.width,
        fb.height,
        fb.format,
        fb.modifier.map_or(0, u64::from),
        fb.planes().count()
    );

    let planes: Vec<PlaneLayout> = fb
        .planes()
        .map(|buffer| PlaneLayout {
            offset: buffer.offset,
            pitch: buffer.pitch,
        })
        .collect();
    for (at, plane) in planes.iter().enumerate() {
        println!(
            "  buffer {at}: offset {} pitch {}",
            plane.offset, plane.pitch
        );
    }
    let Some(first) = fb.planes().next() else {
        eprintln!("no buffers");
        std::process::exit(2);
    };
    let fd = match card.export(first) {
        Ok(fd) => fd,
        Err(error) => {
            eprintln!("cannot export: {error}");
            std::process::exit(2);
        }
    };

    // **The descriptor stays ours here.** This interface duplicates what it
    // needs rather than taking ownership, which is the opposite of the other
    // one and would leak a descriptor a frame if it were assumed either way.
    let imported = match device.import(&Imports {
        width: fb.width,
        height: fb.height,
        format: fb.format,
        modifier: fb.modifier.map_or(0, u64::from),
        fd: fd.as_raw_fd(),
        planes: &planes,
    }) {
        Ok(imported) => imported,
        Err(error) => {
            eprintln!("import failed: {error}");
            std::process::exit(1);
        }
    };
    drop(fd);
    println!("imported {imported:?}");

    // **The target is allocated outside and imported**, which is the path the
    // product takes: a target the driver allocated cannot be handed to an
    // encoder. Allocated as a single untiled region tall enough for both
    // planes, so the colour plane begins exactly one luma plane in.
    let height = fb.height.next_multiple_of(2);
    let (linear, target_fd) =
        match card.allocate_linear(fb.width.next_multiple_of(2), height / 2 * 3) {
            Ok(allocated) => allocated,
            Err(error) => {
                eprintln!("cannot allocate a target: {error}");
                std::process::exit(1);
            }
        };
    println!("target pitch {}", linear.pitch);
    let target = match device.import_nv12(
        std::os::fd::AsRawFd::as_raw_fd(&target_fd),
        fb.width,
        fb.height,
        linear.pitch,
    ) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("cannot import the target: {error}");
            std::process::exit(1);
        }
    };
    let digest = match converter.run(device, &imported, &target, false) {
        Ok(digest) => digest,
        Err(error) => {
            eprintln!("convert failed: {error}");
            std::process::exit(1);
        }
    };
    println!("digest {:#018x}", digest.0);

    match device.read_nv12(&target) {
        Ok((luma, chroma)) => match write_ppm(out, &target, &luma, &chroma) {
            Ok(()) => println!("wrote {}", out.display()),
            Err(error) => eprintln!("cannot write {}: {error}", out.display()),
        },
        Err(error) => eprintln!("read back failed: {error}"),
    }

    device.release(imported);
    device.release_nv12(target);
    drop(target_fd);
    card.release_linear(linear);
}

/// Write the converted frame as eight-bit colour, undoing the transform.
///
/// The inverse is written from the same definitions as the forward one rather
/// than derived from it, so a picture that comes out looking right is evidence
/// about the import rather than about the arithmetic cancelling.
fn write_ppm(path: &Path, target: &Nv12, luma: &[u8], chroma: &[u8]) -> std::io::Result<()> {
    let width = target.width as usize;
    let height = target.height as usize;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(file, "P6\n{width} {height}\n255\n")?;

    let mut row = vec![0_u8; width * 3];
    for y in 0..height {
        for x in 0..width {
            let yy = (f64::from(luma[y * width + x]) - 16.0) / 219.0;
            let at = (y / 2) * width + (x / 2) * 2;
            let u = (f64::from(chroma[at]) - 128.0) / 224.0;
            let v = (f64::from(chroma[at + 1]) - 128.0) / 224.0;
            let kr = 0.2126_f64;
            let kb = 0.0722_f64;
            let kg = 1.0 - kr - kb;
            let r = yy + (2.0 - 2.0 * kr) * v;
            let b = yy + (2.0 - 2.0 * kb) * u;
            let g = (yy - kr * r - kb * b) / kg;
            for (at, value) in [r, g, b].into_iter().enumerate() {
                let scaled = (value * 255.0).round().clamp(0.0, 255.0);
                row[x * 3 + at] = (0..=u8::MAX)
                    .find(|candidate| f64::from(*candidate) >= scaled)
                    .unwrap_or(u8::MAX);
            }
        }
        file.write_all(&row)?;
    }
    file.flush()
}

/// Convert a picture whose answer is known, which needs no display.
fn from_a_known_picture(device: &Device, converter: &Converter) {
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

    let source = device.upload_rgba(width, height, &pixels).expect("upload");
    let target = device.allocate_nv12(width, height).expect("a target");
    let digest = converter
        .run(device, &source, &target, false)
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
}
