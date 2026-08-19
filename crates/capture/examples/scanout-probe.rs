//! Scanout diagnostic. Prints every transition the display pipeline makes.
//!
//! Run it on the machine under test, against the node driving the display:
//!
//!   sudo scanout-probe [/dev/dri/card0]
//!
//! It needs the elevated capability, because reading another client's
//! framebuffer is what it does.
//!
//! What to do with it:
//!
//!   - move the pointer            -> expect the pointer position to follow
//!   - hover a text field          -> expect the redraw count to climb
//!   - enter a mouselook game      -> expect POINTER GONE
//!   - rotate or replug a display  -> expect DISPLAY, a new format or size
//!
//! **The mouselook question is answered and the answer was no**, in the sense
//! that mattered: the pointer does leave the plane for mouselook, but it also
//! leaves for a pointer that merely grew too large to sit there, and only the
//! first means relative. See docs/07-platforms.md section 2.1.
//!
//! **A pointer shape change is not visible from here either**, which this
//! program also established: the redraw count climbs as the pointer moves and
//! carries no information about what the pointer looks like. Telling one shape
//! from another needs the buffer read and compared.
//!
//! Ctrl+C stops; the running totals print every few seconds, so nothing is
//! lost by killing it.

use std::io::{Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use lowlat_capture::scanout::{Card, Cursor, Framebuffer, Layout};
use lowlat_common::clock::{Time, elapsed_ms, precise_sleep};

/// Roughly one frame at sixty. The pipeline is not driven by us, so this is a
/// sampling rate rather than a cadence anything depends on.
const POLL: Duration = Duration::from_millis(16);

/// How often the running totals are printed, in milliseconds.
const STATS_MS: f64 = 5000.0;

fn describe(fb: &Framebuffer) -> String {
    let modifier = match fb.modifier {
        Some(modifier) => format!("{:#018x}", u64::from(modifier)),
        None => "none".to_string(),
    };
    let planes: Vec<String> = fb
        .planes()
        .map(|buffer| format!("pitch {} offset {}", buffer.pitch, buffer.offset))
        .collect();
    format!(
        "{}x{} {:?} modifier {modifier}, {} buffer(s): {}",
        fb.width,
        fb.height,
        fb.format,
        planes.len(),
        planes.join("; ")
    )
}

/// True when the two describe a differently shaped buffer rather than the same
/// one redrawn. The framebuffer id is deliberately not compared here: the
/// display cycles through a small set of them every frame, and every one is the
/// same picture as far as anything downstream is concerned.
fn changed(a: &Framebuffer, b: &Framebuffer) -> bool {
    a.width != b.width
        || a.height != b.height
        || a.format != b.format
        || a.modifier != b.modifier
        || a.planes().count() != b.planes().count()
        || a.planes().zip(b.planes()).any(|(x, y)| x.pitch != y.pitch)
}

/// True when the pointer buffer was redrawn.
///
/// **This is not a shape change, and the difference was measured.** The
/// geometry never moves -- the plane is a fixed 256x256 buffer whatever the
/// pointer is -- so the identifier is the only thing that can differ, and it
/// merely alternates around a pool of about five buffers as the pointer moves.
/// A shape change needs the pixels compared, which needs the buffer read.
fn redrawn(a: &Cursor, b: &Cursor) -> bool {
    changed(&a.image, &b.image) || a.image.id != b.image.id
}

/// Export every buffer once and report what came back, which is the check that
/// the read-only export works on this driver.
fn report_export(card: &Card, fb: &Framebuffer) {
    for (at, buffer) in fb.planes().enumerate() {
        match card.export(buffer) {
            Ok(fd) => {
                // The exported descriptor's length is the allocation size, and
                // it is worth printing because it is how a tiled buffer
                // announces itself: it divides out to more rows than the
                // display has.
                let size = std::fs::File::from(fd).seek(SeekFrom::End(0)).unwrap_or(0);
                println!("         buffer {at}: exported read-only, {size} bytes");
            }
            Err(error) => println!("         buffer {at}: EXPORT FAILED: {error}"),
        }
    }
}

fn announce_cursor(cursor: &Cursor, what: &str) {
    println!(
        "POINTER  {what} at ({},{}), {}",
        cursor.x,
        cursor.y,
        describe(&cursor.image)
    );
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("/dev/dri/card0"), PathBuf::from);

    let card = match Card::open(&path) {
        Ok(card) => card,
        Err(error) => {
            eprintln!("cannot open {}: {error}", path.display());
            std::process::exit(2);
        }
    };
    println!("=== scanout probe on {} ===", path.display());

    let mut last: Option<Layout> = None;
    let mut scans: u64 = 0;
    let mut display_changes: u64 = 0;
    let mut transitions: u64 = 0;
    let mut disappearances: u64 = 0;
    let mut redraws: u64 = 0;
    let mut reported = Time::now();

    loop {
        let layout = match card.scan() {
            Ok(layout) => layout,
            Err(error) => {
                println!("[scan]   {error}");
                precise_sleep(POLL);
                continue;
            }
        };
        scans += 1;

        match &last {
            None => {
                println!("DISPLAY  {}", describe(&layout.primary));
                report_export(&card, &layout.primary);
                match &layout.cursor {
                    Some(cursor) => announce_cursor(cursor, "present"),
                    None => println!("POINTER  absent"),
                }
            }
            Some(previous) => {
                if changed(&previous.primary, &layout.primary) {
                    display_changes += 1;
                    println!("DISPLAY  {}", describe(&layout.primary));
                    report_export(&card, &layout.primary);
                }
                match (&previous.cursor, &layout.cursor) {
                    (None, Some(cursor)) => {
                        transitions += 1;
                        announce_cursor(cursor, "BACK");
                    }
                    (Some(_), None) => {
                        transitions += 1;
                        disappearances += 1;
                        println!("POINTER  GONE");
                    }
                    (Some(before), Some(now)) if redrawn(before, now) => {
                        redraws += 1;
                        // Counted rather than printed: it fires several times a
                        // second while the pointer moves and says nothing about
                        // what the pointer looks like.
                    }
                    _ => {}
                }
            }
        }
        last = Some(layout);

        if elapsed_ms(reported) >= STATS_MS {
            reported = Time::now();
            let pointer = last
                .as_ref()
                .and_then(|layout| layout.cursor.as_ref())
                .map_or_else(|| "absent".to_string(), |c| format!("({},{})", c.x, c.y));
            println!(
                "[stats]  scans={scans} display_changes={display_changes} \
                 pointer_transitions={transitions} pointer_gone={disappearances} \
                 redraws={redraws} pointer={pointer}"
            );
        }

        precise_sleep(POLL);
    }
}
