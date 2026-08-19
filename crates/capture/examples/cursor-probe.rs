//! Read the pointer off its plane and write what came back.
//!
//!   sudo cursor-probe [/dev/dri/card1] [/tmp/pointer.png]
//!
//! **The picture is the check.** An extent computed wrongly, a stride read as
//! a width, or the channel order left alone all produce a file that decodes
//! perfectly and shows the wrong thing. Move the pointer over a text field, a
//! link and a window edge and run it again: each should come back a different
//! shape at a different size.

use std::io::Write;
use std::path::PathBuf;

use lowlat_capture::cursor::Reader;
use lowlat_capture::scanout::Card;

fn main() {
    let mut args = std::env::args().skip(1);
    let node = args
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let out = args
        .next()
        .map_or_else(|| PathBuf::from("/tmp/pointer.png"), PathBuf::from);

    let card = Card::open(&node).unwrap_or_else(|e| fail(&format!("open: {e}")));
    let layout = card.scan().unwrap_or_else(|e| fail(&format!("scan: {e}")));
    let Some(cursor) = layout.cursor.as_ref() else {
        fail("nothing is drawing a pointer")
    };
    println!(
        "plane {}x{} {:?} at ({},{})",
        cursor.image.width, cursor.image.height, cursor.image.format, cursor.x, cursor.y
    );

    let mut reader = Reader::new();
    let read = reader
        .read(&card, &cursor.image)
        .unwrap_or_else(|e| fail(&format!("read: {e}")));
    let Some((area, rgba)) = read else {
        println!("the pointer is drawn as nothing");
        return;
    };
    println!(
        "drawn part {}x{} at ({},{}) inside the plane, {} bytes",
        area.width,
        area.height,
        area.x,
        area.y,
        rgba.len()
    );

    let mut buffer = vec![0u8; lowlat_core::png::upper_bound(area.width, area.height)];
    let used = lowlat_core::png::encode(
        rgba,
        area.width,
        area.height,
        (area.width as usize) * 4,
        &mut buffer,
    )
    .unwrap_or_else(|e| fail(&format!("encode: {e:?}")));
    let hash = lowlat_core::crc32::of(buffer.get(..used).unwrap_or_default());
    println!("encoded {used} bytes, image checksum {hash:#010x}");

    std::fs::File::create(&out)
        .and_then(|mut f| f.write_all(buffer.get(..used).unwrap_or_default()))
        .unwrap_or_else(|e| fail(&format!("write: {e}")));
    println!("wrote {}", out.display());
}

fn fail(what: &str) -> ! {
    eprintln!("{what}");
    std::process::exit(1)
}
