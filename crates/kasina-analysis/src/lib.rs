//! Deterministic, incremental analysis primitives.

/// Incremental single-pole low-pass filter.
#[derive(Debug, Clone)]
pub struct LowPass {
    alpha: f64,
    previous: Option<f64>,
}

impl LowPass {
    /// Construct a filter with `0 < alpha <= 1`.
    pub fn new(alpha: f64) -> Result<Self, &'static str> {
        if !(0.0 < alpha && alpha <= 1.0) {
            return Err("alpha must be in (0, 1]");
        }
        Ok(Self {
            alpha,
            previous: None,
        })
    }

    /// Push one value and return its filtered value.
    pub fn push(&mut self, value: f64) -> f64 {
        let filtered = self.previous.map_or(value, |previous| {
            value.mul_add(self.alpha, previous * (1.0 - self.alpha))
        });
        self.previous = Some(filtered);
        filtered
    }
}

/// Root mean square of successive differences in RR intervals.
#[must_use]
pub fn rmssd(rr_intervals_us: &[u64]) -> Option<f64> {
    if rr_intervals_us.len() < 2 {
        return None;
    }
    let mean_square = rr_intervals_us
        .windows(2)
        .map(|pair| {
            let difference = pair[1] as f64 - pair[0] as f64;
            difference * difference
        })
        .sum::<f64>()
        / (rr_intervals_us.len() - 1) as f64;
    Some(mean_square.sqrt() / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_pass_matches_reference_recurrence() {
        let mut filter = LowPass::new(0.25).unwrap();
        assert_eq!(filter.push(4.0), 4.0);
        assert_eq!(filter.push(8.0), 5.0);
        assert_eq!(filter.push(9.0), 6.0);
    }

    #[test]
    fn computes_rmssd_in_milliseconds() {
        let value = rmssd(&[1_000_000, 1_100_000, 900_000]).unwrap();
        assert!((value - 158.113_883).abs() < 0.001);
    }
}
