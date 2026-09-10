//! How much one layout query costs, which decides whether a poll is affordable.
//!
//!   layout-cost [connector]

fn main() {
    let connector = std::env::args().nth(1).unwrap_or_else(|| "DP-4".to_string());
    let mut samples = Vec::new();
    for _ in 0..200 {
        let started = std::time::Instant::now();
        let _ = lowlat_capture::desktop::placement_of(&connector);
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    let at = |p: f64| samples[((samples.len() - 1) as f64 * p) as usize];
    println!(
        "placement_of({connector}) x{}: p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms  max {:.3} ms",
        samples.len(),
        at(0.5),
        at(0.95),
        at(0.99),
        samples[samples.len() - 1]
    );
}
