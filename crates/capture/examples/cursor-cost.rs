//! Where the time in a pointer read goes.
//!
//!   sudo cursor-cost [/dev/dri/card1] [seconds]
//!
//! The read is on the thread that captures frames, so its cost is measured in
//! frame intervals. This separates the three parts that could be paying it:
//! describing the plane, exporting and mapping the buffer, and touching the
//! pixels.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lowlat_capture::scanout::Card;

/// Row counts to price a partial copy at.
const ROWS: [usize; 3] = [32, 64, 128];

fn main() {
    let mut args = std::env::args().skip(1);
    let node = args
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let seconds: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(5);

    let card = Card::open(&node).expect("open");
    let layout = card.scan().expect("scan");
    let at = layout.cursor_plane.expect("a pointer plane");

    let (mut describe, mut map, mut strided, mut bulk, mut scan_copy) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );
    let mut partial = [Duration::ZERO; ROWS.len()];
    let mut rounds = 0u32;
    let mut copy = vec![0u8; 256 * 256 * 4];

    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        let began = Instant::now();
        let Ok(Some(cursor)) = card.cursor_on(&at) else {
            continue;
        };
        describe += began.elapsed();

        let buffer = cursor.image.planes().next().expect("a buffer");
        let length = (buffer.pitch as usize) * (cursor.image.height as usize);

        let began = Instant::now();
        let fd = card.export(buffer).expect("export");
        // SAFETY: a live buffer of at least `length` bytes, mapped read-only
        // and unmapped below.
        let mapped = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert!(mapped != libc::MAP_FAILED, "mmap");
        map += began.elapsed();

        // SAFETY: mapped above, this length, read only.
        let pixels = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), length) };

        // Every fourth byte, which is what finding the drawn extent does.
        let began = Instant::now();
        let mut opaque = 0u32;
        for at in (3..length).step_by(4) {
            if pixels[at] != 0 {
                opaque += 1;
            }
        }
        strided += began.elapsed();

        // The same bytes as one bulk copy into system memory.
        let began = Instant::now();
        copy[..length].copy_from_slice(pixels);
        bulk += began.elapsed();

        // A copy of only the first rows, which is where every pointer seen
        // here is drawn. The whole buffer is a fixed 256 high whatever the
        // pointer is.
        for (slot, rows) in ROWS.iter().enumerate() {
            let part = (buffer.pitch as usize) * (*rows).min(cursor.image.height as usize);
            let began = Instant::now();
            copy[..part].copy_from_slice(&pixels[..part]);
            partial[slot] += began.elapsed();
        }

        // And the same scan again, over the copy.
        let began = Instant::now();
        let mut second = 0u32;
        for at in (3..length).step_by(4) {
            if copy[at] != 0 {
                second += 1;
            }
        }
        scan_copy += began.elapsed();
        assert_eq!(opaque, second);

        // SAFETY: mapped above, this length, nothing refers to it now.
        unsafe {
            libc::munmap(mapped, length);
        }
        rounds += 1;
    }

    let per = |d: Duration| d.as_secs_f64() * 1000.0 / f64::from(rounds.max(1));
    println!("{rounds} rounds, per round:");
    println!("  describe the plane   {:.3} ms", per(describe));
    println!("  export and map       {:.3} ms", per(map));
    println!("  strided scan, mapped {:.3} ms", per(strided));
    println!("  bulk copy out        {:.3} ms", per(bulk));
    println!("  strided scan, copied {:.3} ms", per(scan_copy));
    for (slot, rows) in ROWS.iter().enumerate() {
        println!("  bulk copy, {rows:>3} rows   {:.3} ms", per(partial[slot]));
    }
}
