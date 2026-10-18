//! A disk descriptor's grant slots, on the driver's side: which grant each slot holds, where
//! the driver's DMA domain mapped it, and the checks every request goes through before the
//! device is told an address.
//!
//! `G` is whatever the driver keeps of the grant (its descriptor, so it can ask whether the
//! grant was revoked and close it on detach).
//!
//! # When a mapping may be released
//!
//! A slot holds its grant as a [`Lease`], and every transfer [`resolve`](GrantSlots::resolve)d
//! from it holds another one until the device has finished with the IOVA. The driver unmaps
//! the grant in `G`'s `Drop`, which runs only when the last lease goes. That is never earlier
//! than the slot letting go (detach, re-attach or close) *and* every in-flight transfer
//! completing. Releasing the IOVA of a revoked grant while a command still targets it would let
//! a later grant reuse that IOVA and receive the stale DMA; the lease makes that impossible
//! however detach, close and revocation interleave with transfers.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
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

/// A shared hold on an attached grant: one belongs to the slot and one to each transfer in
/// flight on it. The grant (and so its mapping) is dropped when the last one goes.
pub type Lease<G> = Rc<Attached<G>>;

/// Which way a transfer moves the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the disk into the buffer: the device writes the grant.
    ToBuffer,
    /// From the buffer onto the disk: the device reads the grant.
    FromBuffer,
}

/// Where a checked transfer goes.
///
/// It holds a [`Lease`] on the slot's grant: keep it until the device has completed the
/// transfer, so the grant's mapping outlives the command even if the slot is emptied meanwhile.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolved<G> {
    /// The transfer's hold on the slot's grant.
    pub lease: Lease<G>,
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
    slots: BTreeMap<Slot, Lease<G>>,
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

    /// Puts `attached` in `slot` and returns the slot's lease on the grant it held before.
    /// Dropping that lease releases the old grant once no transfer still holds it.
    pub fn attach(&mut self, slot: Slot, attached: Attached<G>) -> Option<Lease<G>> {
        self.slots.insert(slot, Rc::new(attached))
    }

    /// Empties `slot` and returns its lease on the grant (see [`attach`](Self::attach)).
    pub fn detach(&mut self, slot: Slot) -> Option<Lease<G>> {
        self.slots.remove(&slot)
    }

    /// Empties every slot (the descriptor closed) and returns their leases.
    pub fn take_all(&mut self) -> Vec<Lease<G>> {
        core::mem::take(&mut self.slots).into_values().collect()
    }

    /// Whether no slot holds a grant.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Checks `transfer` against its slot's grant, for a disk with `block_size`-byte blocks,
    /// and returns where the device must go, with a [`Lease`] the transfer must hold until it
    /// completes.
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
    ) -> Result<Resolved<G>, SlotError> {
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
            lease: Rc::clone(attached),
            iova,
            blocks,
        })
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;

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
        let got = slots
            .resolve(&transfer(0x1000, 0x3000), Direction::ToBuffer, 512)
            .expect("inside the grant");
        assert_eq!(got.lease.grant, "g");
        assert_eq!((got.iova, got.blocks), (0x10_1000, 24));
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

    /// Records when a grant is released, as a driver's unmapping `Drop` would.
    struct Mapping<'a> {
        iova: u64,
        released: &'a RefCell<Vec<u64>>,
    }

    impl Drop for Mapping<'_> {
        fn drop(&mut self) {
            self.released.borrow_mut().push(self.iova);
        }
    }

    fn mapped(iova: u64, released: &RefCell<Vec<u64>>) -> Attached<Mapping<'_>> {
        Attached {
            grant: Mapping { iova, released },
            iova,
            len: 0x4000,
            writable: true,
        }
    }

    #[test]
    fn a_revoked_grant_stays_mapped_until_its_in_flight_transfer_completes() {
        let released = RefCell::new(Vec::new());
        let mut slots = GrantSlots::new();
        slots.attach(slot(1), mapped(0x10_0000, &released));
        // A read is submitted to the device and holds its lease while the command runs.
        let in_flight = slots
            .resolve(&transfer(0, 0x1000), Direction::ToBuffer, 512)
            .expect("inside the grant");
        // Meanwhile the application revokes the grant and detaches it, and attaches a new one
        // that could otherwise be mapped at the freed IOVA.
        drop(slots.detach(slot(1)));
        slots.attach(slot(1), mapped(0x20_0000, &released));
        assert!(
            released.borrow().is_empty(),
            "released under a running command"
        );
        assert!(
            slots
                .resolve(&transfer(0, 0x1000), Direction::ToBuffer, 512)
                .is_ok_and(|r| r.iova == 0x20_0000),
            "new transfers reach the new grant only"
        );
        // The command completes: only now may the revoked grant's IOVA be released.
        assert_eq!(in_flight.iova, 0x10_0000);
        drop(in_flight);
        assert_eq!(*released.borrow(), [0x10_0000]);
    }

    #[test]
    fn closing_waits_for_every_transfer() {
        let released = RefCell::new(Vec::new());
        let mut slots = GrantSlots::new();
        slots.attach(slot(1), mapped(0x10_0000, &released));
        slots.attach(slot(2), mapped(0x20_0000, &released));
        slots.attach(slot(3), mapped(0x30_0000, &released));
        let on_two = Transfer {
            slot: slot(2),
            ..transfer(0, 0x1000)
        };
        let first = slots.resolve(&transfer(0, 0x1000), Direction::ToBuffer, 512);
        let second = slots.resolve(&on_two, Direction::FromBuffer, 512);
        drop(slots.take_all());
        assert_eq!(
            *released.borrow(),
            [0x30_0000],
            "an idle grant goes at once"
        );
        drop(first);
        assert_eq!(*released.borrow(), [0x30_0000, 0x10_0000]);
        drop(second);
        assert_eq!(*released.borrow(), [0x30_0000, 0x10_0000, 0x20_0000]);
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
