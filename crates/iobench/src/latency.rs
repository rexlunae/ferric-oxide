//! A latency histogram with fio's bucket layout, and its percentiles.
//!
//! Latencies are nanoseconds. The layout is the log-linear one fio documents for its
//! completion-latency percentiles (fio's HOWTO, `clat_percentiles`, and the comment on its
//! `FIO_IO_U_PLAT_BITS` layout): values below 128 ns each have their own bucket, and every
//! power of two above that is split into [`SUB_BUCKETS`] equal buckets, so a bucket is never
//! wider than 1/64 of its lower edge (under 1.6% error). The histogram covers [`GROUPS`]
//! powers of two, up to about 17 s; anything slower lands in the last bucket. fio reports a
//! percentile as the middle of the bucket it falls in, and so does [`Histogram::percentile`],
//! so the two read the same distribution the same way.
//!
//! The histogram is a fixed array (no allocation) of [`BUCKETS`] counters, about 15 KiB, so
//! every submitter thread keeps its own and the runner [merges](Histogram::merge) them after
//! the run.

/// Bits of linear resolution per power of two: 64 buckets each.
const BITS: u32 = 6;
/// Buckets per power of two.
pub const SUB_BUCKETS: usize = 1 << BITS;
/// Powers of two covered, counting the exact range below `2 × SUB_BUCKETS` as two.
pub const GROUPS: usize = 29;
/// Buckets in a histogram.
pub const BUCKETS: usize = GROUPS * SUB_BUCKETS;

/// The bucket a latency of `ns` falls in.
fn bucket(ns: u64) -> usize {
    let msb = 63 - ns.max(1).leading_zeros();
    let index = if msb <= BITS {
        // Below 2 × SUB_BUCKETS every value is its own bucket.
        ns as usize
    } else {
        let error_bits = msb - BITS;
        let base = (error_bits as usize + 1) << BITS;
        let offset = ((ns >> error_bits) as usize) & (SUB_BUCKETS - 1);
        base + offset
    };
    index.min(BUCKETS - 1)
}

/// The value a bucket stands for: itself in the exact range, the middle of the bucket above
/// it, as fio reports it.
fn bucket_value(index: usize) -> u64 {
    if index < 2 * SUB_BUCKETS {
        return index as u64;
    }
    let error_bits = (index >> BITS) as u32 - 1;
    let low = (1_u64 << (error_bits + BITS)) + (((index & (SUB_BUCKETS - 1)) as u64) << error_bits);
    low + ((1_u64 << error_bits) >> 1)
}

/// Latencies of completed reads: a count per bucket, and the exact count, sum, minimum and
/// maximum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Histogram {
    buckets: [u64; BUCKETS],
    count: u64,
    sum: u64,
    min: u64,
    max: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    /// An empty histogram.
    pub const fn new() -> Self {
        Self {
            buckets: [0; BUCKETS],
            count: 0,
            sum: 0,
            min: u64::MAX,
            max: 0,
        }
    }

    /// Records one read that took `ns` nanoseconds.
    pub fn record(&mut self, ns: u64) {
        self.buckets[bucket(ns)] += 1;
        self.count += 1;
        self.sum = self.sum.saturating_add(ns);
        self.min = self.min.min(ns);
        self.max = self.max.max(ns);
    }

    /// Adds every read `other` recorded to this histogram.
    pub fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.buckets.iter_mut().zip(&other.buckets) {
            *mine += theirs;
        }
        self.count += other.count;
        self.sum = self.sum.saturating_add(other.sum);
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    /// Reads recorded.
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The fastest read, exactly; 0 when nothing was recorded.
    pub const fn min(&self) -> u64 {
        if self.count == 0 { 0 } else { self.min }
    }

    /// The slowest read, exactly; 0 when nothing was recorded.
    pub const fn max(&self) -> u64 {
        self.max
    }

    /// The mean latency, exactly (rounded down); 0 when nothing was recorded.
    pub const fn mean(&self) -> u64 {
        match self.count {
            0 => 0,
            count => self.sum / count,
        }
    }

    /// The latency `per_million` millionths of the reads finished within (`500_000` is the
    /// median, `999_000` the 99.9th percentile): the middle of the bucket that holds the read
    /// of that rank, clamped to the exact minimum and maximum. The rank is rounded up and at
    /// least 1, as fio does. 0 when nothing was recorded.
    ///
    /// # Panics
    ///
    /// When `per_million` is above `1_000_000`.
    pub fn percentile(&self, per_million: u32) -> u64 {
        assert!(per_million <= 1_000_000, "a percentile is at most 100%");
        if self.count == 0 {
            return 0;
        }
        let rank = (u128::from(self.count) * u128::from(per_million)).div_ceil(1_000_000);
        let rank = u64::try_from(rank).unwrap_or(u64::MAX).max(1);
        let mut seen = 0;
        for (index, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return bucket_value(index).clamp(self.min(), self.max);
            }
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values_are_exact() {
        for ns in 0..128 {
            assert_eq!(bucket_value(bucket(ns)), ns);
        }
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(127), 127);
        assert_eq!(bucket(128), 128);
    }

    #[test]
    fn buckets_are_within_one_sixty_fourth() {
        let mut previous = 0;
        for ns in (128..20_000_000_u64).step_by(997) {
            let index = bucket(ns);
            assert!(index >= previous, "buckets grow with the value");
            previous = index;
            let value = bucket_value(index);
            assert!(value.abs_diff(ns) * 64 <= ns, "{ns} reads as {value}");
        }
    }

    #[test]
    fn every_bucket_value_falls_back_in_its_bucket() {
        for index in 0..BUCKETS {
            assert_eq!(bucket(bucket_value(index)), index);
        }
    }

    #[test]
    fn slow_reads_land_in_the_last_bucket() {
        assert_eq!(bucket(u64::MAX), BUCKETS - 1);
        let mut h = Histogram::new();
        h.record(u64::MAX);
        assert_eq!(h.percentile(1_000_000), u64::MAX);
    }

    #[test]
    fn an_empty_histogram_reads_zero() {
        let h = Histogram::new();
        assert_eq!((h.count(), h.min(), h.max(), h.mean()), (0, 0, 0, 0));
        assert_eq!(h.percentile(500_000), 0);
    }

    #[test]
    fn percentiles_follow_the_ranks() {
        let mut h = Histogram::new();
        // 1..=100 µs, one read each.
        for us in 1..=100 {
            h.record(us * 1000);
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.min(), 1000);
        assert_eq!(h.max(), 100_000);
        assert_eq!(h.mean(), 50_500);
        let close = |got: u64, want: u64| got.abs_diff(want) * 64 <= want;
        assert!(close(h.percentile(500_000), 50_000));
        assert!(close(h.percentile(990_000), 99_000));
        assert!(close(h.percentile(1_000_000), 100_000));
        assert!(close(h.percentile(0), 1000));
    }

    #[test]
    fn merging_is_recording_both() {
        let (mut a, mut b, mut both) = (Histogram::new(), Histogram::new(), Histogram::new());
        for ns in [300, 7_000, 90_000] {
            a.record(ns);
            both.record(ns);
        }
        for ns in [150, 2_000_000] {
            b.record(ns);
            both.record(ns);
        }
        a.merge(&b);
        assert_eq!(a, both);
        a.merge(&Histogram::new());
        assert_eq!(a, both);
    }

    #[test]
    #[should_panic(expected = "at most 100%")]
    fn percentiles_past_100_panic() {
        Histogram::new().percentile(1_000_001);
    }
}
