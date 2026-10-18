//! Producer-side bookkeeping for a ring of fixed-size entries in memory that hardware consumes,
//! shared by VT-d's invalidation queue and AMD-Vi's command buffer.
//!
//! Both are a power-of-two number of 16-byte slots. Software writes entries at the tail and
//! then moves the tail register past them; hardware processes entries from the head and moves
//! the head register. One slot always stays empty, so a full ring is told apart from an empty
//! one: the ring is empty when head equals tail. [`Ring`] tracks the tail and hands out slots.
//!
//! # Examples
//!
//! ```
//! use ferric_iommu::ring::Ring;
//!
//! let mut ring = Ring::new(256).expect("a power of two");
//! let slots: Vec<u32> = ring.reserve(0, 2)?.collect();
//! assert_eq!(slots, [0, 1]);
//! assert_eq!(ring.tail(), 2);
//! assert_eq!(ring.free_slots(0), 253);
//! # Ok::<(), ferric_iommu::ring::RingFull>(())
//! ```

use core::fmt;

/// The ring does not have room for a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingFull {
    /// Entries in the batch.
    pub needed: u32,
    /// Free slots.
    pub free: u32,
}

impl fmt::Display for RingFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ring full: {} entries, {} free slots",
            self.needed, self.free
        )
    }
}

impl core::error::Error for RingFull {}

/// The tail of a ring of `entries` slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ring {
    entries: u32,
    tail: u32,
}

impl Ring {
    /// An empty ring of `entries` slots, or `None` unless `entries` is a power of two of at
    /// least 2.
    pub const fn new(entries: u32) -> Option<Self> {
        if entries >= 2 && entries.is_power_of_two() {
            Some(Self { entries, tail: 0 })
        } else {
            None
        }
    }

    /// The number of slots.
    pub const fn entries(&self) -> u32 {
        self.entries
    }

    /// The slot the next entry goes in.
    pub const fn tail(&self) -> u32 {
        self.tail
    }

    /// How many entries can be added while hardware's head is at slot `head` (taken modulo the
    /// ring's size).
    pub const fn free_slots(&self, head: u32) -> u32 {
        let n = self.entries;
        ((head % n) + n - self.tail - 1) % n
    }

    /// Whether hardware has processed every entry written so far.
    pub const fn is_drained(&self, head: u32) -> bool {
        head % self.entries == self.tail
    }

    /// Reserves `count` slots for a batch, moving the tail past them, and returns the slots in
    /// order (they wrap at the end of the ring). Write the entries into those slots, then
    /// write the new tail to the hardware.
    ///
    /// # Errors
    ///
    /// [`RingFull`] when fewer than `count` slots are free; the tail does not move.
    pub fn reserve(&mut self, head: u32, count: u32) -> Result<Slots, RingFull> {
        let free = self.free_slots(head);
        if count > free {
            return Err(RingFull {
                needed: count,
                free,
            });
        }
        let first = self.tail;
        self.tail = (self.tail + count) % self.entries;
        Ok(Slots {
            next: first,
            left: count,
            entries: self.entries,
        })
    }
}

/// The slots [`Ring::reserve`] handed out, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slots {
    next: u32,
    left: u32,
    entries: u32,
}

impl Iterator for Slots {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.left == 0 {
            return None;
        }
        let slot = self.next;
        self.next = (self.next + 1) % self.entries;
        self.left -= 1;
        Some(slot)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}

impl ExactSizeIterator for Slots {}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    #[test]
    fn wraps_and_fills() {
        assert_eq!(Ring::new(3), None);
        assert_eq!(Ring::new(1), None);
        let mut ring = Ring::new(4).unwrap();
        assert!(ring.is_drained(0));
        assert_eq!(ring.reserve(0, 3).unwrap().collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(
            ring.reserve(0, 1).unwrap_err(),
            RingFull { needed: 1, free: 0 }
        );
        // Hardware consumed two; the batch wraps.
        assert_eq!(ring.reserve(2, 2).unwrap().collect::<Vec<_>>(), [3, 0]);
        assert_eq!(ring.tail(), 1);
        assert!(!ring.is_drained(2));
        assert!(ring.is_drained(5));
    }
}
