//! Logical CPU identities, publication, and atomic scheduler-affinity masks.
//!
//! Mask operations tolerate IDs outside the fixed mask capacity. Mask iteration
//! and snapshots observe individual words atomically, not the entire set at once.

use core::{
    fmt::Display,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

/// Number of logical CPUs published to the scheduler, including the boot CPU.
static CPU_COUNT: AtomicU32 = AtomicU32::new(1);

/// Returns the number of CPUs available to the scheduler.
#[inline(always)]
pub(crate) fn cpu_count() -> u32 {
    CPU_COUNT.load(Ordering::Acquire)
}

/// Publishes an initialized CPU only if it is the next contiguous logical ID.
#[cfg(any(target_arch = "aarch64", test))]
pub(crate) fn publish_cpu(cpu_id: LogicalCpuId) -> bool {
    publish_cpu_into(&CPU_COUNT, cpu_id)
}

#[cfg(any(target_arch = "aarch64", test))]
fn publish_cpu_into(count: &AtomicU32, cpu_id: LogicalCpuId) -> bool {
    let expected = cpu_id.get();
    let Some(next) = expected
        .checked_add(1)
        .filter(|&next| next <= MAX_CPU_COUNT)
    else {
        return false;
    };
    count
        .compare_exchange(expected, next, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// A unique number used internally by the kernel to identify CPUs.
///
/// This is usually but not necessarily the same as the APIC ID.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
// TODO: NonMaxUsize?
// TODO: Optimize away this type if not cfg!(feature = "multi_core")
pub struct LogicalCpuId(u32);

impl LogicalCpuId {
    /// The boot processor's logical identity.
    pub const BSP: Self = Self::new(0);

    /// Reserves the next identity during serialized legacy AP startup.
    ///
    /// # Panics
    ///
    /// Panics if the fixed CPU capacity is exhausted, without changing the count.
    pub fn next() -> Self {
        let id = CPU_COUNT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
                (id < MAX_CPU_COUNT).then(|| id.saturating_add(1))
            })
            .expect("logical CPU capacity exhausted");
        Self(id)
    }

    /// Wraps a logical ID; this does not establish that the CPU is online.
    pub const fn new(inner: u32) -> Self {
        Self(inner)
    }
    /// Returns the numeric logical ID.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl core::fmt::Debug for LogicalCpuId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "[logical cpu #{}]", self.0)
    }
}
impl core::fmt::Display for LogicalCpuId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[cfg(target_pointer_width = "64")]
/// Maximum number of CPUs representable by the scheduler's affinity masks.
pub const MAX_CPU_COUNT: u32 = 128;

#[cfg(target_pointer_width = "32")]
/// Maximum number of CPUs representable by the scheduler's affinity masks.
pub const MAX_CPU_COUNT: u32 = 32;

const SET_WORDS: usize = (MAX_CPU_COUNT / usize::BITS) as usize;

// Ferric Oxide: drivers build the mask `sched-affinity` takes with `ferric_irq::affinity`.
#[cfg(all(feature = "irq-percpu", target_pointer_width = "64"))]
const _: () = assert!(core::mem::size_of::<RawMask>() == ferric_irq::affinity::MASK_BYTES);

// TODO: Support more than 128 CPUs.
// The maximum number of CPUs on Linux is configurable, and the type for LogicalCpuSet and
// LogicalCpuId may be optimized accordingly. In that case, box the mask if it's larger than some
// base size (probably 256 bytes).
#[derive(Debug)]
/// An atomic fixed-capacity set of logical CPUs.
pub struct LogicalCpuSet([AtomicUsize; SET_WORDS]);

fn parts(id: LogicalCpuId) -> (usize, u32) {
    ((id.get() / usize::BITS) as usize, id.get() % usize::BITS)
}
impl LogicalCpuSet {
    /// Creates a mask with no CPUs selected.
    pub const fn empty() -> Self {
        Self([const { AtomicUsize::new(0) }; SET_WORDS])
    }

    /// Creates a mask selecting every representable CPU, including offline CPUs.
    pub const fn all() -> Self {
        Self([const { AtomicUsize::new(!0) }; SET_WORDS])
    }

    /// Tests membership, returning false for an unrepresentable ID.
    pub fn contains(&self, id: LogicalCpuId) -> bool {
        let (word, bit) = parts(id);
        self.0
            .get(word)
            .is_some_and(|word| word.load(Ordering::Acquire) & (1 << bit) != 0)
    }

    /// Selects a CPU; unrepresentable IDs leave the mask unchanged.
    pub fn atomic_set(&self, id: LogicalCpuId) {
        let (word, bit) = parts(id);
        if let Some(word) = self.0.get(word) {
            word.fetch_or(1 << bit, Ordering::Release);
        }
    }

    /// Deselects a CPU; unrepresentable IDs leave the mask unchanged.
    pub fn atomic_clear(&self, id: LogicalCpuId) {
        let (word, bit) = parts(id);
        if let Some(word) = self.0.get(word) {
            word.fetch_and(!(1 << bit), Ordering::Release);
        }
    }

    /// Replaces the mask while it is exclusively borrowed.
    pub fn override_from(&mut self, raw: &RawMask) {
        self.0 = raw.map(AtomicUsize::new);
    }

    /// Loads each mask word with acquire ordering.
    pub fn to_raw(&self) -> RawMask {
        self.0.each_ref().map(|w| w.load(Ordering::Acquire))
    }

    /// Visits selected CPUs in increasing order, observing concurrent changes.
    pub fn iter(&self) -> impl Iterator<Item = LogicalCpuId> + '_ {
        self.0.iter().enumerate().flat_map(move |(i, w)| {
            (0..usize::BITS).filter_map(move |b| {
                if w.load(Ordering::Acquire) & (1 << b) != 0 {
                    Some(LogicalCpuId::new(i as u32 * usize::BITS + b))
                } else {
                    None
                }
            })
        })
    }
}

impl Display for LogicalCpuSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write_mask(&self.to_raw(), cpu_count(), f)
    }
}

fn write_mask(raw: &RawMask, count: u32, f: &mut impl core::fmt::Write) -> core::fmt::Result {
    let count = count.min(MAX_CPU_COUNT);
    let words = raw
        .get(..count.div_ceil(usize::BITS) as usize)
        .unwrap_or(&[]);
    let tail = count % usize::BITS;
    for (i, word) in words.iter().enumerate() {
        if i != 0 {
            write!(f, "_")?;
        }
        let word = if tail != 0 && i == words.len().saturating_sub(1) {
            *word & (1_usize << tail).wrapping_sub(1)
        } else {
            *word
        };
        write!(f, "{word:X}")?;
    }
    Ok(())
}

/// Native-endian words used by the scheduler-affinity ABI.
pub type RawMask = [usize; SET_WORDS];

/// Borrows the native-endian byte representation of an affinity mask.
pub fn mask_as_bytes(mask: &RawMask) -> &[u8] {
    // SAFETY: A usize array has no padding and every bit is initialized. The
    // byte slice borrows the entire input and cannot outlive it.
    unsafe { core::slice::from_raw_parts(mask.as_ptr().cast(), size_of::<RawMask>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec::Vec};

    #[test]
    fn every_cpu_can_be_selected_and_cleared() {
        let set = LogicalCpuSet::empty();
        for id in 0..MAX_CPU_COUNT {
            let cpu = LogicalCpuId::new(id);
            assert!(!set.contains(cpu));
            set.atomic_set(cpu);
            assert!(set.contains(cpu));
        }
        assert_eq!(set.to_raw(), [usize::MAX; SET_WORDS]);
        assert_eq!(
            set.iter().map(LogicalCpuId::get).collect::<Vec<_>>(),
            (0..MAX_CPU_COUNT).collect::<Vec<_>>()
        );
        for cpu in set.iter() {
            set.atomic_clear(cpu);
        }
        assert_eq!(set.to_raw(), [0; SET_WORDS]);
    }

    #[test]
    fn invalid_ids_do_not_touch_the_mask() {
        let set = LogicalCpuSet::all();
        for id in [MAX_CPU_COUNT, MAX_CPU_COUNT + 1, u32::MAX] {
            let cpu = LogicalCpuId::new(id);
            assert!(!set.contains(cpu));
            set.atomic_set(cpu);
            set.atomic_clear(cpu);
            assert_eq!(set.to_raw(), [usize::MAX; SET_WORDS]);
        }
    }

    #[test]
    fn masks_preserve_whole_words_at_cpu_boundaries() {
        let raw = [usize::MAX; SET_WORDS];
        for count in 0..=MAX_CPU_COUNT {
            let mut text = String::new();
            write_mask(&raw, count, &mut text).unwrap();
            let full = count / usize::BITS;
            let tail = count % usize::BITS;
            let mut expected = alloc::vec![alloc::format!("{:X}", usize::MAX); full as usize];
            if tail != 0 {
                expected.push(alloc::format!("{:X}", (1_usize << tail) - 1));
            }
            assert_eq!(text, expected.join("_"), "count {count}");
        }
    }

    #[test]
    fn raw_mask_round_trip_and_byte_order() {
        let mut set = LogicalCpuSet::empty();
        let raw = [0x1234; SET_WORDS];
        set.override_from(&raw);
        assert_eq!(set.to_raw(), raw);
        assert_eq!(
            mask_as_bytes(&raw),
            raw.into_iter()
                .flat_map(usize::to_ne_bytes)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn publication_rejects_duplicates_gaps_and_overflow() {
        let count = AtomicU32::new(1);
        assert!(!publish_cpu_into(&count, LogicalCpuId::new(2)));
        assert!(!publish_cpu_into(&count, LogicalCpuId::BSP));
        for id in 1..MAX_CPU_COUNT {
            assert!(publish_cpu_into(&count, LogicalCpuId::new(id)));
            assert!(!publish_cpu_into(&count, LogicalCpuId::new(id)));
        }
        for id in [MAX_CPU_COUNT, u32::MAX] {
            assert!(!publish_cpu_into(&count, LogicalCpuId::new(id)));
        }
        assert_eq!(count.load(Ordering::Acquire), MAX_CPU_COUNT);
    }

    #[test]
    fn concurrent_publication_has_one_winner() {
        use alloc::sync::Arc;
        use std::{sync::Barrier, thread};

        let count = Arc::new(AtomicU32::new(1));
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let count = Arc::clone(&count);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    publish_cpu_into(&count, LogicalCpuId::new(1))
                })
            })
            .collect();
        let winners = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap().then_some(()))
            .count();
        assert_eq!(winners, 1);
        assert_eq!(count.load(Ordering::Acquire), 2);
    }
}
