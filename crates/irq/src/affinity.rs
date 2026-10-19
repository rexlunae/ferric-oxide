//! The CPU mask a thread's `sched-affinity` handle reads and writes (the kernel's proc scheme),
//! in one place for the kernel and the drivers that pin threads to the CPU their queue
//! interrupts.
//!
//! The mask is one bit per logical CPU ([`CpuId`]), CPU `n` in bit `n % 64` of word `n / 64`,
//! [`MASK_WORDS`] native-endian 64-bit words in all (the kernel's `MAX_CPU_COUNT` of 128 on
//! 64-bit targets). Writing it replaces the thread's affinity; the scheduler moves the thread
//! at its next switch.
//!
//! # Examples
//!
//! ```
//! use ferric_irq::CpuId;
//! use ferric_irq::affinity::AffinityMask;
//!
//! let mask = AffinityMask::only(CpuId::new(65)).unwrap();
//! assert!(mask.contains(CpuId::new(65)));
//! assert!(!mask.contains(CpuId::new(1)));
//! assert_eq!(AffinityMask::from_bytes(mask.to_bytes()), mask);
//! assert!(AffinityMask::only(CpuId::new(128)).is_none());
//! ```

use crate::CpuId;

/// Words in the mask.
pub const MASK_WORDS: usize = 2;
/// Bytes in the mask, as the kernel reads and writes it.
pub const MASK_BYTES: usize = MASK_WORDS * 8;
/// The CPUs a mask can name: `0..MAX_CPUS`.
pub const MAX_CPUS: u32 = (MASK_WORDS * 64) as u32;

/// A set of logical CPUs a thread may run on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AffinityMask([u64; MASK_WORDS]);

impl AffinityMask {
    /// No CPU.
    #[must_use]
    pub const fn empty() -> Self {
        Self([0; MASK_WORDS])
    }

    /// Only `cpu`, or `None` when the mask cannot name it ([`MAX_CPUS`] or above).
    #[must_use]
    pub fn only(cpu: CpuId) -> Option<Self> {
        let mut mask = Self::empty();
        mask.insert(cpu).then_some(mask)
    }

    /// Adds `cpu`; returns `false`, changing nothing, when the mask cannot name it.
    pub fn insert(&mut self, cpu: CpuId) -> bool {
        let Some((word, bit)) = place(cpu) else {
            return false;
        };
        self.0[word] |= 1 << bit;
        true
    }

    /// Whether the set holds `cpu`.
    #[must_use]
    pub fn contains(&self, cpu: CpuId) -> bool {
        place(cpu).is_some_and(|(word, bit)| self.0[word] & (1 << bit) != 0)
    }

    /// The bytes the kernel reads.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; MASK_BYTES] {
        let mut bytes = [0; MASK_BYTES];
        for (chunk, word) in bytes.chunks_exact_mut(8).zip(self.0) {
            chunk.copy_from_slice(&word.to_ne_bytes());
        }
        bytes
    }

    /// The mask in `bytes`, as the kernel writes it.
    #[must_use]
    pub fn from_bytes(bytes: [u8; MASK_BYTES]) -> Self {
        let mut words = [0; MASK_WORDS];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(8)) {
            let mut raw = [0; 8];
            raw.copy_from_slice(chunk);
            *word = u64::from_ne_bytes(raw);
        }
        Self(words)
    }
}

/// The word and bit of `cpu`, or `None` beyond [`MAX_CPUS`].
fn place(cpu: CpuId) -> Option<(usize, u32)> {
    let n = cpu.get();
    (n < MAX_CPUS).then_some(((n / 64) as usize, n % 64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpus_land_in_their_word_and_bit() {
        let mut mask = AffinityMask::empty();
        assert!(mask.insert(CpuId::new(0)));
        assert!(mask.insert(CpuId::new(63)));
        assert!(mask.insert(CpuId::new(64)));
        assert_eq!(mask.0, [1 | 1 << 63, 1]);
        assert!(mask.contains(CpuId::new(64)));
        assert!(!mask.contains(CpuId::new(65)));
    }

    #[test]
    fn cpus_past_the_mask_are_refused() {
        let mut mask = AffinityMask::empty();
        assert!(!mask.insert(CpuId::new(MAX_CPUS)));
        assert_eq!(mask, AffinityMask::empty());
        assert!(!mask.contains(CpuId::new(u32::MAX)));
        assert!(AffinityMask::only(CpuId::new(MAX_CPUS)).is_none());
    }

    #[test]
    fn bytes_round_trip_in_native_order() {
        let mask = AffinityMask::only(CpuId::new(3)).unwrap();
        let bytes = mask.to_bytes();
        assert_eq!(&bytes[..8], &(1_u64 << 3).to_ne_bytes());
        assert_eq!(&bytes[8..], &[0; 8]);
        assert_eq!(AffinityMask::from_bytes(bytes), mask);
    }
}
