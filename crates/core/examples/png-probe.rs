//! Write a picture and let something outside the project judge it.
//!
//!   png-probe /tmp/probe.png
//!
//! **A PNG writer that only its own reader accepts is worth nothing here**, and
//! nothing in this crate can decode one. The output is checked by whatever the
//! machine has: a decoder that reads it, reports the size we asked for, and
//! shows the pattern we drew.
use std::io::Write;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or("/tmp/probe.png".to_string());
    let (width, height) = (37u32, 23u32);

    // Deliberately not square, not a multiple of anything, and with a
    // transparent border: a writer that confuses rows with columns, drops the
    // filter marker, or mishandles the last partial block produces something
    // visibly wrong rather than subtly wrong.
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let at = ((y * width + x) * 4) as usize;
            let edge = x == 0 || y == 0 || x == width - 1 || y == height - 1;
            let quad = &mut pixels[at..at + 4];
            // Wrapped deliberately: the gradient is meant to repeat, and a
            // cast here is the lint the data path forbids for good reason.
            quad[0] = if edge {
                0
            } else {
                u8::try_from((x * 7) % 256).unwrap_or(0)
            };
            quad[1] = if edge {
                0
            } else {
                u8::try_from((y * 11) % 256).unwrap_or(0)
            };
            quad[2] = if edge { 0 } else { 0x80 };
            quad[3] = if edge { 0 } else { 0xFF };
        }
    }

    let mut buffer = vec![0u8; lowlat_core::png::upper_bound(width, height)];
    let used = lowlat_core::png::encode(&pixels, width, height, (width * 4) as usize, &mut buffer)
        .expect("encode");
    std::fs::File::create(&out)
        .expect("create")
        .write_all(&buffer[..used])
        .expect("write");
    println!("wrote {out}, {used} bytes for {width}x{height}");
}
