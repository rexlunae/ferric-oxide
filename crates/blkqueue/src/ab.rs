//! The same-boot A/B comparison of queue policies, and its pass rule.
//!
//! The benchmark measures a policy several times in one boot, alternating with the policy it
//! is compared against, so both see the same runner and the same disk state. Runner hardware
//! varies by about a factor of two between CI jobs, which makes numbers from different runs
//! useless for a gate; rounds in one boot vary by much less, and that variation is the noise
//! the rule allows for.
//!
//! The rule: the candidate's median must reach the baseline's median less a tolerance, which
//! is the larger relative spread (largest minus smallest, over the median) of the two sample
//! sets, clamped to [`MIN_TOLERANCE`]..=[`MAX_TOLERANCE`] permille. The lower clamp allows for
//! noise three rounds may not show; the upper one keeps a very noisy run from passing anything.

/// The smallest tolerance, in permille of the baseline's median.
pub const MIN_TOLERANCE: u32 = 50;
/// The largest tolerance, in permille of the baseline's median.
pub const MAX_TOLERANCE: u32 = 150;

/// The median of `samples` (the lower middle one for an even count), sorting them in place,
/// or 0 for no samples.
pub fn median(samples: &mut [u32]) -> u32 {
    samples.sort_unstable();
    match samples.len() {
        0 => 0,
        len => samples[(len - 1) / 2],
    }
}

/// How far `samples` spread, largest minus smallest, in permille of their median (0 for no
/// samples or a zero median).
pub fn spread(samples: &mut [u32]) -> u32 {
    let middle = median(samples);
    match (samples.first(), samples.last()) {
        (Some(&low), Some(&high)) if middle > 0 => {
            let permille = u64::from(high - low) * 1000 / u64::from(middle);
            u32::try_from(permille).unwrap_or(u32::MAX)
        }
        _ => 0,
    }
}

/// A candidate policy's rounds against a baseline's, from the same boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The baseline's median.
    pub baseline: u32,
    /// The candidate's median.
    pub candidate: u32,
    /// The noise allowed, in permille of the baseline's median.
    pub tolerance: u32,
}

impl Comparison {
    /// Compares `candidate` samples against `baseline` samples (both sorted in place).
    pub fn new(baseline: &mut [u32], candidate: &mut [u32]) -> Self {
        let tolerance = spread(baseline)
            .max(spread(candidate))
            .clamp(MIN_TOLERANCE, MAX_TOLERANCE);
        Self {
            baseline: median(baseline),
            candidate: median(candidate),
            tolerance,
        }
    }

    /// The candidate's median over the baseline's, in permille (0 for a zero baseline).
    pub fn ratio(&self) -> u32 {
        match self.baseline {
            0 => 0,
            baseline => {
                let permille = u64::from(self.candidate) * 1000 / u64::from(baseline);
                u32::try_from(permille).unwrap_or(u32::MAX)
            }
        }
    }

    /// Whether the candidate is at least the baseline within the tolerance.
    pub fn passes(&self) -> bool {
        u64::from(self.candidate) * 1000
            >= u64::from(self.baseline) * u64::from(1000 - self.tolerance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_spread() {
        assert_eq!(median(&mut []), 0);
        assert_eq!(median(&mut [5]), 5);
        assert_eq!(median(&mut [9, 1, 5]), 5);
        assert_eq!(median(&mut [4, 1, 3, 2]), 2);
        assert_eq!(spread(&mut [900, 1000, 1100]), 200);
        assert_eq!(spread(&mut [0, 0, 0]), 0);
    }

    #[test]
    fn equal_policies_pass() {
        let c = Comparison::new(&mut [5000, 5100, 4900], &mut [5050, 4950, 5000]);
        assert_eq!(c.baseline, 5000);
        assert_eq!(c.candidate, 5000);
        assert_eq!(c.ratio(), 1000);
        assert!(c.passes());
    }

    #[test]
    fn a_clear_regression_fails() {
        // The candidate is a third slower with quiet rounds: the 5% floor does not cover it.
        let c = Comparison::new(&mut [6000, 6050, 5950], &mut [4000, 4020, 3980]);
        assert_eq!(c.tolerance, MIN_TOLERANCE);
        assert!(!c.passes());
    }

    #[test]
    fn noise_widens_the_tolerance_up_to_the_cap() {
        // 10% spread in the baseline allows a candidate 8% lower.
        let c = Comparison::new(&mut [4750, 5000, 5250], &mut [4600, 4600, 4600]);
        assert_eq!(c.tolerance, 100);
        assert!(c.passes());
        // Wild rounds are capped at 15%, so a candidate at half the baseline still fails.
        let c = Comparison::new(&mut [1000, 5000, 9000], &mut [2500, 2500, 2500]);
        assert_eq!(c.tolerance, MAX_TOLERANCE);
        assert!(!c.passes());
    }

    #[test]
    fn a_faster_candidate_passes() {
        let c = Comparison::new(&mut [3000, 3000, 3000], &mut [6000, 6100, 5900]);
        assert_eq!(c.ratio(), 2000);
        assert!(c.passes());
    }
}
