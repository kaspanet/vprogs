use std::time::{Duration, Instant};

/// Filters reorgs using exponential decay.
///
/// Observed reorg depths accumulate into a threshold that determines the `min_confirmation_count`
/// passed to the L1 API. Every half-life, the threshold halves until it decays to zero. This
/// creates stable oscillation around a level that filters most reorgs while allowing visibility
/// when the network stabilizes.
pub(crate) struct ReorgFilter {
    /// Half-life, or zero to disable filtering.
    half_life: Duration,
    /// Current threshold level (halves each half-life).
    threshold: u64,
    /// When the next halving occurs.
    next_halving: Option<Instant>,
}

impl ReorgFilter {
    /// Creates a new filter with the given half-life. Pass `Duration::ZERO` to disable.
    pub(crate) fn new(half_life: Duration) -> Self {
        Self { half_life, threshold: 0, next_halving: None }
    }

    /// Returns the current threshold after applying any pending halvings.
    ///
    /// Returns `None` if the filter is disabled (zero period) or the threshold has decayed to zero.
    pub(crate) fn threshold(&mut self) -> Option<u64> {
        if self.half_life.is_zero() {
            return None;
        }

        // Apply halvings for each expired period.
        if let Some(mut expiry) = self.next_halving {
            while Instant::now() >= expiry && self.threshold > 0 {
                self.threshold /= 2;
                expiry += self.half_life;
            }

            self.next_halving = if self.threshold == 0 { None } else { Some(expiry) };
        }

        if self.threshold == 0 { None } else { Some(self.threshold) }
    }

    /// Records a reorg of the given depth, adding it to the threshold and resetting the decay
    /// timer.
    pub(crate) fn record(&mut self, depth: u64) {
        if !self.half_life.is_zero() && depth != 0 {
            self.threshold += depth;
            self.next_halving = Some(Instant::now() + self.half_life);
        }
    }
}
