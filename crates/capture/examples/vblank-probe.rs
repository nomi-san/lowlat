//! Does this display deliver vblank events?
//!
//!   sudo vblank-probe [/dev/dri/cardN]
//!
//! Arms one vblank event and polls for it, reporting what arrives.

use std::os::fd::AsFd;
use std::os::fd::AsRawFd;

fn main() {
    let node = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/dri/card1".to_string());
    let path = std::path::Path::new(&node);
    let card =
        lowlat_capture::scanout::Card::open(path).unwrap_or_else(|e| fail(&format!("open: {e}")));
    let layout = card.scan().unwrap_or_else(|e| fail(&format!("scan: {e}")));
    println!(
        "{}: {}x{} on vblank index {}",
        node, layout.primary.width, layout.primary.height, layout.crtc_index
    );
    card.arm_vblank(layout.crtc_index)
        .unwrap_or_else(|e| fail(&format!("arm: {e}")));
    println!("armed");

    let mut pollfd = libc::pollfd {
        fd: card.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let began = std::time::Instant::now();
    loop {
        // SAFETY: the descriptor is live and the array is a local.
        let ready = unsafe { libc::poll(&raw mut pollfd, 1, 100) };
        if ready > 0 {
            let saw = card
                .drain_events()
                .unwrap_or_else(|e| fail(&format!("drain: {e}")));
            println!(
                "readable after {:.1} ms, vblank seen: {saw}",
                began.elapsed().as_secs_f64() * 1000.0
            );
            return;
        }
        if began.elapsed().as_secs() > 3 {
            println!(
                "no event after {} ms",
                began.elapsed().as_secs_f64() * 1000.0
            );
            return;
        }
    }
}

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(1)
}
