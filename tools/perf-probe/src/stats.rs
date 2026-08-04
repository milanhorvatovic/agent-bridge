//! Percentiles, computed rather than estimated.
//!
//! The budgets under test are stated at P99, so how P99 is defined is part of
//! the measurement. This uses the **nearest-rank** definition: sort the
//! samples ascending and take element `ceil(p × n)`, 1-indexed. It is the
//! definition that answers the question the budget asks — "99% of samples
//! came in at or under this" — exactly, with no interpolation between
//! neighbouring samples and no sketch that trades accuracy for memory.
//!
//! Ten thousand `u64`s is 80 KB. There is no reason to approximate.

/// A summary of one measured distribution. Units are the caller's — this
/// module counts and sorts, it does not know whether it holds nanoseconds or
/// bytes per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub count: usize,
    pub min: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
    pub mean: u64,
}

/// Summarise `samples`. Sorts in place — the caller has no further use for
/// the order, and copying ten thousand samples to preserve it would be a
/// courtesy to nobody.
///
/// `None` for an empty set: a percentile over no samples is not a number,
/// and returning zero would put a passing verdict on a lane that measured
/// nothing.
pub fn summarize(samples: &mut [u64]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let total: u128 = samples.iter().map(|value| u128::from(*value)).sum();
    Some(Summary {
        count: samples.len(),
        min: samples[0],
        p50: nearest_rank(samples, 50),
        p90: nearest_rank(samples, 90),
        p99: nearest_rank(samples, 99),
        max: samples[samples.len() - 1],
        mean: (total / samples.len() as u128) as u64,
    })
}

/// Element `ceil(percent/100 × n)` of an ascending slice, 1-indexed.
fn nearest_rank(sorted: &[u64], percent: u64) -> u64 {
    let n = sorted.len() as u64;
    // Integer ceiling of percent × n / 100, floored at 1 so the rank is a
    // real element even for a single sample.
    let rank = (percent * n).div_ceil(100).max(1);
    sorted[(rank - 1) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distribution whose answers can be read off by hand: 1..=100, so the
    /// P99 is 99 and the P50 is 50, under the nearest-rank definition.
    #[test]
    fn percentiles_match_a_known_distribution() {
        let mut samples: Vec<u64> = (1..=100).collect();
        let summary = summarize(&mut samples).expect("100 samples summarise");
        assert_eq!(summary.count, 100);
        assert_eq!(summary.min, 1);
        assert_eq!(summary.p50, 50);
        assert_eq!(summary.p90, 90);
        assert_eq!(summary.p99, 99);
        assert_eq!(summary.max, 100);
        assert_eq!(summary.mean, 50); // 5050/100 = 50.5, truncated
    }

    #[test]
    fn the_order_samples_arrive_in_does_not_matter() {
        let mut ascending: Vec<u64> = (1..=1000).collect();
        let mut descending: Vec<u64> = (1..=1000).rev().collect();
        assert_eq!(summarize(&mut ascending), summarize(&mut descending));
    }

    /// The property the budget verdicts rest on: at most 1% of samples may
    /// sit above the P99. A definition that interpolated, or that rounded
    /// the rank down, would let a lane pass with more.
    #[test]
    fn at_most_one_percent_of_samples_exceed_the_p99() {
        for n in [1usize, 7, 99, 100, 101, 10_000] {
            let mut samples: Vec<u64> = (0..n as u64).collect();
            let summary = summarize(&mut samples).expect("non-empty");
            let above = samples.iter().filter(|s| **s > summary.p99).count();
            assert!(
                above * 100 <= n,
                "n={n}: {above} samples above the P99 is more than 1%"
            );
        }
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let summary = summarize(&mut [42]).expect("one sample summarises");
        assert_eq!(
            (summary.min, summary.p50, summary.p99, summary.max),
            (42, 42, 42, 42)
        );
    }

    #[test]
    fn no_samples_is_not_a_zero() {
        assert_eq!(summarize(&mut []), None);
    }

    #[test]
    fn the_mean_does_not_overflow_on_large_samples() {
        // Nanosecond samples near u64::MAX would overflow a u64 accumulator
        // long before the count did.
        let mut samples = vec![u64::MAX; 16];
        let summary = summarize(&mut samples).expect("non-empty");
        assert_eq!(summary.mean, u64::MAX);
    }
}
