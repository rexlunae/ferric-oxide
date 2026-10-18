//! Invalidation of the unit's caches (section 6.5).
//!
//! A unit caches context entries (the context cache), translations and paging-structure
//! entries (the IOTLB) and, with interrupt remapping, interrupt remapping entries (the
//! interrupt entry cache, IEC). After changing a table, software tells the unit which cached
//! entries to drop.
//!
//! There are two ways to do that:
//!
//! - The **register interface** (6.5.1): the context command register (CCMD, 10.4.7) and the
//!   IOTLB invalidate register pair (10.4.8). One request at a time; software polls the
//!   register until hardware clears the busy bit. It is only available while queued
//!   invalidation is off, and it cannot invalidate the IEC.
//! - **Queued invalidation** (6.5.2): a ring of 128-bit descriptors in memory (the
//!   invalidation queue) with head (IQH) and tail (IQT) registers. Software writes
//!   descriptors at the tail and moves IQT; hardware processes them from the head. An
//!   invalidation wait descriptor (6.5.2.8) writes a status word to memory once everything
//!   before it is done.
//!
//! [`Request`] names one invalidation independently of the interface. It encodes as a
//! queued-invalidation [`Descriptor`] ([`Request::descriptor`]) or as register writes
//! ([`Request::register_command`]). [`Queue`] models the ring's head and tail, so the kernel
//! only has to copy descriptors into memory and write the registers. [`iotlb_flush_for`]
//! picks the narrowest IOTLB invalidation that covers a range of IOVAs.

use core::fmt;

use crate::regs::{Capability, ccmd, iotlb};
use crate::table::ADDR_MASK;
use ferric_iommu::ring::Ring;
/// The slots [`Queue::reserve`] handed out, in order.
pub use ferric_iommu::ring::Slots;

/// One cache invalidation, as either interface can issue it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// Every context-cache entry.
    ContextGlobal,
    /// The context-cache entries tagged with domain `did`.
    ContextDomain {
        /// The domain ID.
        did: u16,
    },
    /// The context-cache entries for one device (or, with `function_mask`, several functions
    /// of it). `did` is the domain ID the context entry held *before* the change.
    ContextDevice {
        /// The domain ID the entry held before it changed.
        did: u16,
        /// The device's source-id ([`crate::Bdf::source_id`]).
        source_id: u16,
        /// FM (2 bits): how many low bits of the function number to ignore. 0 means exactly
        /// this function.
        function_mask: u8,
    },
    /// Every IOTLB entry, in every domain.
    IotlbGlobal,
    /// The IOTLB and paging-structure cache entries of domain `did`.
    IotlbDomain {
        /// The domain ID.
        did: u16,
    },
    /// The IOTLB entries of domain `did` for the `2^address_mask` 4 KiB pages starting at
    /// `addr` (which must be aligned to that many pages). Needs CAP.PSI and
    /// `address_mask <= CAP.MAMV`.
    IotlbPage {
        /// The domain ID.
        did: u16,
        /// The first IOVA, aligned to `4096 << address_mask`.
        addr: u64,
        /// AM: log2 of the number of 4 KiB pages.
        address_mask: u8,
        /// IH: only leaf entries changed, so paging-structure caches may be kept. Must be
        /// false when table pages were freed.
        leaf_only: bool,
    },
    /// Every interrupt entry cache entry (queued invalidation only).
    IecGlobal,
    /// The interrupt entry cache entries for the `2^index_mask` IRTEs starting at `index`
    /// (queued invalidation only).
    IecIndex {
        /// The first interrupt remapping table index.
        index: u16,
        /// IM (5 bits): log2 of the number of entries.
        index_mask: u8,
    },
}

/// The register writes that perform a [`Request`] through the register interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterCommand {
    /// Write this to CCMD, then poll until [`ccmd::ICC`] clears.
    Context(u64),
    /// Write `iva` (if any) to the invalidate address register, then `command` to the IOTLB
    /// invalidate register, then poll until [`iotlb::IVT`] clears.
    Iotlb {
        /// The value for the IVA register, for page-selective invalidations.
        iva: Option<u64>,
        /// The value for the IOTLB invalidate register.
        command: u64,
    },
}

/// A 128-bit queued-invalidation descriptor (section 6.5.2), low word first. Its type is in
/// bits 3:0 of the low word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Descriptor(pub [u64; 2]);

/// Descriptor type 1: context-cache invalidate (6.5.2.1).
pub const TYPE_CONTEXT: u64 = 0x1;
/// Descriptor type 2: IOTLB invalidate (6.5.2.3).
pub const TYPE_IOTLB: u64 = 0x2;
/// Descriptor type 4: interrupt entry cache invalidate (6.5.2.7).
pub const TYPE_IEC: u64 = 0x4;
/// Descriptor type 5: invalidation wait (6.5.2.8).
pub const TYPE_WAIT: u64 = 0x5;

/// Wait descriptor bit 4, IF: raise the invalidation completion event.
pub const WAIT_IF: u64 = 1 << 4;
/// Wait descriptor bit 5, SW: write the status data to the status address.
pub const WAIT_SW: u64 = 1 << 5;
/// Wait descriptor bit 6, FN: fetch no later descriptors until this one completes.
pub const WAIT_FN: u64 = 1 << 6;

/// The granularity field (bits 5:4 of context-cache and IOTLB descriptors, CIRG/IIRG in the
/// registers): 1 global, 2 domain, 3 device (context) or page (IOTLB).
const GRAN_GLOBAL: u64 = 1;
const GRAN_DOMAIN: u64 = 2;
const GRAN_SELECTIVE: u64 = 3;

impl Request {
    /// Whether the request needs queued invalidation (the register interface cannot do it).
    pub const fn needs_queue(self) -> bool {
        matches!(self, Self::IecGlobal | Self::IecIndex { .. })
    }

    /// The queued-invalidation descriptor for this request. IOTLB invalidations drain reads
    /// and writes when the unit supports it (CAP.DRD, CAP.DWD).
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_vtd::invalidation::Request;
    /// use ferric_vtd::regs::Capability;
    ///
    /// let d = Request::ContextDevice { did: 1, source_id: 0x0010, function_mask: 0 }
    ///     .descriptor(Capability(0));
    /// assert_eq!(d.0, [0x0010_0001_0031, 0]);
    /// ```
    pub fn descriptor(self, cap: Capability) -> Descriptor {
        let drain = drain_bits(cap);
        let lo = match self {
            Self::ContextGlobal => TYPE_CONTEXT | (GRAN_GLOBAL << 4),
            Self::ContextDomain { did } => {
                TYPE_CONTEXT | (GRAN_DOMAIN << 4) | (u64::from(did) << 16)
            }
            Self::ContextDevice {
                did,
                source_id,
                function_mask,
            } => {
                TYPE_CONTEXT
                    | (GRAN_SELECTIVE << 4)
                    | (u64::from(did) << 16)
                    | (u64::from(source_id) << 32)
                    | (u64::from(function_mask & 3) << 48)
            }
            Self::IotlbGlobal => TYPE_IOTLB | (GRAN_GLOBAL << 4) | drain,
            Self::IotlbDomain { did } => {
                TYPE_IOTLB | (GRAN_DOMAIN << 4) | drain | (u64::from(did) << 16)
            }
            Self::IotlbPage { did, .. } => {
                TYPE_IOTLB | (GRAN_SELECTIVE << 4) | drain | (u64::from(did) << 16)
            }
            Self::IecGlobal => TYPE_IEC,
            Self::IecIndex { index, index_mask } => {
                TYPE_IEC
                    | (1 << 4)
                    | (u64::from(index_mask & 0x1f) << 27)
                    | (u64::from(index) << 32)
            }
        };
        let hi = match self {
            Self::IotlbPage {
                addr,
                address_mask,
                leaf_only,
                ..
            } => iva(addr, address_mask, leaf_only),
            _ => 0,
        };
        Descriptor([lo, hi])
    }

    /// The register writes for this request, or `None` for requests only queued invalidation
    /// can perform ([`Request::needs_queue`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_vtd::invalidation::{RegisterCommand, Request};
    /// use ferric_vtd::regs::Capability;
    ///
    /// assert_eq!(
    ///     Request::IotlbDomain { did: 5 }.register_command(Capability(0)),
    ///     Some(RegisterCommand::Iotlb { iva: None, command: 0xa000_0005_0000_0000 }),
    /// );
    /// ```
    pub fn register_command(self, cap: Capability) -> Option<RegisterCommand> {
        Some(match self {
            Self::ContextGlobal => RegisterCommand::Context(ccmd::global()),
            Self::ContextDomain { did } => RegisterCommand::Context(ccmd::domain(did)),
            Self::ContextDevice {
                did,
                source_id,
                function_mask,
            } => RegisterCommand::Context(ccmd::device(did, source_id, function_mask)),
            Self::IotlbGlobal => RegisterCommand::Iotlb {
                iva: None,
                command: iotlb::global(cap),
            },
            Self::IotlbDomain { did } => RegisterCommand::Iotlb {
                iva: None,
                command: iotlb::domain(cap, did),
            },
            Self::IotlbPage {
                did,
                addr,
                address_mask,
                leaf_only,
            } => RegisterCommand::Iotlb {
                iva: Some(iva(addr, address_mask, leaf_only)),
                command: iotlb::page(cap, did),
            },
            Self::IecGlobal | Self::IecIndex { .. } => return None,
        })
    }
}

/// DR and DW (bits 7 and 6 of an IOTLB descriptor) as the unit supports them.
const fn drain_bits(cap: Capability) -> u64 {
    let mut v = 0;
    if cap.read_draining() {
        v |= 1 << 7;
    }
    if cap.write_draining() {
        v |= 1 << 6;
    }
    v
}

/// The address word of a page-selective IOTLB invalidation: ADDR in bits 63:12, IH in bit 6,
/// AM in bits 5:0. Used both as the IVA register (10.4.8.2) and as the descriptor's high word.
const fn iva(addr: u64, address_mask: u8, leaf_only: bool) -> u64 {
    (addr & !0xfff) | ((leaf_only as u64) << 6) | (address_mask & 0x3f) as u64
}

impl Descriptor {
    /// An invalidation wait descriptor that writes `data` to the 4-byte-aligned physical
    /// address `status` once every descriptor before it has completed, and holds back later
    /// descriptors until then (SW and FN set).
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_vtd::invalidation::Descriptor;
    /// assert_eq!(Descriptor::wait(0x1234_5678, 1).0, [0x0000_0001_0000_0065, 0x1234_5678]);
    /// ```
    pub const fn wait(status: u64, data: u32) -> Self {
        Self([
            TYPE_WAIT | WAIT_SW | WAIT_FN | ((data as u64) << 32),
            status & !0x3,
        ])
    }

    /// The descriptor type, bits 3:0.
    pub const fn kind(self) -> u64 {
        self.0[0] & 0xf
    }
}

/// The invalidation queue (IQA, 10.4.24; IQH, 10.4.22; IQT, 10.4.23) as software sees it: its
/// size and where the next descriptor goes. Hardware owns the head; software passes in the
/// IQH value it reads.
///
/// One slot always stays empty, so head equal to tail means the queue is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Queue {
    size: QueueSize,
    ring: Ring,
}

/// The queue size field (IQA.QS, bits 2:0): the queue is `2^QS` 4 KiB pages, holding
/// `256 << QS` 128-bit descriptors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueSize(u8);

impl QueueSize {
    /// One page, 256 descriptors.
    pub const ONE_PAGE: Self = Self(0);

    /// The size for `2^qs` pages, or `None` when `qs` is above 7.
    pub const fn new(qs: u8) -> Option<Self> {
        if qs <= 7 { Some(Self(qs)) } else { None }
    }

    /// The number of 4 KiB pages.
    pub const fn pages(self) -> usize {
        1 << self.0
    }

    /// The number of descriptors.
    pub const fn entries(self) -> u32 {
        256 << self.0
    }
}

/// The queue does not have room for a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueFull {
    /// Descriptors in the batch.
    pub needed: u32,
    /// Free slots.
    pub free: u32,
}

impl fmt::Display for QueueFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalidation queue full: {} descriptors, {} free slots",
            self.needed, self.free
        )
    }
}

impl core::error::Error for QueueFull {}

impl Queue {
    /// An empty queue of `size`. Hardware resets IQH to 0 when queued invalidation is enabled,
    /// and software writes IQT as 0 before enabling it ([`Queue::tail_register`]).
    pub const fn new(size: QueueSize) -> Self {
        let ring = match Ring::new(size.entries()) {
            Some(ring) => ring,
            // `QueueSize` only holds powers of two from 256 up.
            None => panic!("queue sizes are powers of two"),
        };
        Self { size, ring }
    }

    /// The queue's size.
    pub const fn size(&self) -> QueueSize {
        self.size
    }

    /// The IQA value for a queue at the 4 KiB-aligned physical address `base`, with 128-bit
    /// descriptors (DW clear, as legacy mode requires).
    pub const fn iqa(&self, base: u64) -> u64 {
        (base & ADDR_MASK) | self.size.0 as u64
    }

    /// The slot the next descriptor goes in.
    pub const fn tail(&self) -> u32 {
        self.ring.tail()
    }

    /// The IQT value: the tail slot in bits 18:4.
    pub const fn tail_register(&self) -> u64 {
        (self.ring.tail() as u64) << 4
    }

    /// The head slot from an IQH value: bits 18:4.
    pub const fn head(&self, iqh: u64) -> u32 {
        ((iqh >> 4) as u32) & (self.size.entries() - 1)
    }

    /// How many descriptors can be added while hardware's head is at `iqh`.
    pub const fn free_slots(&self, iqh: u64) -> u32 {
        self.ring.free_slots(self.head(iqh))
    }

    /// Whether hardware has processed every descriptor written so far.
    pub const fn is_drained(&self, iqh: u64) -> bool {
        self.ring.is_drained(self.head(iqh))
    }

    /// Reserves `count` slots for a batch, moving the tail past them, and returns the slots in
    /// order (they wrap at the end of the ring). Write the descriptors into those slots, then
    /// write [`Queue::tail_register`] to IQT.
    ///
    /// # Errors
    ///
    /// [`QueueFull`] when fewer than `count` slots are free; the tail does not move.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_vtd::invalidation::{Queue, QueueSize};
    ///
    /// let mut q = Queue::new(QueueSize::ONE_PAGE);
    /// let slots: Vec<u32> = q.reserve(0, 2)?.collect();
    /// assert_eq!(slots, [0, 1]);
    /// assert_eq!(q.tail_register(), 0x20);
    /// # Ok::<(), ferric_vtd::invalidation::QueueFull>(())
    /// ```
    pub fn reserve(&mut self, iqh: u64, count: u32) -> Result<Slots, QueueFull> {
        let head = self.head(iqh);
        self.ring.reserve(head, count).map_err(|full| QueueFull {
            needed: full.needed,
            free: full.free,
        })
    }
}

/// An invalidation queue error reported in the fault status register (10.4.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueueError {
    /// IQE, bit 4: hardware found an invalid descriptor at the head; it stops fetching.
    InvalidDescriptor,
    /// ICE, bit 5: a device-TLB invalidation completion was invalid.
    InvalidCompletion,
    /// ITE, bit 6: a device-TLB invalidation timed out.
    TimeOut,
}

impl QueueError {
    /// The FSTS bits that report queue errors.
    pub const FSTS_BITS: u32 = 0x70;

    /// The first queue error set in an FSTS value, if any.
    pub const fn from_fault_status(fsts: u32) -> Option<Self> {
        if fsts & (1 << 4) != 0 {
            Some(Self::InvalidDescriptor)
        } else if fsts & (1 << 5) != 0 {
            Some(Self::InvalidCompletion)
        } else if fsts & (1 << 6) != 0 {
            Some(Self::TimeOut)
        } else {
            None
        }
    }
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidDescriptor => "invalid invalidation descriptor",
            Self::InvalidCompletion => "invalid device-TLB invalidation completion",
            Self::TimeOut => "device-TLB invalidation timed out",
        })
    }
}

impl core::error::Error for QueueError {}

/// The narrowest IOTLB invalidation for domain `did` that covers `len` bytes of IOVA from
/// `iova`: one page-selective invalidation when the unit supports it (CAP.PSI) and the range
/// rounds out to a naturally aligned block of at most `2^MAMV` pages, otherwise
/// domain-selective. `freed_tables` must be true when table pages were removed, so the
/// paging-structure caches are invalidated too.
///
/// # Examples
///
/// ```
/// use ferric_vtd::invalidation::{iotlb_flush_for, Request};
/// use ferric_vtd::regs::Capability;
///
/// // CAP.PSI with MAMV = 18.
/// let cap = Capability((1 << 39) | (18 << 48));
/// assert_eq!(
///     iotlb_flush_for(cap, 7, 0x20_0000, 0x20_0000, false),
///     Request::IotlbPage { did: 7, addr: 0x20_0000, address_mask: 9, leaf_only: true },
/// );
/// assert_eq!(iotlb_flush_for(Capability(0), 7, 0x1000, 0x1000, false), Request::IotlbDomain { did: 7 });
/// ```
pub fn iotlb_flush_for(
    cap: Capability,
    did: u16,
    iova: u64,
    len: u64,
    freed_tables: bool,
) -> Request {
    let domain = Request::IotlbDomain { did };
    if !cap.page_selective_invalidation() || len == 0 {
        return domain;
    }
    let first = iova >> 12;
    let Some(last) = iova.checked_add(len - 1).map(|end| end >> 12) else {
        return domain;
    };
    // The smallest naturally aligned block of pages holding [first, last]: the bits above the
    // highest bit in which first and last differ are shared.
    let address_mask = (64 - (first ^ last).leading_zeros()) as u8;
    if address_mask > cap.max_address_mask() {
        return domain;
    }
    Request::IotlbPage {
        did,
        addr: (first >> address_mask) << (address_mask + 12),
        address_mask,
        leaf_only: !freed_tables,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::regs::tests::QEMU_CAP;

    #[test]
    fn context_descriptors() {
        let cap = QEMU_CAP;
        assert_eq!(Request::ContextGlobal.descriptor(cap).0, [0x11, 0]);
        assert_eq!(
            Request::ContextDomain { did: 0x1234 }.descriptor(cap).0,
            [0x1234_0021, 0]
        );
        let d = Request::ContextDevice {
            did: 2,
            source_id: 0x01f8,
            function_mask: 3,
        }
        .descriptor(cap);
        assert_eq!(d.0, [0x0003_01f8_0002_0031, 0]);
        assert_eq!(d.kind(), TYPE_CONTEXT);
    }

    #[test]
    fn iotlb_descriptors() {
        // QEMU drains reads and writes: DR (bit 7) and DW (bit 6).
        assert_eq!(Request::IotlbGlobal.descriptor(QEMU_CAP).0, [0xd2, 0]);
        assert_eq!(Request::IotlbGlobal.descriptor(Capability(0)).0, [0x12, 0]);
        assert_eq!(
            Request::IotlbDomain { did: 9 }.descriptor(QEMU_CAP).0,
            [0x0009_00e2, 0]
        );
        let page = Request::IotlbPage {
            did: 3,
            addr: 0x8020_0000,
            address_mask: 9,
            leaf_only: true,
        };
        assert_eq!(page.descriptor(Capability(0)).0, [0x0003_0032, 0x8020_0049]);
        assert_eq!(page.descriptor(Capability(0)).kind(), TYPE_IOTLB);
    }

    #[test]
    fn iec_and_wait_descriptors() {
        assert_eq!(Request::IecGlobal.descriptor(QEMU_CAP).0, [0x4, 0]);
        assert_eq!(
            Request::IecIndex {
                index: 0x40,
                index_mask: 2
            }
            .descriptor(QEMU_CAP)
            .0,
            [0x0040_1000_0014, 0]
        );
        assert!(Request::IecGlobal.needs_queue());
        assert_eq!(Request::IecGlobal.register_command(QEMU_CAP), None);
        let w = Descriptor::wait(0xabc7, 0xdead_beef);
        assert_eq!(w.0, [0xdead_beef_0000_0065, 0xabc4]);
        assert_eq!(w.kind(), TYPE_WAIT);
    }

    #[test]
    fn register_commands_match_registers() {
        assert_eq!(
            Request::ContextGlobal.register_command(QEMU_CAP),
            Some(RegisterCommand::Context(ccmd::global()))
        );
        // ICC, CIRG = 11, FM = 1, SID = 0x0018, DID = 2.
        assert_eq!(
            Request::ContextDevice {
                did: 2,
                source_id: 0x18,
                function_mask: 1
            }
            .register_command(QEMU_CAP),
            Some(RegisterCommand::Context(0xe000_0001_0018_0002))
        );
        assert_eq!(
            Request::ContextDomain { did: 4 }.register_command(QEMU_CAP),
            Some(RegisterCommand::Context(0xc000_0000_0000_0004))
        );
        // IVT, IIRG = 11, DR, DW, DID = 3; IVA with IH and AM = 0.
        assert_eq!(
            Request::IotlbPage {
                did: 3,
                addr: 0x5000,
                address_mask: 0,
                leaf_only: true
            }
            .register_command(QEMU_CAP),
            Some(RegisterCommand::Iotlb {
                iva: Some(0x5040),
                command: 0xb003_0003_0000_0000
            })
        );
        assert_eq!(
            Request::IotlbGlobal.register_command(QEMU_CAP),
            Some(RegisterCommand::Iotlb {
                iva: None,
                command: 0x9003_0000_0000_0000
            })
        );
    }

    #[test]
    fn queue_head_and_tail() {
        let mut q = Queue::new(QueueSize::ONE_PAGE);
        assert_eq!(q.iqa(0x1234_5000), 0x1234_5000);
        assert_eq!(q.free_slots(0), 255);
        assert!(q.is_drained(0));
        let slots: std::vec::Vec<u32> = q.reserve(0, 3).unwrap().collect();
        assert_eq!(slots, [0, 1, 2]);
        assert_eq!(q.tail_register(), 0x30);
        assert!(!q.is_drained(0x10));
        assert!(q.is_drained(0x30));
        // Hardware is still at slot 1: slots 1 and 2 are in flight and one stays empty.
        assert_eq!(q.free_slots(0x10), 253);
        assert_eq!(
            q.reserve(0x10, 254).err(),
            Some(QueueFull {
                needed: 254,
                free: 253
            })
        );
        assert_eq!(q.tail(), 3);
    }

    #[test]
    fn queue_wraps() {
        let size = QueueSize::new(1).unwrap();
        assert_eq!((size.pages(), size.entries()), (2, 512));
        assert_eq!(QueueSize::new(8), None);
        let mut q = Queue::new(size);
        assert_eq!(q.iqa(0x8000), 0x8001);
        // Move everything up to slot 510 through the queue.
        q.reserve(0, 510).unwrap();
        let head = 510 << 4;
        assert!(q.is_drained(head));
        let slots: std::vec::Vec<u32> = q.reserve(head, 4).unwrap().collect();
        assert_eq!(slots, [510, 511, 0, 1]);
        assert_eq!(q.tail_register(), 0x20);
        assert_eq!(q.free_slots(head), 507);
        // Bits outside 18:4 of IQH are ignored.
        assert_eq!(q.head(head | 0xf | (1 << 40)), 510);
    }

    #[test]
    fn queue_errors() {
        assert_eq!(QueueError::from_fault_status(0x2), None);
        assert_eq!(
            QueueError::from_fault_status(0x12),
            Some(QueueError::InvalidDescriptor)
        );
        assert_eq!(
            QueueError::from_fault_status(0x20),
            Some(QueueError::InvalidCompletion)
        );
        assert_eq!(
            QueueError::from_fault_status(0x40),
            Some(QueueError::TimeOut)
        );
    }

    #[test]
    fn iotlb_flush_scope() {
        let cap = QEMU_CAP; // PSI, MAMV 18
        let page = |addr, address_mask, leaf_only| Request::IotlbPage {
            did: 1,
            addr,
            address_mask,
            leaf_only,
        };
        assert_eq!(
            iotlb_flush_for(cap, 1, 0x3000, 0x1000, false),
            page(0x3000, 0, true)
        );
        assert_eq!(
            iotlb_flush_for(cap, 1, 0x4000, 0x4000, true),
            page(0x4000, 2, false)
        );
        // 0x3000..0x5000 crosses a 4-page boundary, so it rounds out to the 8-page block at 0.
        assert_eq!(
            iotlb_flush_for(cap, 1, 0x3000, 0x2000, false),
            page(0, 3, true)
        );
        // A 1 GiB block needs AM = 18, the most QEMU allows.
        assert_eq!(
            iotlb_flush_for(cap, 1, 1 << 30, 1 << 30, false),
            page(1 << 30, 18, true)
        );
        assert_eq!(
            iotlb_flush_for(cap, 1, 1 << 30, 2 << 30, false),
            Request::IotlbDomain { did: 1 }
        );
        assert_eq!(
            iotlb_flush_for(cap, 1, u64::MAX - 0xfff, 0x2000, false),
            Request::IotlbDomain { did: 1 }
        );
        assert_eq!(
            iotlb_flush_for(cap, 1, 0, 0, false),
            Request::IotlbDomain { did: 1 }
        );
    }
}
