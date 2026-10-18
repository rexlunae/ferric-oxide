//! Commands (section 2.4) and the command buffer.
//!
//! The command buffer is a ring of 128-bit [`Command`]s in memory ([`regs::RingSize`]
//! entries). Software writes commands at the tail and moves the tail pointer register past them;
//! the IOMMU executes them in order from the head. Each command's opcode is in bits 63:60 of
//! its first word. The ones the kernel uses:
//!
//! - [`Command::completion_wait`] (01h): once every earlier command has completed, the IOMMU
//!   stores a 64-bit value at an address, which software polls;
//! - [`Command::invalidate_devtab_entry`] (02h): drop the cached device table entry of one
//!   device ID;
//! - [`Command::invalidate_iommu_pages`] (03h): drop a domain's cached translations for a range
//!   of IOVAs ([`PageRange`]), with PDE set so cached table entries go too;
//! - [`Command::invalidate_interrupt_table`] (05h): drop a device's cached interrupt remapping
//!   entries;
//! - [`Command::invalidate_all`] (08h, when the extended feature register has IASup): drop
//!   everything.
//!
//! The ring itself is `ferric_iommu::ring::Ring`, which VT-d's invalidation queue uses too;
//! [`CommandBuffer`] adds the head and tail register encoding.
//!
//! [`regs::RingSize`]: crate::regs::RingSize

use core::fmt;

use ferric_iommu::ring::{Ring, RingFull, Slots};

use crate::DeviceId;
use crate::regs::{RING_POINTER_MASK, RingSize};

/// Opcode of COMPLETION_WAIT.
pub const COMPLETION_WAIT: u8 = 0x01;
/// Opcode of INVALIDATE_DEVTAB_ENTRY.
pub const INVALIDATE_DEVTAB_ENTRY: u8 = 0x02;
/// Opcode of INVALIDATE_IOMMU_PAGES.
pub const INVALIDATE_IOMMU_PAGES: u8 = 0x03;
/// Opcode of INVALIDATE_INTERRUPT_TABLE.
pub const INVALIDATE_INTERRUPT_TABLE: u8 = 0x05;
/// Opcode of INVALIDATE_IOMMU_ALL.
pub const INVALIDATE_IOMMU_ALL: u8 = 0x08;

/// INVALIDATE_IOMMU_PAGES word 1 bit 0, S: the address encodes a range of pages.
const S: u64 = 1 << 0;
/// INVALIDATE_IOMMU_PAGES word 1 bit 1, PDE: cached table entries are invalidated too.
const PDE: u64 = 1 << 1;

/// The largest IOVA range one INVALIDATE_IOMMU_PAGES with S set names before it is easier to
/// name everything.
const ALL_PAGES: u64 = 0x7fff_ffff_ffff_f000;

/// One 128-bit command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command(pub [u64; 2]);

/// The pages an INVALIDATE_IOMMU_PAGES names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageRange {
    /// Every page of the domain.
    All,
    /// The naturally aligned block of `2^log2_bytes` bytes at `base`; `log2_bytes` is from 12
    /// (one page) to 63.
    Block {
        /// The first IOVA, aligned to the block size.
        base: u64,
        /// log2 of the block size.
        log2_bytes: u8,
    },
}

impl PageRange {
    /// The smallest range that covers `len` bytes from `iova` (at least the page at `iova`).
    pub const fn covering(iova: u64, len: u64) -> Self {
        let start = iova & !0xfff;
        let last = match iova.checked_add(if len == 0 { 1 } else { len }) {
            Some(end) => end - 1,
            None => return Self::All,
        };
        // The block is the smallest power of two in which start and last agree above it.
        let differ = start ^ (last & !0xfff);
        let log2 = if differ == 0 {
            12
        } else {
            64 - differ.leading_zeros() as u8
        };
        if log2 >= 52 {
            Self::All
        } else {
            Self::Block {
                base: start & !((1u64 << log2) - 1),
                log2_bytes: if log2 < 12 { 12 } else { log2 },
            }
        }
    }

    /// Word 1 of the command, without PDE: the address and S.
    const fn operand(self) -> u64 {
        match self {
            Self::All => ALL_PAGES | S,
            Self::Block {
                base,
                log2_bytes: 12,
            } => base & !0xfff,
            // With S set, the lowest clear address bit from bit 12 up, n, makes the range
            // 2^(n+1) bytes: so bits 12 to log2 - 2 are set and bit log2 - 1 is clear.
            Self::Block { base, log2_bytes } => {
                let ones = ((1u64 << (log2_bytes - 1)) - 1) & !0xfff;
                (base & !((1u64 << log2_bytes) - 1)) | ones | S
            }
        }
    }
}

impl Command {
    /// The opcode, bits 63:60 of word 0.
    pub const fn opcode(self) -> u8 {
        (self.0[0] >> 60) as u8
    }

    const fn with_opcode(opcode: u8, word0: u64, word1: u64) -> Self {
        Self([word0 | ((opcode as u64) << 60), word1])
    }

    /// COMPLETION_WAIT with S set: when every earlier command has completed, the IOMMU writes
    /// `data` to the 8-byte-aligned physical address `store`.
    pub const fn completion_wait(store: u64, data: u64) -> Self {
        Self::with_opcode(COMPLETION_WAIT, (store & 0x000f_ffff_ffff_fff8) | 1, data)
    }

    /// INVALIDATE_DEVTAB_ENTRY for `device`.
    pub const fn invalidate_devtab_entry(device: DeviceId) -> Self {
        Self::with_opcode(INVALIDATE_DEVTAB_ENTRY, device.0 as u64, 0)
    }

    /// INVALIDATE_IOMMU_PAGES for `domain` and `range`, with PDE set, so cached table entries
    /// on the way to those pages go too.
    pub const fn invalidate_iommu_pages(domain: u16, range: PageRange) -> Self {
        Self::with_opcode(
            INVALIDATE_IOMMU_PAGES,
            (domain as u64) << 32,
            range.operand() | PDE,
        )
    }

    /// INVALIDATE_INTERRUPT_TABLE for `device`.
    pub const fn invalidate_interrupt_table(device: DeviceId) -> Self {
        Self::with_opcode(INVALIDATE_INTERRUPT_TABLE, device.0 as u64, 0)
    }

    /// INVALIDATE_IOMMU_ALL (needs IASup).
    pub const fn invalidate_all() -> Self {
        Self::with_opcode(INVALIDATE_IOMMU_ALL, 0, 0)
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.opcode() {
            COMPLETION_WAIT => "COMPLETION_WAIT",
            INVALIDATE_DEVTAB_ENTRY => "INVALIDATE_DEVTAB_ENTRY",
            INVALIDATE_IOMMU_PAGES => "INVALIDATE_IOMMU_PAGES",
            INVALIDATE_INTERRUPT_TABLE => "INVALIDATE_INTERRUPT_TABLE",
            INVALIDATE_IOMMU_ALL => "INVALIDATE_IOMMU_ALL",
            _ => "command",
        };
        write!(f, "{name} {:#018x}:{:#018x}", self.0[0], self.0[1])
    }
}

/// The command buffer's tail, and the encoding of its pointer registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandBuffer {
    size: RingSize,
    ring: Ring,
}

impl CommandBuffer {
    /// An empty buffer of `size`. The head and tail registers are written as 0 before the
    /// buffer is enabled.
    pub const fn new(size: RingSize) -> Self {
        let ring = match Ring::new(size.entries()) {
            Some(ring) => ring,
            // `RingSize` only holds powers of two from 256 up.
            None => panic!("ring sizes are powers of two"),
        };
        Self { size, ring }
    }

    /// The buffer's size.
    pub const fn size(&self) -> RingSize {
        self.size
    }

    /// The head slot from a head pointer register value.
    pub const fn head(&self, register: u64) -> u32 {
        ((register & RING_POINTER_MASK) >> 4) as u32 & (self.size.entries() - 1)
    }

    /// The tail pointer register value.
    pub const fn tail_register(&self) -> u64 {
        (self.ring.tail() as u64) << 4
    }

    /// Whether the IOMMU has fetched every command written so far.
    pub const fn is_drained(&self, head_register: u64) -> bool {
        self.ring.is_drained(self.head(head_register))
    }

    /// Reserves `count` slots, moving the tail past them, while the head pointer register reads
    /// `head_register`. Write the commands into the slots, then write
    /// [`CommandBuffer::tail_register`] to the tail pointer register.
    ///
    /// # Errors
    ///
    /// [`RingFull`] when fewer than `count` slots are free; the tail does not move.
    pub fn reserve(&mut self, head_register: u64, count: u32) -> Result<Slots, RingFull> {
        self.ring.reserve(self.head(head_register), count)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    #[test]
    fn encodings_qemu_accepted() {
        // The words the QEMU probe sent and QEMU executed.
        assert_eq!(
            Command::completion_wait(0x90_2000, 0x1234_abcd).0,
            [0x1000_0000_0090_2001, 0x1234_abcd]
        );
        assert_eq!(
            Command::invalidate_devtab_entry(DeviceId(0x80)).0,
            [0x2000_0000_0000_0080, 0]
        );
        assert_eq!(
            Command::invalidate_iommu_pages(7, PageRange::All).0,
            [0x3000_0007_0000_0000, 0x7fff_ffff_ffff_f003]
        );
        assert_eq!(Command::invalidate_all().0, [0x8000_0000_0000_0000, 0]);
        assert_eq!(
            Command::invalidate_interrupt_table(DeviceId(0xa0)).opcode(),
            INVALIDATE_INTERRUPT_TABLE
        );
    }

    #[test]
    fn page_ranges() {
        let one = PageRange::covering(0x1_2345, 1);
        assert_eq!(
            one,
            PageRange::Block {
                base: 0x1_2000,
                log2_bytes: 12
            }
        );
        // One page: S clear.
        assert_eq!(Command::invalidate_iommu_pages(1, one).0[1], 0x1_2000 | PDE);
        // Two pages from an even page: 8 KiB, bit 12 clear.
        let two = PageRange::covering(0x4000, 0x2000);
        assert_eq!(
            two,
            PageRange::Block {
                base: 0x4000,
                log2_bytes: 13
            }
        );
        assert_eq!(two.operand(), 0x4000 | S);
        // 16 KiB from 0x8000: bit 12 set, bit 13 clear.
        let four = PageRange::covering(0x8000, 0x4000);
        assert_eq!(four.operand(), 0x9000 | S);
        // A range across a 16 KiB boundary takes the enclosing 32 KiB block.
        let across = PageRange::covering(0x3000, 0x2000);
        assert_eq!(
            across,
            PageRange::Block {
                base: 0,
                log2_bytes: 15
            }
        );
        assert_eq!(across.operand(), 0x3000 | S);
        // 2 MiB at 2 MiB.
        assert_eq!(
            PageRange::covering(0x20_0000, 0x20_0000).operand(),
            0x20_0000 | 0xf_f000 | S
        );
        assert_eq!(PageRange::covering(0, 1 << 60), PageRange::All);
        assert_eq!(PageRange::covering(u64::MAX - 5, 10), PageRange::All);
        assert_eq!(
            PageRange::covering(0x5000, 0),
            PageRange::Block {
                base: 0x5000,
                log2_bytes: 12
            }
        );
    }

    #[test]
    fn ring_registers() {
        let mut buf = CommandBuffer::new(RingSize::ONE_PAGE);
        assert!(buf.is_drained(0));
        let slots = buf.reserve(0, 3).unwrap().collect::<Vec<_>>();
        assert_eq!(slots, [0, 1, 2]);
        assert_eq!(buf.tail_register(), 0x30);
        assert!(!buf.is_drained(0x10));
        assert!(buf.is_drained(0x30));
        // Upper bits of the head register are reserved.
        assert_eq!(buf.head(0xffff_0000_0000_0030), 3);
        assert_eq!(buf.reserve(0x30, 256).unwrap_err().free, 255);
    }
}
