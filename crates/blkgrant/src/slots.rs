//! A disk descriptor's grant slots, on the driver's side: which grant each slot holds, where
//! the driver's DMA domain mapped it, and the checks every request goes through before the
//! device is told an address.
//!
//! `G` is whatever the driver keeps of the grant (its descriptor, so it can ask whether the
//! grant was revoked and close it on detach).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;

use crate::abi::{PAGE_SIZE, Slot, Transfer};

/// A grant attached to a slot and mapped into the driver's DMA domain.
#[derive(Debug, PartialEq, Eq)]
pub struct Attached<G> {
    /// What the driver keeps of the grant.
    pub grant: G,
    /// The IOVA of the grant's first byte (`MapGrant`'s answer).
    pub iova: u64,
    /// The grant's length in bytes.
    pub len: u64,
    /// Whether the grant lets the device write the buffer.
    pub writable: bool,
}

/// Which way a transfer moves the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the disk into the buffer: the device writes the grant.
    ToBuffer,
    /// From the buffer onto the disk: the device reads the grant.
    FromBuffer,
}

/// Where a checked transfer goes.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolved<'a, G> {
    /// The slot's grant.
    pub grant: &'a G,
    /// The IOVA of the transfer's first byte, page-aligned.
    pub iova: u64,
    /// The transfer's length in blocks.
    pub blocks: u32,
}

/// Why a transfer was refused before reaching the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlotError {
    /// The slot holds no grant.
    Empty(Slot),
    /// The length is zero or not a multiple of the block size.
    Length,
    /// The offset in the grant is not a multiple of [`PAGE_SIZE`].
    Unaligned,
    /// The range does not fit in the grant, or has more blocks than one request can carry.
    Range,
    /// A read from the disk into a read-only grant.
    ReadOnly(Slot),
}

impl fmt::Display for SlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(slot) => write!(f, "{slot} holds no grant"),
            Self::Length => f.write_str("transfer length is zero or not whole blocks"),
            Self::Unaligned => f.write_str("grant offset is not page-aligned"),
            Self::Range => f.write_str("transfer does not fit in the grant"),
            Self::ReadOnly(slot) => write!(f, "{slot} holds a read-only grant"),
        }
    }
}

impl core::error::Error for SlotError {}

/// The grant slots of one disk descriptor.
#[derive(Debug)]
pub struct GrantSlots<G> {
    slots: BTreeMap<Slot, Attached<G>>,
}

impl<G> Default for GrantSlots<G> {
    fn default() -> Self {
        Self {
            slots: BTreeMap::new(),
        }
    }
}

impl<G> GrantSlots<G> {
    /// No slot holds a grant.
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts `attached` in `slot` and returns the grant it held before, which the caller must
    /// unmap.
    pub fn attach(&mut self, slot: Slot, attached: Attached<G>) -> Option<Attached<G>> {
        self.slots.insert(slot, attached)
    }

    /// Empties `slot` and returns its grant, which the caller must unmap.
    pub fn detach(&mut self, slot: Slot) -> Option<Attached<G>> {
        self.slots.remove(&slot)
    }

    /// Empties every slot (the descriptor closed) and returns their grants.
    pub fn take_all(&mut self) -> Vec<Attached<G>> {
        core::mem::take(&mut self.slots).into_values().collect()
    }

    /// Whether no slot holds a grant.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Checks `transfer` against its slot's grant, for a disk with `block_size`-byte blocks,
    /// and returns where the device must go.
    ///
    /// # Errors
    ///
    /// [`SlotError::Empty`] for an empty slot, [`SlotError::Length`] for a length that is zero
    /// or not whole blocks (or a zero `block_size`), [`SlotError::Unaligned`] for an offset
    /// that is not page-aligned, [`SlotError::Range`] for a range outside the grant or over
    /// `u32::MAX` blocks, and [`SlotError::ReadOnly`] for [`Direction::ToBuffer`] into a
    /// read-only grant.
    pub fn resolve(
        &self,
        transfer: &Transfer,
        direction: Direction,
        block_size: u32,
    ) -> Result<Resolved<'_, G>, SlotError> {
        let attached = self
            .slots
            .get(&transfer.slot)
            .ok_or(SlotError::Empty(transfer.slot))?;
        let block_size = u64::from(block_size);
        if transfer.len == 0 || block_size == 0 || !transfer.len.is_multiple_of(block_size) {
            return Err(SlotError::Length);
        }
        if !transfer.offset.is_multiple_of(PAGE_SIZE) {
            return Err(SlotError::Unaligned);
        }
        let end = transfer
            .offset
            .checked_add(transfer.len)
            .ok_or(SlotError::Range)?;
        if end > attached.len {
            return Err(SlotError::Range);
        }
        if direction == Direction::ToBuffer && !attached.writable {
            return Err(SlotError::ReadOnly(transfer.slot));
        }
        let blocks = u32::try_from(transfer.len / block_size).map_err(|_| SlotError::Range)?;
        let iova = attached
            .iova
            .checked_add(transfer.offset)
            .ok_or(SlotError::Range)?;
        Ok(Resolved {
            grant: &attached.grant,
            iova,
            blocks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(raw: u64) -> Slot {
        Slot::new(raw).expect("in range")
    }

    fn table(writable: bool) -> GrantSlots<&'static str> {
        let mut slots = GrantSlots::new();
        let attached = Attached {
            grant: "g",
            iova: 0x10_0000,
            len: 0x4000,
            writable,
        };
        assert!(slots.attach(slot(1), attached).is_none());
        slots
    }

    fn transfer(offset: u64, len: u64) -> Transfer {
        Transfer {
            slot: slot(1),
            offset,
            lba: 0,
            len,
        }
    }

    #[test]
    fn a_transfer_inside_the_grant_resolves_to_its_iova() {
        let slots = table(true);
        let got = slots.resolve(&transfer(0x1000, 0x3000), Direction::ToBuffer, 512);
        assert_eq!(
            got,
            Ok(Resolved {
                grant: &"g",
                iova: 0x10_1000,
                blocks: 24
            })
        );
    }

    #[test]
    fn transfers_outside_the_rules_are_refused() {
        let slots = table(true);
        let to = Direction::ToBuffer;
        assert_eq!(
            slots.resolve(&transfer(0, 0), to, 512),
            Err(SlotError::Length)
        );
        assert_eq!(
            slots.resolve(&transfer(0, 100), to, 512),
            Err(SlotError::Length)
        );
        assert_eq!(
            slots.resolve(&transfer(0, 512), to, 0),
            Err(SlotError::Length)
        );
        assert_eq!(
            slots.resolve(&transfer(512, 512), to, 512),
            Err(SlotError::Unaligned)
        );
        assert_eq!(
            slots.resolve(&transfer(0x1000, 0x4000), to, 512),
            Err(SlotError::Range)
        );
        assert_eq!(
            slots.resolve(&transfer(u64::MAX - 0xfff, 0x1000), to, 512),
            Err(SlotError::Range)
        );
        let empty = Transfer {
            slot: slot(2),
            ..transfer(0, 512)
        };
        assert_eq!(
            slots.resolve(&empty, to, 512),
            Err(SlotError::Empty(slot(2)))
        );
    }

    #[test]
    fn a_read_only_grant_only_feeds_writes() {
        let slots = table(false);
        let t = transfer(0, 0x1000);
        assert_eq!(
            slots.resolve(&t, Direction::ToBuffer, 512),
            Err(SlotError::ReadOnly(slot(1)))
        );
        assert!(slots.resolve(&t, Direction::FromBuffer, 512).is_ok());
    }

    #[test]
    fn attaching_replaces_and_detaching_empties() {
        let mut slots = table(true);
        let newer = Attached {
            grant: "h",
            iova: 0x20_0000,
            len: 0x1000,
            writable: false,
        };
        let old = slots.attach(slot(1), newer).expect("slot 1 held a grant");
        assert_eq!(old.grant, "g");
        assert_eq!(slots.detach(slot(1)).map(|a| a.grant), Some("h"));
        assert!(slots.detach(slot(1)).is_none());
        assert!(slots.is_empty());
    }

    #[test]
    fn closing_takes_every_grant() {
        let mut slots = table(true);
        let other = Attached {
            grant: "h",
            iova: 0x20_0000,
            len: 0x1000,
            writable: true,
        };
        slots.attach(slot(7), other);
        let all: Vec<_> = slots.take_all().into_iter().map(|a| a.grant).collect();
        assert_eq!(all, ["g", "h"]);
        assert!(slots.is_empty());
    }
}
