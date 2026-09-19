//! The figure a client reports: a smoothed time per unit of work, in the
//! shape every peer reports it.

/// An exponentially weighted average with a tenth's weight on the newest
/// sample, in milliseconds, seeded from the first.
#[derive(Debug, Clone, Copy, Default)]
pub struct Smoothed {
    ms: Option<f64>,
}

impl Smoothed {
    /// Fold one sample in and return the average in whole microseconds.
    pub fn push(&mut self, sample_ms: f64) -> u32 {
        let ms = match self.ms {
            Some(ms) => 0.9 * ms + 0.1 * sample_ms,
            None => sample_ms,
        };
        self.ms = Some(ms);
        micros(ms)
    }
}

/// Whole microseconds from a millisecond figure, saturated.
pub fn micros(ms: f64) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative duration in whole microseconds, saturated"
    )]
    let us = (ms * 1000.0).round().max(0.0).min(f64::from(u32::MAX)) as u32;
    us
}

#[cfg(test)]
mod tests {
    use super::Smoothed;

    #[test]
    fn the_first_sample_seeds_and_later_ones_weigh_a_tenth() {
        let mut s = Smoothed::default();
        assert_eq!(s.push(2.0), 2000);
        assert_eq!(s.push(4.0), 2200);
        assert_eq!(s.push(4.0), 2380);
    }
}
