//! Import diagnostic. Captures the display once, imports it without a copy,
//! and writes what came back as an image file.
//!
//!   sudo import-probe [/dev/dri/card1] [/tmp/scanout.ppm]
//!
//! **The picture is the check.** An import whose tiling is described wrongly
//! still succeeds at every call and returns a buffer of the right size; the
//! only thing that says it was read correctly is that it looks like a desktop
//! rather than like noise or diagonal smears. Channel order fails the same way,
//! more subtly: red and blue exchanged looks plausible until something in the
//! picture is a known colour.

use std::io::Write;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::path::{Path, PathBuf};

use lowlat_capture::scanout::Card;
use lowlat_capture::vulkan::{Device, Imports, PlaneLayout};

fn main() {
    let mut args = std::env::args().skip(1);
    let node = args
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let out = args
        .next()
        .map_or_else(|| PathBuf::from("/tmp/scanout.ppm"), PathBuf::from);

    let card = match Card::open(&node) {
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
    let device = match Device::for_display(&node) {
        Ok(device) => device,
        Err(error) => {
            eprintln!("cannot open the display's device: {error}");
            std::process::exit(2);
        }
    };
    println!("device {}", device.name());

    // Both planes are imported, not just the display's. The pointer is a
    // different pixel layout under a different modifier -- eight bit, blue
    // first, untiled -- so it exercises a path the display's own buffer never
    // reaches.
    let mut targets = vec![(&layout.primary, out.clone())];
    if let Some(cursor) = layout.cursor.as_ref() {
        targets.push((&cursor.image, out.with_extension("cursor.ppm")));
    }

    for (fb, out) in targets {
        dump(&card, &device, fb, &out);
    }
}

fn dump(card: &Card, device: &Device, fb: &lowlat_capture::scanout::Framebuffer, out: &Path) {
    println!(
        "captured {}x{} {:?} modifier {:#018x}, {} buffer(s)",
        fb.width,
        fb.height,
        fb.format,
        fb.modifier.map_or(0, u64::from),
        fb.planes().count()
    );

    // Several distinct descriptors are not handled; see Imports. Nothing seen
    // here scans out that way, and saying so beats guessing at the arrangement.
    let planes: Vec<PlaneLayout> = fb
        .planes()
        .map(|buffer| PlaneLayout {
            offset: buffer.offset,
            pitch: buffer.pitch,
        })
        .collect();
    let Some(first) = fb.planes().next() else {
        eprintln!("no buffers");
        return;
    };
    let fd = match card.export(first) {
        Ok(fd) => fd,
        Err(error) => {
            eprintln!("cannot export: {error}");
            return;
        }
    };
    println!("exported descriptor {}", fd.as_raw_fd());

    let imported = match device.import(&Imports {
        width: fb.width,
        height: fb.height,
        format: fb.format,
        modifier: fb.modifier.map_or(0, u64::from),
        // The interface takes ownership on success, so the descriptor is
        // released here rather than closed by us.
        fd: fd.into_raw_fd(),
        planes: &planes,
    }) {
        Ok(imported) => imported,
        Err(error) => {
            eprintln!("import failed: {error}");
            return;
        }
    };
    println!("imported {imported:?}");

    let pixels = match device.read_back(&imported) {
        Ok(pixels) => pixels,
        Err(error) => {
            eprintln!("read back failed: {error}");
            device.release(imported);
            return;
        }
    };
    device.release(imported);

    match write_ppm(out, fb.width, fb.height, fb.format, &pixels) {
        Ok(()) => println!("wrote {}", out.display()),
        Err(error) => eprintln!("cannot write {}: {error}", out.display()),
    }
}

/// Write eight-bit RGB, whatever came in.
///
/// The two packed layouts differ in more than depth, so each is unpacked
/// explicitly rather than through a shared path that would have to branch
/// anyway.
fn write_ppm(
    path: &Path,
    width: u32,
    height: u32,
    format: drm::buffer::DrmFourcc,
    pixels: &[u8],
) -> std::io::Result<()> {
    use drm::buffer::DrmFourcc;

    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P6\n{width} {height}\n255\n")?;

    let mut row = Vec::with_capacity((width as usize) * 3);
    for y in 0..height as usize {
        row.clear();
        for x in 0..width as usize {
            let at = (y * width as usize + x) * 4;
            let Some(quad) = pixels.get(at..at + 4) else {
                continue;
            };
            let (r, g, b) = match format {
                // Ten bits a channel in one little-endian word, red lowest.
                // Shifted down rather than scaled: this is a diagnostic and the
                // two low bits do not decide whether the import worked.
                DrmFourcc::Abgr2101010 | DrmFourcc::Xbgr2101010 => {
                    let word = u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]);
                    (
                        u8::try_from((word & 0x3ff) >> 2).unwrap_or(0),
                        u8::try_from((word >> 10 & 0x3ff) >> 2).unwrap_or(0),
                        u8::try_from((word >> 20 & 0x3ff) >> 2).unwrap_or(0),
                    )
                }
                // Bytes in memory are blue, green, red, alpha.
                DrmFourcc::Argb8888 | DrmFourcc::Xrgb8888 => (quad[2], quad[1], quad[0]),
                // Bytes in memory are red, green, blue, alpha.
                _ => (quad[0], quad[1], quad[2]),
            };
            row.extend_from_slice(&[r, g, b]);
        }
        out.write_all(&row)?;
    }
    out.flush()
}
