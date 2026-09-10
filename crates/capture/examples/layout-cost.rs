//! How much one layout query costs, which decides whether a poll is affordable.
//!
//!   layout-cost [connector] [uid]

fn main() {
    let connector = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "DP-4".to_string());
    let mut samples = Vec::new();
    for _ in 0..200 {
        let started = std::time::Instant::now();
        let _ = lowlat_capture::desktop::placement_of(&connector);
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    // SAFETY: a plain read of this process's own identity.
    let uid = std::env::args()
        .nth(2)
        .and_then(|uid| uid.parse().ok())
        .unwrap_or_else(|| unsafe { libc::getuid() });
    println!(
        "layout_of({uid}): {:?} output(s)",
        lowlat_capture::desktop::layout_of(uid).map(|outputs| outputs.len())
    );
    samples.sort_by(f64::total_cmp);
    let at = |numerator: usize, denominator: usize| {
        samples[(samples.len() - 1) * numerator / denominator]
    };
    println!(
        "placement_of({connector}) x{}: p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms  max {:.3} ms",
        samples.len(),
        at(1, 2),
        at(19, 20),
        at(99, 100),
        samples[samples.len() - 1]
    );
}
