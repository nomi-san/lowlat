//! Whether a framebuffer identifier names a buffer.
//!
//!   sudo buffer-identity [/dev/dri/card1] [seconds]
//!
//! The display cycles a pool of buffers and the capture path keeps one import
//! per buffer, keyed by the identifier the kernel reports. That is only sound
//! if an identifier names the same memory every time it appears. **Turn the
//! monitor off and on while this runs**: if an identifier comes back pointing
//! at different memory, a cache keyed on it hands out a picture from before
//! the blank.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lowlat_capture::scanout::Card;

fn main() {
    let mut args = std::env::args().skip(1);
    let node = args
        .next()
        .map_or_else(|| PathBuf::from("/dev/dri/card1"), PathBuf::from);
    let seconds: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(20);

    let card = Card::open(&node).expect("open");
    println!(
        "anything plugged in: {}",
        card.attached()
            .map_or_else(|e| format!("{e}"), |a| a.to_string())
    );
    let layout = card.scan().expect("scan");
    let plane = layout.primary_plane;

    // Identifier to the inode last seen behind it.
    let mut seen: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    let (mut reads, mut reused, mut moved) = (0u32, 0u32, 0u32);

    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        let Ok(fb) = card.framebuffer_on(plane) else {
            std::thread::sleep(Duration::from_millis(16));
            continue;
        };
        let Some(buffer) = fb.planes().next() else {
            continue;
        };
        let Ok(fd) = card.export(buffer) else {
            continue;
        };
        // SAFETY: a live descriptor, and the struct is plain data.
        let mut stat: libc::stat = unsafe { core::mem::zeroed() };
        // SAFETY: the descriptor is open and the struct is writable.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
            continue;
        }
        let inode = stat.st_ino;
        reads += 1;
        match seen.insert(fb.id, inode) {
            Some(previous) if previous == inode => reused += 1,
            Some(previous) => {
                moved += 1;
                println!("id {} moved: inode {previous} -> {inode}", fb.id);
            }
            None => println!("id {} first seen, inode {inode}", fb.id),
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    println!("{reads} reads over {} identifiers", seen.len());
    println!("  {reused} where the identifier named the same memory");
    println!("  {moved} where it named different memory");
    if moved > 0 {
        println!("an identifier is not a buffer, and a cache keyed on one serves stale pictures");
    }
}
