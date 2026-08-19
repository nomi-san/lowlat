//! Conversion diagnostic. Captures the display, imports it, converts it, and
//! measures the round trip against the source.
//!
//!   sudo convert-probe [/dev/dri/card1] [/tmp/converted.ppm] [--dither]
//!
//! **Read the saturated figure, not the overall one.** A colour transform with
//! the wrong matrix still produces a plausible picture, so the source is read
//! twice -- once as captured, once through the conversion and back -- and the
//! difference reported. But a grey pixel has equal channels, every luma matrix
//! returns the same luma for it, and it carries no chroma at all; a dark
//! desktop is almost entirely such pixels, so the overall average cannot see
//! the matrix and says nothing about it.
//!
//! Measured on this machine against a desktop, with the shader deliberately
//! given the wrong coefficients as a control:
//!
//!   matrix    overall   saturated
//!   correct      0.17        2.64
//!   wrong        0.28        7.32
//!
//! So the overall figure moved by half and the saturated one by nearly three
//! times. Even that separation is thinner than it looks, because a desktop
//! offers few saturated pixels and most of them are text edges where chroma
//! subsampling dominates. **A synthetic pattern compared against a reference
//! computed on the processor is the check that would settle it**, and it needs
//! no display; this program is what can be run against a real one.

use std::io::{Seek, Write};
use std::os::fd::IntoRawFd;
use std::path::{Path, PathBuf};

use lowlat_capture::convert::Converter;
use lowlat_capture::scanout::Card;
use lowlat_capture::vulkan::{Device, Imports, PlaneLayout};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dither = args.iter().any(|arg| arg == "--dither");
    let mut positional = args.iter().filter(|arg| !arg.starts_with("--"));
    let node = positional
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let out = positional
        .next()
        .map_or_else(|| PathBuf::from("/tmp/converted.ppm"), PathBuf::from);

    let card = Card::open(&node).unwrap_or_else(|error| fail(&format!("open {node:?}: {error}")));
    let layout = card
        .scan()
        .unwrap_or_else(|error| fail(&format!("scan: {error}")));
    let fb = &layout.primary;
    println!(
        "captured {}x{} {:?} modifier {:#018x}",
        fb.width,
        fb.height,
        fb.format,
        fb.modifier.map_or(0, u64::from)
    );

    let device =
        Device::for_display(&node).unwrap_or_else(|error| fail(&format!("device: {error}")));
    println!("device {}, dither {}", device.name(), dither);

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
    let imported = device
        .import(&Imports {
            width: fb.width,
            height: fb.height,
            format: fb.format,
            modifier: fb.modifier.map_or(0, u64::from),
            fd: fd.into_raw_fd(),
            planes: &planes,
        })
        .unwrap_or_else(|error| fail(&format!("import: {error}")));

    // The source as captured, for the comparison below.
    let source = device
        .read_back(&imported)
        .unwrap_or_else(|error| fail(&format!("read back: {error}")));

    let converter =
        Converter::new(&device).unwrap_or_else(|error| fail(&format!("pipeline: {error}")));
    let target = device
        .allocate_nv12(fb.width, fb.height)
        .unwrap_or_else(|error| fail(&format!("allocate: {error}")));
    converter
        .run(&device, &imported, &target, dither)
        .unwrap_or_else(|error| fail(&format!("convert: {error}")));
    let (luma, chroma) = device
        .read_nv12(&target)
        .unwrap_or_else(|error| fail(&format!("read planes: {error}")));
    println!(
        "converted, {} luma and {} chroma bytes",
        luma.len(),
        chroma.len()
    );

    let width = target.width as usize;
    let height = target.height as usize;
    let rgb = to_rgb(&luma, &chroma, width, height);

    let (error, saturated, counted) =
        mean_error(&source, &rgb, fb.width as usize, fb.height as usize, width);
    println!("mean absolute error {error:.2} per channel out of 255");
    // **The overall figure cannot see the colour matrix.** A grey pixel has
    // equal channels, so every luma matrix returns the same luma and no chroma
    // at all; a dark desktop is nearly all such pixels and averages the
    // interesting ones away. This is the number that moves.
    println!("over saturated pixels only: {saturated:.2} across {counted} of them");

    // The same frame as an encoder will take it, at a real size, because the
    // padding a driver applies is not visible at test dimensions.
    match device.export_nv12(&target) {
        Ok((fd, exported)) => {
            let size = std::fs::File::from(fd)
                .seek(std::io::SeekFrom::End(0))
                .unwrap_or(0);
            println!(
                "exported {size} bytes, modifier {:#018x}, luma offset {} pitch {}, chroma offset {} pitch {}",
                exported.modifier,
                exported.planes[0].offset,
                exported.planes[0].pitch,
                exported.planes[1].offset,
                exported.planes[1].pitch
            );
            let naive = u64::from(exported.planes[0].pitch) * u64::from(exported.height);
            if u64::from(exported.planes[1].offset) != naive {
                println!(
                    "  chroma is NOT where pitch times height would put it ({naive}); \
                     an importer told the computed number reads the wrong bytes"
                );
            }
        }
        Err(error) => eprintln!("export failed: {error}"),
    }

    if let Err(why) = write_ppm(&out, width, height, &rgb) {
        eprintln!("cannot write {}: {why}", out.display());
    } else {
        println!("wrote {}", out.display());
    }

    converter.destroy(&device);
    device.release_nv12(target);
    device.release(imported);
}

fn fail(what: &str) -> ! {
    eprintln!("{what}");
    std::process::exit(1)
}

/// Undo the conversion, so the result can be compared with where it started.
///
/// The inverse of what the shader does, written out rather than shared with it,
/// because a mistake shared by both directions would cancel and prove nothing.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0.0..=1.0 and scaled by 255 on the same line"
)]
fn to_rgb(luma: &[u8], chroma: &[u8], width: usize, height: usize) -> Vec<u8> {
    const KR: f32 = 0.2126;
    const KB: f32 = 0.0722;
    let kg = 1.0 - KR - KB;

    let mut rgb = vec![0u8; width * height * 3];
    for y in 0..height {
        for x in 0..width {
            let luma_at = y * width + x;
            let chroma_at = (y / 2) * (width / 2 * 2) + (x / 2) * 2;
            let yv = (f32::from(*luma.get(luma_at).unwrap_or(&0)) - 16.0) / 219.0;
            let u = (f32::from(*chroma.get(chroma_at).unwrap_or(&128)) - 128.0) / 224.0;
            let v = (f32::from(*chroma.get(chroma_at + 1).unwrap_or(&128)) - 128.0) / 224.0;

            let r = yv + v * (2.0 - 2.0 * KR);
            let b = yv + u * (2.0 - 2.0 * KB);
            let g = (yv - KR * r - KB * b) / kg;

            let at = luma_at * 3;
            for (offset, value) in [r, g, b].into_iter().enumerate() {
                if let Some(slot) = rgb.get_mut(at + offset) {
                    *slot = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
        }
    }
    rgb
}

/// How far the round trip landed from where it started.
///
/// The source arrives packed four bytes to a pixel in whatever depth it was
/// captured at; only the top eight bits of each channel are compared, because
/// that is all the conversion was ever going to keep.
fn mean_error(
    source: &[u8],
    rgb: &[u8],
    width: usize,
    height: usize,
    stride: usize,
) -> (f64, f64, u64) {
    let mut total = 0u64;
    let mut counted = 0u64;
    let mut vivid_total = 0u64;
    let mut vivid_counted = 0u64;
    for y in 0..height {
        for x in 0..width {
            let at = (y * width + x) * 4;
            let Some(quad) = source.get(at..at + 4) else {
                continue;
            };
            let word = u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]);
            // Ten-bit packed and eight-bit byte order both reduce to the same
            // eight bits a channel, which is what the conversion kept.
            let (sr, sg, sb) = if width * height * 4 == source.len() && word > 0 {
                (
                    ((word & 0x3ff) >> 2) as u8,
                    ((word >> 10 & 0x3ff) >> 2) as u8,
                    ((word >> 20 & 0x3ff) >> 2) as u8,
                )
            } else {
                (quad[0], quad[1], quad[2])
            };
            let out = (y * stride + x) * 3;
            let Some(trio) = rgb.get(out..out + 3) else {
                continue;
            };
            // A pixel whose channels are far apart is one the matrix acts on.
            let high = sr.max(sg).max(sb);
            let low = sr.min(sg).min(sb);
            let vivid = high.abs_diff(low) > 60;
            for (a, b) in [sr, sg, sb].into_iter().zip(trio.iter().copied()) {
                let apart = u64::from(a.abs_diff(b));
                total += apart;
                counted += 1;
                if vivid {
                    vivid_total += apart;
                    vivid_counted += 1;
                }
            }
        }
    }
    let overall = if counted == 0 {
        f64::NAN
    } else {
        total as f64 / counted as f64
    };
    let vivid = if vivid_counted == 0 {
        f64::NAN
    } else {
        vivid_total as f64 / vivid_counted as f64
    };
    (overall, vivid, vivid_counted / 3)
}

fn write_ppm(path: &Path, width: usize, height: usize, rgb: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P6\n{width} {height}\n255\n")?;
    out.write_all(rgb)?;
    out.flush()
}
