//! NVMe submission and completion rings in coherent device-visible memory.
//!
//! Completion publication follows NVMe Base Specification 2.1, sections 4.2.1
//! and 4.2.4: the controller updates the phase tag in its final write. Observe
//! that tag before reading the completion payload, which may otherwise still
//! contain the previous ring cycle's command identifier.
//! <https://nvmexpress.org/wp-content/uploads/NVM-Express-Base-Specification-Revision-2.1-2024.08.05-Ratified.pdf>

use std::cell::UnsafeCell;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{fence, Ordering};

use common::dma::Dma;
use common::io::{Io, Mmio};
use simple_endian::Endianize;
use syscall::Result;

/// A submission queue entry.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, packed)]
pub struct NvmeCmd {
    /// Opcode
    pub opcode: u8,
    /// Flags
    pub flags: u8,
    /// Command ID
    pub cid: u16,
    /// Namespace identifier
    pub nsid: u32,
    /// Reserved
    pub _rsvd: u64,
    /// Metadata pointer
    pub mptr: u64,
    /// Data pointer
    pub dptr: [u64; 2],
    /// Command dword 10
    pub cdw10: u32,
    /// Command dword 11
    pub cdw11: u32,
    /// Command dword 12
    pub cdw12: u32,
    /// Command dword 13
    pub cdw13: u32,
    /// Command dword 14
    pub cdw14: u32,
    /// Command dword 15
    pub cdw15: u32,
}

/// A completion queue entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Endianize)]
#[endian(le)]
#[repr(C)]
pub struct NvmeComp {
    /// First command-specific result dword.
    pub command_specific: u32,
    /// Second result dword, unused by the commands currently supported here.
    pub _rsvd: u32,
    /// Submission entries consumed by the controller.
    pub sq_head: u16,
    /// Submission queue that supplied the completed command.
    pub sq_id: u16,
    /// Completed command's identifier within its submission queue.
    pub cid: u16,
    /// Phase tag in bit zero and command status in the remaining bits.
    pub status: u16,
}

const _: () = {
    assert!(size_of::<NvmeCompWire>() == 16);
    assert!(std::mem::offset_of!(NvmeCompWire, status) == 14);
};

/// Acquire publication before reading any of the controller's completion payload.
fn read_published_completion(
    phase: bool,
    read_status: impl FnOnce() -> u16,
    read_entry: impl FnOnce() -> NvmeComp,
) -> Option<NvmeComp> {
    if ((read_status() & 1) != 0) != phase {
        return None;
    }
    // The device publishes the phase last. Do not let payload reads precede it,
    // even on architectures whose ordinary loads are not already ordered.
    fence(Ordering::Acquire);
    Some(read_entry())
}

/// Completion queue
pub struct NvmeCompQueue {
    // Size must be a power of two.
    /// Controller-written little-endian completion slots.
    pub data: Dma<[UnsafeCell<NvmeCompWire>]>,
    /// Next slot the host will consume.
    pub head: u16,
    /// Publication phase expected for the current ring cycle.
    pub phase: bool,
    /// Register acknowledging the host's consumed queue head.
    pub doorbell: &'static mut Mmio<u32>,
    last_doorbell: u16,
}

impl NvmeCompQueue {
    /// Allocate a zeroed completion ring for this queue's doorbell.
    ///
    /// # Errors
    ///
    /// Returns a syscall error if DMA memory cannot be allocated or mapped.
    pub fn new(doorbell: &'static mut Mmio<u32>) -> Result<Self> {
        Ok(Self {
            // SAFETY: Every wire field is an integer with valid all-zero bits.
            // Initial zero phase tags differ from the first expected phase.
            data: unsafe { Dma::zeroed_slice(256)?.assume_init() },
            head: 0,
            phase: true,
            doorbell,
            last_doorbell: 0,
        })
    }

    /// Get a new completion queue entry, or return None if no entry is available yet.
    pub(crate) fn complete(&mut self) -> Option<NvmeComp> {
        let slot = self.data[usize::from(self.head)].get();
        let entry = read_published_completion(
            self.phase,
            || self.head_status(),
            || {
                // SAFETY: The aligned DMA slot is initialized and the matching phase
                // publishes its fields. The controller cannot reuse it before kick.
                unsafe { ptr::read_volatile(slot) }.into()
            },
        )?;
        self.head = (self.head + 1) % (self.data.len() as u16);
        if self.head == 0 {
            self.phase = !self.phase;
        }
        Some(entry)
    }

    fn head_status(&self) -> u16 {
        let slot = self.data[usize::from(self.head)].get();
        // SAFETY: DMA allocations are page-aligned; each 16-byte slot's status
        // is aligned at offset 14. This reads only the device's publication tag.
        unsafe { ptr::read_volatile(ptr::addr_of!((*slot).status)) }.into()
    }

    /// Ferric Oxide: whether an entry is waiting at the head, without taking it (for
    /// `percore`'s stall watchdog).
    #[cfg(feature = "percore")]
    pub(crate) fn has_entry(&self) -> bool {
        ((self.head_status() & 1) != 0) == self.phase
    }

    /// Acknowledge newly consumed completion slots to the controller.
    pub fn kick(&mut self) {
        if self.last_doorbell != self.head {
            self.doorbell.write(u32::from(self.head));
            self.last_doorbell = self.head;
        }
    }
}

/// Submission queue
pub struct NvmeCmdQueue {
    // Size must be a power of 2.
    pub data: Dma<[UnsafeCell<NvmeCmd>]>,
    pub tail: u16,
    pub head: u16,

    doorbell: &'static mut Mmio<u32>,
}

impl NvmeCmdQueue {
    pub fn new(doorbell: &'static mut Mmio<u32>) -> Result<Self> {
        Ok(Self {
            data: unsafe { Dma::zeroed_slice(64)?.assume_init() },
            tail: 0,
            head: 0,
            doorbell,
        })
    }

    /// Whether every slot but one is in use, the most an NVMe submission queue may hold (NVMe
    /// 2.0 base specification, section 3.3.1.5).
    pub fn is_full(&self) -> bool {
        ring_is_full(self.head, self.tail, self.data.len())
    }

    /// Add a new submission command entry to the queue. The caller must ensure that the queue have free
    /// entries; this can be checked using `is_full`.
    pub fn submit_unchecked(&mut self, entry: NvmeCmd) -> u16 {
        unsafe { ptr::write_volatile(self.data[usize::from(self.tail)].get(), entry) }
        self.tail = (self.tail + 1) & (self.data.len() as u16 - 1);
        self.tail
    }

    pub fn kick(&mut self) {
        self.doorbell.write(u32::from(self.tail));
    }
}

/// Whether advancing `tail` in a power-of-two ring of `len` slots would make it equal `head`.
fn ring_is_full(head: u16, tail: u16, len: usize) -> bool {
    (usize::from(tail) + 1) & (len - 1) == usize::from(head)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::mem::size_of;
    use std::ptr;

    use ferric_blkqueue::command_ids::CommandIds;

    use super::{read_published_completion, ring_is_full, NvmeComp, NvmeCompWire};

    fn completion(cid: u16, phase: bool) -> NvmeComp {
        NvmeComp {
            command_specific: 0,
            _rsvd: 0,
            sq_head: 0,
            sq_id: 1,
            cid,
            status: u16::from(phase),
        }
    }

    #[test]
    fn completion_publication_precedes_command_identifier_reads() {
        for phase in [true, false] {
            let mut ids = CommandIds::new();
            let old_id = ids.allocate().unwrap();
            assert!(ids.release(old_id));
            let new_id = ids.allocate().unwrap();
            let slot = Cell::new(completion(old_id, !phase));
            let published = completion(new_id, phase);
            let entry = read_published_completion(
                phase,
                || {
                    // The controller publishes between a speculative payload
                    // read and the phase load, including after a CQ wrap.
                    slot.set(published);
                    published.status
                },
                || slot.get(),
            )
            .unwrap();
            assert_eq!(entry, published);
            assert!(ids.release(entry.cid));
        }
    }

    #[test]
    fn unpublished_completion_does_not_read_payload() {
        for phase in [true, false] {
            assert!(read_published_completion(
                phase,
                || u16::from(!phase),
                || panic!("unpublished payload was read"),
            )
            .is_none());
        }
    }

    #[test]
    fn completion_wire_layout_matches_nvme_dwords() {
        let entry = NvmeComp {
            command_specific: 0x1122_3344,
            _rsvd: 0x5566_7788,
            sq_head: 0x1234,
            sq_id: 0x5678,
            cid: 0x9abc,
            status: 0xdef1,
        };
        let wire = NvmeCompWire::from(entry);
        // SAFETY: The wire record has exactly sixteen initialized bytes, with
        // no padding: two u32 fields followed by four u16 fields.
        let bytes = unsafe {
            std::slice::from_raw_parts(ptr::from_ref(&wire).cast::<u8>(), size_of::<NvmeCompWire>())
        };
        assert_eq!(
            bytes,
            [
                0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55, 0x34, 0x12, 0x78, 0x56, 0xbc, 0x9a,
                0xf1, 0xde
            ]
        );
        assert_eq!(NvmeComp::from(wire), entry);
    }

    #[test]
    fn full_when_tail_is_one_behind_head() {
        assert!(ring_is_full(5, 4, 64));
        assert!(!ring_is_full(5, 5, 64));
        assert!(!ring_is_full(5, 3, 64));
    }

    #[test]
    fn full_when_tail_wraps_to_head() {
        assert!(ring_is_full(0, 63, 64));
        assert!(!ring_is_full(1, 63, 64));
    }
}
