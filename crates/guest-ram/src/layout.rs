//! Where a guest's RAM sits in its physical address space, and where each part is in the
//! RAM file.

/// 4 GiB: where RAM above the PCI hole continues.
pub const FOUR_GIB: u64 = 1 << 32;

/// On q35, RAM of this size or more is split at [`Q35_LOW_SPLIT`]; smaller RAM lies wholly
/// below 4 GiB.
pub const Q35_SPLIT_THRESHOLD: u64 = 0xb000_0000;

/// Where q35 ends RAM below 4 GiB when it splits it: 2 GiB.
pub const Q35_LOW_SPLIT: u64 = 0x8000_0000;

/// A guest's RAM as one file: the first [`RamLayout::below_4g`] bytes at guest-physical 0,
/// the rest at [`FOUR_GIB`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamLayout {
    ram: u64,
    below_4g: u64,
}

/// Why a split is not a RAM layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// More RAM below 4 GiB than there is RAM, or than fits below 4 GiB.
    #[error("{below_4g:#x} bytes below 4 GiB of {ram:#x} bytes of RAM")]
    Split {
        /// The RAM's size.
        ram: u64,
        /// The bytes asked for below 4 GiB.
        below_4g: u64,
    },
    /// The RAM above 4 GiB would end past the 64-bit address space.
    #[error("{ram:#x} bytes of RAM do not fit in the address space")]
    TooLarge {
        /// The RAM's size.
        ram: u64,
    },
}

impl RamLayout {
    /// The layout of `ram` bytes on QEMU's q35 machine with its default
    /// `max-ram-below-4g`: wholly below 4 GiB when `ram` is below [`Q35_SPLIT_THRESHOLD`]
    /// (2.75 GiB), otherwise [`Q35_LOW_SPLIT`] (2 GiB) below and the rest from 4 GiB. This is
    /// QEMU's `pc_q35_init` rule.
    pub const fn q35(ram: u64) -> Self {
        let below_4g = if ram >= Q35_SPLIT_THRESHOLD {
            Q35_LOW_SPLIT
        } else {
            ram
        };
        Self { ram, below_4g }
    }

    /// `ram` bytes with the first `below_4g` at guest-physical 0 and the rest from 4 GiB, for
    /// a machine whose split is not q35's default.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Split`] when `below_4g` is more than `ram` or than 4 GiB, and
    /// [`LayoutError::TooLarge`] when the RAM above 4 GiB would pass the end of the address
    /// space.
    pub fn split(ram: u64, below_4g: u64) -> Result<Self, LayoutError> {
        if below_4g > ram || below_4g > FOUR_GIB {
            return Err(LayoutError::Split { ram, below_4g });
        }
        FOUR_GIB
            .checked_add(ram - below_4g)
            .ok_or(LayoutError::TooLarge { ram })?;
        Ok(Self { ram, below_4g })
    }

    /// The RAM's size: the bytes of the file that are RAM.
    pub const fn ram_bytes(&self) -> u64 {
        self.ram
    }

    /// The bytes of RAM below 4 GiB, at guest-physical 0.
    pub const fn below_4g(&self) -> u64 {
        self.below_4g
    }

    /// The file offset of the `len` bytes at guest-physical `phys`, or `None` unless RAM holds
    /// all of them in one piece.
    pub fn offset_of(&self, phys: u64, len: u64) -> Option<u64> {
        let end = phys.checked_add(len)?;
        if end <= self.below_4g {
            return Some(phys);
        }
        let above = self.ram - self.below_4g;
        let offset = phys.checked_sub(FOUR_GIB)?;
        (offset.checked_add(len)? <= above).then(|| self.below_4g + offset)
    }

    /// The guest-physical address of file offset `offset`, or `None` past the RAM.
    pub fn phys_of(&self, offset: u64) -> Option<u64> {
        if offset < self.below_4g {
            Some(offset)
        } else if offset < self.ram {
            FOUR_GIB.checked_add(offset - self.below_4g)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn q35_keeps_small_ram_below_4g() {
        for ram in [512 * MIB, 2048 * MIB, Q35_SPLIT_THRESHOLD - 4096] {
            let layout = RamLayout::q35(ram);
            assert_eq!(layout.below_4g(), ram);
            assert_eq!(layout.offset_of(ram - 4096, 4096), Some(ram - 4096));
            assert_eq!(layout.offset_of(ram - 4096, 4097), None, "past the end");
            assert_eq!(layout.offset_of(FOUR_GIB, 8), None, "nothing above 4 GiB");
            assert_eq!(layout.phys_of(ram - 8), Some(ram - 8));
            assert_eq!(layout.phys_of(ram), None);
        }
    }

    #[test]
    fn q35_splits_large_ram_at_2_gib() {
        for ram in [Q35_SPLIT_THRESHOLD, 4096 * MIB, 16384 * MIB] {
            let layout = RamLayout::q35(ram);
            assert_eq!(layout.below_4g(), Q35_LOW_SPLIT);
            let above = ram - Q35_LOW_SPLIT;
            assert_eq!(
                layout.offset_of(Q35_LOW_SPLIT - 8, 8),
                Some(Q35_LOW_SPLIT - 8)
            );
            assert_eq!(
                layout.offset_of(Q35_LOW_SPLIT - 8, 16),
                None,
                "into the hole"
            );
            assert_eq!(layout.offset_of(Q35_LOW_SPLIT, 8), None, "the hole");
            assert_eq!(layout.offset_of(FOUR_GIB, 8), Some(Q35_LOW_SPLIT));
            assert_eq!(
                layout.offset_of(FOUR_GIB + above - 8, 8),
                Some(ram - 8),
                "the last word"
            );
            assert_eq!(layout.offset_of(FOUR_GIB + above - 8, 9), None);
            assert_eq!(layout.phys_of(Q35_LOW_SPLIT), Some(FOUR_GIB));
            assert_eq!(layout.phys_of(ram - 8), Some(FOUR_GIB + above - 8));
            assert_eq!(layout.phys_of(ram), None);
        }
    }

    #[test]
    fn offsets_and_addresses_are_inverse() {
        let layout = RamLayout::q35(4096 * MIB);
        for offset in (0..layout.ram_bytes()).step_by(97 * 4096 * 1024 + 8) {
            let phys = layout.phys_of(offset).expect("in RAM");
            assert_eq!(layout.offset_of(phys, 8), Some(offset));
        }
    }

    #[test]
    fn explicit_splits_are_checked() {
        let layout = RamLayout::split(3072 * MIB, 3072 * MIB).expect("all below 4 GiB");
        assert_eq!(layout.offset_of(0xbfff_fff8, 8), Some(0xbfff_fff8));
        assert_eq!(
            RamLayout::split(1024 * MIB, 2048 * MIB),
            Err(LayoutError::Split {
                ram: 1024 * MIB,
                below_4g: 2048 * MIB
            })
        );
        assert_eq!(
            RamLayout::split(8 * FOUR_GIB, 2 * FOUR_GIB),
            Err(LayoutError::Split {
                ram: 8 * FOUR_GIB,
                below_4g: 2 * FOUR_GIB
            })
        );
        assert_eq!(
            RamLayout::split(u64::MAX, 0),
            Err(LayoutError::TooLarge { ram: u64::MAX })
        );
        assert_eq!(RamLayout::q35(u64::MAX).offset_of(u64::MAX, 1), None);
    }
}
