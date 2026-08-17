//! Per-stage timing, as percentiles rather than averages.
//!
//! **Every figure in docs/05-host.md section 10 is p50, p95 and p99, never a
//! mean.** A pipeline that clears a frame in 4 ms on average and 40 ms once a
//! second holds its frame rate on paper and stutters visibly, and the mean is
//! the one statistic that cannot tell the two apart.
//!
//! Recording is a store into a ring. Sorting happens where the report is
//! asked for, which is never on a frame path.

/// Samples one stage keeps. Seventeen seconds at sixty frames a second, which
/// is long enough for a percentile to mean something and short enough that a
/// tail from a minute ago is not still being reported.
const CAPACITY: usize = 1024;

/// What a stage's samples say, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    /// Samples taken since the stage was created, not the ring's occupancy.
    pub count: u64,
}

/// One stage's rolling samples.
///
/// Fixed storage, written in place. **Nothing here allocates**, because it
/// runs once per frame per stage on the loop that must clear a frame within a
/// frame interval.
#[derive(Debug)]
pub struct Stage {
    samples: [f32; CAPACITY],
    at: usize,
    count: u64,
}

impl Default for Stage {
    fn default() -> Self {
        Self::new()
    }
}

impl Stage {
    pub const fn new() -> Self {
        Self {
            samples: [0.0; CAPACITY],
            at: 0,
            count: 0,
        }
    }

    /// Record one sample, in milliseconds.
    pub fn record(&mut self, ms: f64) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a stage duration in milliseconds; f32 resolves a nanosecond at these scales"
        )]
        if let Some(slot) = self.samples.get_mut(self.at) {
            *slot = ms as f32;
        }
        self.at = (self.at + 1) % CAPACITY;
        self.count = self.count.saturating_add(1);
    }

    /// Sort a copy and read the percentiles off it.
    ///
    /// Off the frame path by construction: it copies the whole ring and sorts
    /// it. Called when a report is due, not when a sample is taken.
    pub fn percentiles(&self) -> Percentiles {
        let filled = usize::try_from(self.count)
            .unwrap_or(CAPACITY)
            .min(CAPACITY);
        if filled == 0 {
            return Percentiles::default();
        }
        let mut sorted = self.samples;
        let Some(window) = sorted.get_mut(..filled) else {
            return Percentiles::default();
        };
        window.sort_unstable_by(f32::total_cmp);
        Percentiles {
            p50: at_rank(window, 0.50),
            p95: at_rank(window, 0.95),
            p99: at_rank(window, 0.99),
            count: self.count,
        }
    }
}

/// The sample at a fraction through a sorted window.
///
/// **Nearest rank**, `ceil(fraction * n) - 1`, not interpolated: the value
/// reported is one an actual frame took, which is what makes a percentile
/// something a reader can go and look at rather than an average of two frames
/// that never happened.
fn at_rank(sorted: &[f32], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "an index into a window of at most CAPACITY entries"
    )]
    let rank = ((fraction * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted
        .get(rank.min(sorted.len() - 1))
        .copied()
        .map_or(0.0, f64::from)
}

/// Every stage the loop measures.
///
/// Conversion is absent because there is no conversion stage: the source
/// emits the layout the encoder takes. It returns with real capture, and the
/// wire stage belongs to the guest's thread rather than this one.
#[derive(Debug, Default)]
pub struct Stages {
    /// A frame becoming available.
    pub acquire: Stage,
    /// Submit to bitstream collected.
    pub encode: Stage,
    /// Bitstream to published and every guest woken.
    pub publish: Stage,
    /// Wall clock between one frame reaching the encoder and the next.
    pub interval: Stage,
}

/// What the stages say, as one value a caller can assert against.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Report {
    pub acquire: Percentiles,
    pub encode: Percentiles,
    pub publish: Percentiles,
    pub interval: Percentiles,
}

impl Report {
    /// The host-side stages added together at the median.
    ///
    /// **The floor Gate A item 7 states**: a pipeline that cannot clear a
    /// frame within a frame interval cannot hold the frame rate.
    pub fn host_p50(&self) -> f64 {
        self.acquire.p50 + self.encode.p50 + self.publish.p50
    }

    /// The same at the tail, which is where a stutter lives.
    pub fn host_p99(&self) -> f64 {
        self.acquire.p99 + self.encode.p99 + self.publish.p99
    }
}

impl Stages {
    pub fn report(&self) -> Report {
        Report {
            acquire: self.acquire.percentiles(),
            encode: self.encode.percentiles(),
            publish: self.publish.percentiles(),
            interval: self.interval.percentiles(),
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn percentiles_come_off_the_rank_they_name() {
        let mut stage = Stage::new();
        // One through a hundred, so every percentile is its own value and a
        // rank that is off by one is visible rather than plausible.
        for value in 1..=100 {
            stage.record(f64::from(value));
        }
        let got = stage.percentiles();
        assert_eq!(got.count, 100);
        // Exact, not approximate: with a hundred distinct samples every
        // percentile lands on its own value, so an off-by-one rank is a
        // failure rather than a rounding difference.
        assert!((got.p50 - 50.0).abs() < 0.001, "p50 was {}", got.p50);
        assert!((got.p95 - 95.0).abs() < 0.001, "p95 was {}", got.p95);
        assert!((got.p99 - 99.0).abs() < 0.001, "p99 was {}", got.p99);
    }

    /// **The reason percentiles are reported and averages are not.** One slow
    /// frame in a hundred moves the mean by a fifth of nothing and moves p99
    /// onto the stall, which is the figure that decides whether a stream
    /// stutters.
    #[test]
    fn a_rare_stall_shows_at_the_tail_and_not_at_the_median() {
        let mut stage = Stage::new();
        // Two in a hundred, which is past p99 rather than sitting exactly on
        // it. One in a hundred lands on the boundary and says nothing either
        // way, which is the sort of test that looks like a measurement and is
        // an arithmetic accident.
        for _ in 0..980 {
            stage.record(4.0);
        }
        for _ in 0..20 {
            stage.record(40.0);
        }

        let got = stage.percentiles();
        assert!((got.p50 - 4.0).abs() < 0.001, "the median moved");
        assert!(got.p99 >= 40.0, "the stall did not reach p99: {}", got.p99);
    }

    /// The ring holds the most recent samples, so a report describes now
    /// rather than the whole session.
    #[test]
    fn samples_past_the_capacity_replace_the_oldest() {
        let mut stage = Stage::new();
        for _ in 0..CAPACITY {
            stage.record(100.0);
        }
        for _ in 0..CAPACITY {
            stage.record(1.0);
        }
        let got = stage.percentiles();
        assert_eq!(got.count, (CAPACITY * 2) as u64);
        assert!(
            (got.p99 - 1.0).abs() < 0.001,
            "an old sample is still being reported: {}",
            got.p99
        );
    }
}
