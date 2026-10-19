//! The same-boot A/B comparison of queue policies, and its pass rule.
//!
//! The benchmark measures a policy several times in one boot, alternating with the policy it
//! is compared against, so both see the same runner and the same disk state. Runner hardware
//! varies by about a factor of two between CI jobs, which makes numbers from different runs
//! useless for a gate; rounds in one boot vary by much less, and that variation is the noise
//! the rule allows for.
//!
//! The rule works on *paired* rounds: round `i` of the baseline and round `i` of the candidate
//! run back to back, so their ratio cancels the drift of the runner's speed over the boot,
//! which unpaired medians do not. The candidate passes when the median of the per-round ratios
//! reaches 1 less a tolerance. The tolerance is the largest relative spread (largest minus
//! smallest, over the median) of the baseline's rounds, the candidate's rounds and the ratios,
//! clamped to [`MIN_TOLERANCE`]..=[`MAX_TOLERANCE`] permille. The lower clamp allows for noise
//! a few rounds may not show; the upper one keeps a very noisy run from passing anything.
//!
//! A first batch of [`ROUNDS`] rounds that fails is not yet a verdict ([`Verdict::Confirm`]):
//! the benchmark runs [`CONFIRM_ROUNDS`] more and decides on all of them. Two policies that
//! run the same code (`Adaptive` serves inline, as `MainQueue` does, while the scheme serves
//! one call at a time) then pass, because one unlucky round no longer moves the median, while
//! a candidate that is really slower is slower in every round and still fails.

/// The smallest tolerance, in permille of the baseline.
pub const MIN_TOLERANCE: u32 = 50;
/// The largest tolerance, in permille of the baseline.
pub const MAX_TOLERANCE: u32 = 150;
/// Rounds in the first batch.
pub const ROUNDS: usize = 3;
/// Rounds added when the first batch fails.
pub const CONFIRM_ROUNDS: usize = 6;
/// The most rounds a comparison uses: the first batch and the confirmation.
pub const MAX_ROUNDS: usize = ROUNDS + CONFIRM_ROUNDS;

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
        (Some(&low), Some(&high)) if middle > 0 => permille(high - low, middle),
        _ => 0,
    }
}

/// `part` over `whole` in permille, saturating (0 for a zero `whole`).
fn permille(part: u32, whole: u32) -> u32 {
    if whole == 0 {
        return 0;
    }
    u32::try_from(u64::from(part) * 1000 / u64::from(whole)).unwrap_or(u32::MAX)
}

/// Round `i`'s ratio, candidate over baseline, in permille. A round whose baseline measured
/// nothing has no ratio to take, so it counts as a tie when the candidate measured nothing too,
/// and as the candidate being as fast as can be when it measured something.
fn round_ratio(baseline: u32, candidate: u32) -> u32 {
    match (baseline, candidate) {
        (0, 0) => 1000,
        (0, _) => u32::MAX,
        (baseline, candidate) => permille(candidate, baseline),
    }
}

/// What to do with a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// The candidate reaches the baseline within the noise.
    Pass,
    /// The first batch fails: run [`CONFIRM_ROUNDS`] more rounds and compare all of them.
    Confirm,
    /// The candidate is slower than the baseline in [`MAX_ROUNDS`] rounds.
    Fail,
}

/// A candidate policy's rounds against a baseline's, from the same boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comparison {
    /// The baseline's median.
    pub baseline: u32,
    /// The candidate's median.
    pub candidate: u32,
    /// The median of the per-round ratios, candidate over baseline, in permille (see
    /// [`Comparison::new`] for rounds whose baseline measured nothing).
    pub ratio: u32,
    /// The noise allowed, in permille.
    pub tolerance: u32,
    /// The rounds compared.
    pub rounds: usize,
}

impl Comparison {
    /// Compares `candidate` against `baseline`, where `baseline[i]` and `candidate[i]` were
    /// measured in the same round. Rounds past the shorter slice, and past [`MAX_ROUNDS`], are
    /// left out. A round whose baseline is 0 counts as a tie when its candidate is 0 too, and
    /// as a win for the candidate otherwise, since a zero baseline gives no ratio to take.
    pub fn new(baseline: &[u32], candidate: &[u32]) -> Self {
        let rounds = baseline.len().min(candidate.len()).min(MAX_ROUNDS);
        let mut base = [0; MAX_ROUNDS];
        let mut cand = [0; MAX_ROUNDS];
        let mut ratios = [0; MAX_ROUNDS];
        base[..rounds].copy_from_slice(&baseline[..rounds]);
        cand[..rounds].copy_from_slice(&candidate[..rounds]);
        for ((ratio, &b), &c) in ratios.iter_mut().zip(&base).zip(&cand).take(rounds) {
            *ratio = round_ratio(b, c);
        }
        let (base, cand, ratios) = (
            &mut base[..rounds],
            &mut cand[..rounds],
            &mut ratios[..rounds],
        );
        let tolerance = spread(base)
            .max(spread(cand))
            .max(spread(ratios))
            .clamp(MIN_TOLERANCE, MAX_TOLERANCE);
        Self {
            baseline: median(base),
            candidate: median(cand),
            ratio: median(ratios),
            tolerance,
            rounds,
        }
    }

    /// Whether the candidate reaches the baseline within the tolerance.
    pub fn passes(&self) -> bool {
        self.ratio >= 1000 - self.tolerance
    }

    /// What to do next: pass, run the confirmation rounds, or fail.
    pub fn verdict(&self) -> Verdict {
        if self.passes() {
            Verdict::Pass
        } else if self.rounds < MAX_ROUNDS {
            Verdict::Confirm
        } else {
            Verdict::Fail
        }
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
        let c = Comparison::new(&[5000, 5100, 4900], &[5050, 4950, 5000]);
        assert_eq!(c.baseline, 5000);
        assert_eq!(c.candidate, 5000);
        assert_eq!(c.ratio, 1010);
        assert_eq!(c.verdict(), Verdict::Pass);
    }

    #[test]
    fn pairing_cancels_drift() {
        // The runner slows down over the boot; each round the candidate matches the baseline.
        let c = Comparison::new(&[6000, 5500, 5000], &[5980, 5510, 4990]);
        assert!(c.ratio >= 997);
        assert_eq!(c.verdict(), Verdict::Pass);
    }

    #[test]
    fn identical_code_with_one_bad_round_passes_after_confirmation() {
        // The 2026-10-19 amdvi-enforce failure, in shape: the same code in both arms, and
        // two of three candidate rounds about 16% slow. Three rounds fail ...
        let first = Comparison::new(&[6046, 6100, 6000], &[5106, 5150, 6050]);
        assert!(!first.passes());
        assert_eq!(first.verdict(), Verdict::Confirm);
        // ... and six more ordinary rounds put the median back at about 1.
        let baseline = [6046, 6100, 6000, 6000, 5900, 6100, 5800, 6050, 5950];
        let candidate = [5106, 5150, 6050, 6010, 5880, 6120, 5790, 6040, 5960];
        let all = Comparison::new(&baseline, &candidate);
        assert_eq!(all.rounds, MAX_ROUNDS);
        assert_eq!(all.verdict(), Verdict::Pass);
    }

    #[test]
    fn a_real_regression_still_fails() {
        // A third slower with quiet rounds fails, after the confirmation too.
        let c = Comparison::new(&[6000, 6050, 5950], &[4000, 4020, 3980]);
        assert_eq!(c.tolerance, MIN_TOLERANCE);
        assert_eq!(c.verdict(), Verdict::Confirm);
        let c = Comparison::new(&[6000; MAX_ROUNDS], &[4000; MAX_ROUNDS]);
        assert_eq!(c.verdict(), Verdict::Fail);
        // 20% slower in every round, with rounds noisy enough to reach the cap, fails too.
        let baseline = [5000, 6000, 5500, 5200, 5800, 5400, 5600, 5100, 5900];
        let candidate = baseline.map(|b| b * 8 / 10);
        let c = Comparison::new(&baseline, &candidate);
        assert_eq!(c.tolerance, MAX_TOLERANCE);
        assert_eq!(c.verdict(), Verdict::Fail);
    }

    #[test]
    fn noise_widens_the_tolerance_up_to_the_cap() {
        // 10% spread in the baseline allows a candidate 8% lower.
        let c = Comparison::new(&[4750, 5000, 5250], &[4600, 4600, 4600]);
        assert_eq!(c.tolerance, 100);
        assert!(c.passes());
        // Wild rounds are capped at 15%, so a candidate at half the baseline still fails.
        let c = Comparison::new(&[1000, 5000, 9000], &[500, 2500, 4500]);
        assert_eq!(c.tolerance, MAX_TOLERANCE);
        assert!(!c.passes());
    }

    #[test]
    fn a_faster_candidate_passes() {
        let c = Comparison::new(&[3000, 3000, 3000], &[6000, 6100, 5900]);
        assert_eq!(c.ratio, 2000);
        assert_eq!(c.verdict(), Verdict::Pass);
    }

    #[test]
    fn edge_cases() {
        // No rounds, unequal lengths and too many rounds do not panic.
        assert_eq!(Comparison::new(&[], &[]).rounds, 0);
        assert_eq!(Comparison::new(&[1, 2, 3], &[1]).rounds, 1);
        assert_eq!(Comparison::new(&[1; 20], &[1; 20]).rounds, MAX_ROUNDS);
    }

    #[test]
    fn zero_baseline_rounds() {
        // All rounds zero on both sides: a tie, which passes.
        let c = Comparison::new(&[0, 0, 0], &[0, 0, 0]);
        assert_eq!(c.ratio, 1000);
        assert_eq!(c.verdict(), Verdict::Pass);
        // A positive candidate against a baseline that measured nothing passes.
        let c = Comparison::new(&[0, 0, 0], &[5, 5, 5]);
        assert_eq!(c.ratio, u32::MAX);
        assert_eq!(c.verdict(), Verdict::Pass);
        // One zero baseline round among ordinary ones does not decide the median.
        let c = Comparison::new(&[0, 6000, 6000], &[5900, 5950, 6010]);
        assert_eq!(c.verdict(), Verdict::Pass);
        // Nor does it hide a regression in the other rounds.
        let baseline = [0, 6000, 6000, 6000, 6000, 6000, 6000, 6000, 6000];
        let c = Comparison::new(&baseline, &[4000; MAX_ROUNDS]);
        assert_eq!(c.verdict(), Verdict::Fail);
        // A zero candidate against a working baseline fails.
        let c = Comparison::new(&[6000; MAX_ROUNDS], &[0; MAX_ROUNDS]);
        assert_eq!(c.verdict(), Verdict::Fail);
    }
}
