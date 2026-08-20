//! Watch the pointer and write what it is drawing.
//!
//!   sudo cursor-probe [/dev/dri/card1] [/tmp/pointer.png] [seconds]
//!
//! **The picture is one check and the counts are the other.** An extent
//! computed wrongly, a stride read as a width, or the channel order left alone
//! all produce a file that decodes perfectly and shows the wrong thing. Move
//! the pointer over a text field, a link and a window edge: each should come
//! back a different shape at a different size, and **moving the pointer
//! without changing its shape must not count as a new one**, however many
//! times the display redraws it.

use std::io::Write;
use std::path::PathBuf;

use lowlat_capture::cursor::Watcher;
use lowlat_capture::scanout::Card;

fn main() {
    let mut args = std::env::args().skip(1);
    let node = args
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let out = args
        .next()
        .map_or_else(|| PathBuf::from("/tmp/pointer.png"), PathBuf::from);
    let seconds: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(10);

    let card = Card::open(&node).unwrap_or_else(|e| fail(&format!("open: {e}")));
    let layout = card.scan().unwrap_or_else(|e| fail(&format!("scan: {e}")));
    let Some(at) = layout.cursor_plane else {
        fail("this pipeline has no pointer plane")
    };

    let mut watcher = Watcher::new();
    let (mut reads, mut shapes, mut moves, mut blank) = (0u32, 0u32, 0u32, 0u32);
    let mut last = (i32::MIN, i32::MIN);
    let mut seen: Vec<u32> = Vec::new();

    let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < until {
        match watcher.read(&card, &at) {
            Ok(Some(pointer)) => {
                reads += 1;
                if pointer.fresh {
                    shapes += 1;
                    seen.push(pointer.checksum);
                    println!(
                        "shape {}x{} checksum {:#010x} at ({},{}), {} bytes",
                        pointer.width,
                        pointer.height,
                        pointer.checksum,
                        pointer.x,
                        pointer.y,
                        watcher.image().len()
                    );
                    std::fs::File::create(&out)
                        .and_then(|mut f| f.write_all(watcher.image()))
                        .unwrap_or_else(|e| fail(&format!("write: {e}")));
                }
                if (pointer.x, pointer.y) != last {
                    last = (pointer.x, pointer.y);
                    moves += 1;
                }
            }
            Ok(None) => blank += 1,
            Err(error) => fail(&format!("read: {error}")),
        }
        std::thread::sleep(std::time::Duration::from_millis(18));
    }

    seen.sort_unstable();
    let distinct = {
        let mut unique = seen.clone();
        unique.dedup();
        unique.len()
    };
    println!(
        "{reads} reads, {moves} positions, {shapes} shape changes over {distinct} distinct \
         pictures, {blank} with nothing drawn"
    );
    println!("wrote {}", out.display());
    if seen.len() > distinct {
        println!("note: a picture was adopted twice, so something re-sends an unchanged pointer");
    }
}

fn fail(what: &str) -> ! {
    eprintln!("{what}");
    std::process::exit(1)
}
