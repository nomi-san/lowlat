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

    println!(
        "plane {}x{} pitch {} at ({},{})",
        layout.cursor.as_ref().map_or(0, |c| c.image.width),
        layout.cursor.as_ref().map_or(0, |c| c.image.height),
        layout
            .cursor
            .as_ref()
            .and_then(|c| c.image.planes().next().map(|b| b.pitch))
            .unwrap_or(0),
        layout.cursor.as_ref().map_or(0, |c| c.x),
        layout.cursor.as_ref().map_or(0, |c| c.y),
    );

    let mut watcher = Watcher::new();
    let (mut reads, mut shapes, mut moves, mut blank) = (0u32, 0u32, 0u32, 0u32);
    let mut last = (i32::MIN, i32::MIN);
    let mut seen: Vec<u32> = Vec::new();

    let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut spent = std::time::Duration::ZERO;
    let mut worst = std::time::Duration::ZERO;
    while std::time::Instant::now() < until {
        let began = std::time::Instant::now();
        let outcome = watcher.read(&card, &at);
        let took = began.elapsed();
        spent += took;
        worst = worst.max(took);
        match outcome {
            Ok(Some(pointer)) => {
                reads += 1;
                if pointer.fresh {
                    shapes += 1;
                    seen.push(pointer.checksum);
                    println!(
                        "shape {}x{} checksum {:#010x} at ({},{}), {} bytes, buffer {}",
                        pointer.width,
                        pointer.height,
                        pointer.checksum,
                        pointer.x,
                        pointer.y,
                        watcher.image().len(),
                        watcher.buffer()
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
    // **The number that says whether an identifier can be trusted as a
    // trigger.** Anything above zero is a shape that arrived in the buffer
    // that carried the one before it, which a reader watching identifiers
    // would never have looked at.
    println!(
        "{} shape(s) arrived in the buffer that already held one, {} read(s) copied the whole \
         plane",
        watcher.repeated_buffers(),
        watcher.whole_reads()
    );
    // **What reading every time costs**, which is the price of not trusting
    // the identifier. It runs on the thread that captures frames, so a mean
    // anywhere near a frame interval is a mean that has to come down.
    if reads > 0 {
        println!(
            "read cost: {:.3} ms mean, {:.3} ms worst, over {reads} reads",
            spent.as_secs_f64() * 1000.0 / f64::from(reads),
            worst.as_secs_f64() * 1000.0
        );
    }
    println!("wrote {}", out.display());
    if seen.len() > distinct {
        // Not a defect: only the last picture is held, so a pointer
        // alternating between two shapes re-encodes each time. What stops it
        // travelling twice is the per-guest cache, not this.
        println!("note: shapes alternated, so a picture was encoded more than once");
    }
}

fn fail(what: &str) -> ! {
    eprintln!("{what}");
    std::process::exit(1)
}
