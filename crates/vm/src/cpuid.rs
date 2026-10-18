//! The CPUID table a VMM gives the kernel with `SetCpuid`, and the lookup the kernel answers
//! guest CPUID exits from without going back to the VMM.
//!
//! The payload is up to [`MAX_ENTRIES`] records of [`CpuidEntry::BYTES`] bytes, sorted by
//! (function, index) with no duplicates, so the kernel can binary-search it. A function whose
//! subleaves differ (for example 4, 7, 0Bh, 0Dh) has one entry per index with
//! [`CpuidEntry::indexed`] set; other functions have a single entry with index 0 that answers
//! any ECX. A function not in the table answers zeros, as CPUs do for leaves above their
//! maximum.

use thiserror::Error;

/// The most entries a table may hold.
pub const MAX_ENTRIES: usize = 256;

/// One CPUID answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CpuidEntry {
    /// The function (EAX on input).
    pub function: u32,
    /// The index (ECX on input), when `indexed`.
    pub index: u32,
    /// Whether the answer depends on ECX.
    pub indexed: bool,
    /// The answer: EAX, EBX, ECX, EDX.
    pub regs: [u32; 4],
}

impl CpuidEntry {
    /// The record's size.
    pub const BYTES: usize = 32;

    /// The record: function, index, flags (bit 0 indexed), EAX, EBX, ECX, EDX, and a
    /// reserved zero word.
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        let words = [
            self.function,
            self.index,
            u32::from(self.indexed),
            self.regs[0],
            self.regs[1],
            self.regs[2],
            self.regs[3],
            0,
        ];
        let mut out = [0; Self::BYTES];
        for (chunk, word) in out.chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// The entry a record holds.
    ///
    /// # Errors
    ///
    /// [`CpuidError::Length`] when `bytes` is not [`CpuidEntry::BYTES`] long, and
    /// [`CpuidError::Reserved`] when an unknown flag or the reserved word is set, or an
    /// unindexed entry has a non-zero index.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CpuidError> {
        if bytes.len() != Self::BYTES {
            return Err(CpuidError::Length);
        }
        let mut words = [0u32; 8];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(4)) {
            *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        let [function, index, flags, eax, ebx, ecx, edx, reserved] = words;
        if flags & !1 != 0 || reserved != 0 || (flags == 0 && index != 0) {
            return Err(CpuidError::Reserved);
        }
        Ok(Self {
            function,
            index,
            indexed: flags == 1,
            regs: [eax, ebx, ecx, edx],
        })
    }

    const fn key(&self) -> (u32, u32) {
        (self.function, self.index)
    }
}

/// Checks that `entries` is a valid table: at most [`MAX_ENTRIES`], sorted by (function,
/// index), no duplicates, and each function either indexed throughout or a single unindexed
/// entry.
///
/// # Errors
///
/// [`CpuidError::TooMany`], [`CpuidError::Order`] or [`CpuidError::MixedIndexing`].
pub fn validate(entries: &[CpuidEntry]) -> Result<(), CpuidError> {
    if entries.len() > MAX_ENTRIES {
        return Err(CpuidError::TooMany);
    }
    for pair in entries.windows(2) {
        if pair[0].key() >= pair[1].key() {
            return Err(CpuidError::Order);
        }
        if pair[0].function == pair[1].function && !(pair[0].indexed && pair[1].indexed) {
            return Err(CpuidError::MixedIndexing);
        }
    }
    Ok(())
}

/// The answer for CPUID with EAX = `function` and ECX = `index`, from a table [`validate`]
/// accepted: the matching entry's registers, or zeros.
pub fn lookup(entries: &[CpuidEntry], function: u32, index: u32) -> [u32; 4] {
    let start = entries.partition_point(|entry| entry.function < function);
    let mut candidates = entries[start..]
        .iter()
        .take_while(|entry| entry.function == function);
    let found = match candidates.next() {
        Some(first) if !first.indexed => Some(first),
        Some(first) => core::iter::once(first)
            .chain(candidates)
            .find(|entry| entry.index == index),
        None => None,
    };
    found.map_or([0; 4], |entry| entry.regs)
}

/// Why a CPUID table or record was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CpuidError {
    /// A record has the wrong length.
    #[error("a CPUID record has the wrong length")]
    Length,
    /// A record sets a reserved field.
    #[error("a CPUID record sets a reserved field")]
    Reserved,
    /// The table has more than [`MAX_ENTRIES`] entries.
    #[error("the CPUID table has more than {MAX_ENTRIES} entries")]
    TooMany,
    /// The table is not sorted, or has a duplicate.
    #[error("the CPUID table is not sorted or has a duplicate")]
    Order,
    /// A function has both indexed and unindexed entries, or several unindexed ones.
    #[error("a CPUID function mixes indexed and unindexed entries")]
    MixedIndexing,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(function: u32, index: Option<u32>, eax: u32) -> CpuidEntry {
        CpuidEntry {
            function,
            index: index.unwrap_or(0),
            indexed: index.is_some(),
            regs: [eax, 0, 0, 0],
        }
    }

    #[test]
    fn lookups() {
        let table = [
            entry(0, None, 0xd),
            entry(1, None, 0x806c1),
            entry(7, Some(0), 1),
            entry(7, Some(1), 0x10),
            entry(0x4000_0000, None, 0x4000_0001),
        ];
        assert_eq!(validate(&table), Ok(()));
        assert_eq!(lookup(&table, 1, 99)[0], 0x806c1);
        assert_eq!(lookup(&table, 7, 1)[0], 0x10);
        assert_eq!(lookup(&table, 7, 2), [0; 4]);
        assert_eq!(lookup(&table, 3, 0), [0; 4]);
        assert_eq!(lookup(&table, 0x4000_0000, 0)[0], 0x4000_0001);
        assert_eq!(lookup(&[], 0, 0), [0; 4]);
    }

    #[test]
    fn invalid_tables() {
        assert_eq!(
            validate(&[entry(1, None, 0), entry(0, None, 0)]),
            Err(CpuidError::Order)
        );
        assert_eq!(
            validate(&[entry(7, Some(0), 0), entry(7, Some(0), 0)]),
            Err(CpuidError::Order)
        );
        assert_eq!(
            validate(&[entry(7, None, 0), entry(7, Some(1), 0)]),
            Err(CpuidError::MixedIndexing)
        );
        let many = [entry(0, None, 0); MAX_ENTRIES + 1];
        assert_eq!(validate(&many), Err(CpuidError::TooMany));
    }

    #[test]
    fn records_round_trip() {
        let leaf = entry(0xd, Some(1), 0xf);
        assert_eq!(CpuidEntry::from_bytes(&leaf.to_bytes()), Ok(leaf));
        let mut bad = leaf.to_bytes();
        bad[8] = 2;
        assert_eq!(CpuidEntry::from_bytes(&bad), Err(CpuidError::Reserved));
        let mut bad = entry(1, None, 0).to_bytes();
        bad[4] = 1;
        assert_eq!(CpuidEntry::from_bytes(&bad), Err(CpuidError::Reserved));
        let mut bad = leaf.to_bytes();
        bad[31] = 1;
        assert_eq!(CpuidEntry::from_bytes(&bad), Err(CpuidError::Reserved));
        assert_eq!(CpuidEntry::from_bytes(&bad[..16]), Err(CpuidError::Length));
    }
}
