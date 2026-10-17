//! Remapping unit registers (chapter 10).
//!
//! Each unit has a memory-mapped register set, 4 KiB-aligned, at the DRHD's register base.
//! The fixed registers sit at the offsets below (section 10.4, table "Register address map").
//! The IOTLB registers and the fault recording registers sit at offsets the unit reports in
//! [`ExtendedCapability::iotlb_offset`] and [`Capability::fault_recording_offset`].
//!
//! Registers may be read and written 32 or 64 bits at a time, but not in smaller pieces.

/// Version register (32 bits, 10.4.1).
pub const VER: usize = 0x00;
/// Capability register (64 bits, 10.4.2).
pub const CAP: usize = 0x08;
/// Extended capability register (64 bits, 10.4.3).
pub const ECAP: usize = 0x10;
/// Global command register (32 bits, 10.4.4).
pub const GCMD: usize = 0x18;
/// Global status register (32 bits, 10.4.5).
pub const GSTS: usize = 0x1c;
/// Root table address register (64 bits, 10.4.6).
pub const RTADDR: usize = 0x20;
/// Context command register (64 bits, 10.4.7).
pub const CCMD: usize = 0x28;
/// Fault status register (32 bits, 10.4.9).
pub const FSTS: usize = 0x34;
/// Fault event control register (32 bits, 10.4.10).
pub const FECTL: usize = 0x38;
/// Fault event data register (32 bits, 10.4.11).
pub const FEDATA: usize = 0x3c;
/// Fault event address register (32 bits, 10.4.12).
pub const FEADDR: usize = 0x40;
/// Fault event upper address register (32 bits, 10.4.13).
pub const FEUADDR: usize = 0x44;
/// Advanced fault log register (64 bits, 10.4.15).
pub const AFLOG: usize = 0x58;
/// Protected memory enable register (32 bits, 10.4.16).
pub const PMEN: usize = 0x64;
/// Invalidation queue head (64 bits, 10.4.22).
pub const IQH: usize = 0x80;
/// Invalidation queue tail (64 bits, 10.4.23).
pub const IQT: usize = 0x88;
/// Invalidation queue address (64 bits, 10.4.24).
pub const IQA: usize = 0x90;
/// Interrupt remapping table address (64 bits, 10.4.31).
pub const IRTA: usize = 0xb8;

/// Offset of the IOTLB invalidate register within the IOTLB register pair (10.4.8.1). The
/// invalidate-address register (IVA, 10.4.8.2) is at offset 0 of the pair.
pub const IOTLB_REG_IN_PAIR: usize = 8;

/// Version register contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version(pub u32);

impl Version {
    /// Major version, bits 7:4.
    pub const fn major(self) -> u8 {
        ((self.0 >> 4) & 0xf) as u8
    }
    /// Minor version, bits 3:0.
    pub const fn minor(self) -> u8 {
        (self.0 & 0xf) as u8
    }
}

/// Capability register contents (10.4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability(pub u64);

impl Capability {
    const fn bit(self, n: u32) -> bool {
        (self.0 >> n) & 1 != 0
    }
    const fn field(self, lo: u32, width: u32) -> u64 {
        (self.0 >> lo) & ((1 << width) - 1)
    }

    /// ND, bits 2:0: the number of domain IDs the unit supports, `2^(4 + 2 * ND)`.
    pub const fn domain_count(self) -> u32 {
        1 << (4 + 2 * self.field(0, 3) as u32)
    }
    /// RWBF, bit 4: software must flush the write buffer (GCMD.WBF) after changing tables.
    pub const fn required_write_buffer_flush(self) -> bool {
        self.bit(4)
    }
    /// PLMR, bit 5: the unit has a protected low-memory region.
    pub const fn protected_low_memory(self) -> bool {
        self.bit(5)
    }
    /// PHMR, bit 6: the unit has a protected high-memory region.
    pub const fn protected_high_memory(self) -> bool {
        self.bit(6)
    }
    /// CM, bit 7: caching mode. Not-present and invalid entries may be cached too, so
    /// software must invalidate after making an entry present.
    pub const fn caching_mode(self) -> bool {
        self.bit(7)
    }
    /// SAGAW, bits 12:8: the supported second-stage address widths. Bit 1 is 39 bits
    /// (3-level tables), bit 2 is 48 bits (4-level) and bit 3 is 57 bits (5-level).
    pub const fn supported_agaws(self) -> u8 {
        self.field(8, 5) as u8
    }
    /// MGAW, bits 21:16: the maximum input (IOVA) address width, in bits.
    pub const fn max_guest_address_width(self) -> u8 {
        self.field(16, 6) as u8 + 1
    }
    /// ZLR, bit 22: zero-length DMA reads are supported.
    pub const fn zero_length_read(self) -> bool {
        self.bit(22)
    }
    /// FRO, bits 33:24: byte offset of the first fault recording register.
    pub const fn fault_recording_offset(self) -> usize {
        self.field(24, 10) as usize * 16
    }
    /// SLLPS bit 0 (bit 34): second-stage 2 MiB pages.
    pub const fn large_pages_2m(self) -> bool {
        self.bit(34)
    }
    /// SLLPS bit 1 (bit 35): second-stage 1 GiB pages.
    pub const fn large_pages_1g(self) -> bool {
        self.bit(35)
    }
    /// PSI, bit 39: page-selective IOTLB invalidation.
    pub const fn page_selective_invalidation(self) -> bool {
        self.bit(39)
    }
    /// NFR, bits 47:40: the number of fault recording registers (the field plus one).
    pub const fn fault_recording_count(self) -> usize {
        self.field(40, 8) as usize + 1
    }
    /// MAMV, bits 53:48: the largest address mask for page-selective invalidation.
    pub const fn max_address_mask(self) -> u8 {
        self.field(48, 6) as u8
    }
    /// DWD, bit 54: IOTLB invalidations can drain writes.
    pub const fn write_draining(self) -> bool {
        self.bit(54)
    }
    /// DRD, bit 55: IOTLB invalidations can drain reads.
    pub const fn read_draining(self) -> bool {
        self.bit(55)
    }
    /// PI, bit 59: posted interrupts.
    pub const fn posted_interrupts(self) -> bool {
        self.bit(59)
    }
}

/// Extended capability register contents (10.4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtendedCapability(pub u64);

impl ExtendedCapability {
    const fn bit(self, n: u32) -> bool {
        (self.0 >> n) & 1 != 0
    }

    /// C, bit 0: page walks snoop the CPU caches. When clear, software must flush table
    /// updates out of the CPU caches (`clflush`) before the unit can see them.
    pub const fn coherent(self) -> bool {
        self.bit(0)
    }
    /// QI, bit 1: queued invalidation.
    pub const fn queued_invalidation(self) -> bool {
        self.bit(1)
    }
    /// DT, bit 2: device TLBs (ATS).
    pub const fn device_tlb(self) -> bool {
        self.bit(2)
    }
    /// IR, bit 3: interrupt remapping.
    pub const fn interrupt_remapping(self) -> bool {
        self.bit(3)
    }
    /// EIM, bit 4: extended (x2APIC, 32-bit destination) interrupt mode.
    pub const fn extended_interrupt_mode(self) -> bool {
        self.bit(4)
    }
    /// PT, bit 6: pass-through context entries.
    pub const fn pass_through(self) -> bool {
        self.bit(6)
    }
    /// SC, bit 7: snoop control (the SNP bit in page-table entries).
    pub const fn snoop_control(self) -> bool {
        self.bit(7)
    }
    /// IRO, bits 17:8: byte offset of the IOTLB register pair.
    pub const fn iotlb_offset(self) -> usize {
        ((self.0 >> 8) & 0x3ff) as usize * 16
    }
    /// Byte offset of the IOTLB invalidate register (the second register of the pair).
    pub const fn iotlb_invalidate_offset(self) -> usize {
        self.iotlb_offset() + IOTLB_REG_IN_PAIR
    }
    /// SMTS, bit 43: scalable-mode translation.
    pub const fn scalable_mode(self) -> bool {
        self.bit(43)
    }
}

/// Global command (GCMD) and status (GSTS) bits (10.4.4, 10.4.5). Each command bit has the
/// status bit at the same position.
pub mod global {
    /// Translation enable.
    pub const TE: u32 = 1 << 31;
    /// Set root table pointer (one-shot).
    pub const SRTP: u32 = 1 << 30;
    /// Set fault log (one-shot).
    pub const SFL: u32 = 1 << 29;
    /// Enable advanced fault logging.
    pub const EAFL: u32 = 1 << 28;
    /// Write buffer flush (one-shot).
    pub const WBF: u32 = 1 << 27;
    /// Queued invalidation enable.
    pub const QIE: u32 = 1 << 26;
    /// Interrupt remapping enable.
    pub const IRE: u32 = 1 << 25;
    /// Set interrupt remap table pointer (one-shot).
    pub const SIRTP: u32 = 1 << 24;
    /// Compatibility format interrupts allowed (when interrupt remapping is on).
    pub const CFI: u32 = 1 << 23;

    /// The status bits that are states rather than one-shot commands. GCMD has no read-back,
    /// so a command is written as the current state (GSTS masked with this) plus the new bit;
    /// see [`command`].
    pub const PERSISTENT: u32 = TE | EAFL | QIE | IRE | CFI | 0x007f_ffff;

    /// The GCMD value that issues `cmd` while keeping every other enabled state in `gsts`.
    ///
    /// ```
    /// use ferric_vtd::regs::global::{command, SRTP, TE};
    /// // With translation on and the root-table-pointer status set, issuing SRTP keeps TE on
    /// // and does not re-issue the old one-shot.
    /// assert_eq!(command(TE | SRTP, SRTP), TE | SRTP);
    /// assert_eq!(command(SRTP, TE), TE);
    /// ```
    pub const fn command(gsts: u32, cmd: u32) -> u32 {
        (gsts & PERSISTENT) | cmd
    }
}

/// Root table address register: legacy-mode translation table format (bits 11:10 = 00).
pub const RTADDR_TTM_LEGACY: u64 = 0;

/// Context command register (10.4.7).
pub mod ccmd {
    /// ICC, bit 63: invalidate; hardware clears it when done.
    pub const ICC: u64 = 1 << 63;
    /// CIRG = 01 (bits 62:61): global invalidation.
    pub const GLOBAL: u64 = 1 << 61;
    /// CIRG = 10: domain-selective invalidation.
    pub const DOMAIN: u64 = 2 << 61;
    /// CIRG = 11: device-selective invalidation.
    pub const DEVICE: u64 = 3 << 61;

    /// The command for a global context-cache invalidation.
    pub const fn global() -> u64 {
        ICC | GLOBAL
    }

    /// The command for a domain-selective context-cache invalidation (DID in bits 15:0).
    pub const fn domain(did: u16) -> u64 {
        ICC | DOMAIN | did as u64
    }

    /// The command for a device-selective context-cache invalidation: SID in bits 31:16, FM
    /// (function mask, 2 bits) in bits 33:32, and the domain ID the entry held before the
    /// change in bits 15:0.
    pub const fn device(did: u16, source_id: u16, function_mask: u8) -> u64 {
        ICC | DEVICE
            | (((function_mask & 3) as u64) << 32)
            | ((source_id as u64) << 16)
            | did as u64
    }
}

/// IOTLB invalidate register (10.4.8.1).
pub mod iotlb {
    use super::Capability;

    /// IVT, bit 63: invalidate; hardware clears it when done.
    pub const IVT: u64 = 1 << 63;
    /// IIRG = 01 (bits 61:60): global invalidation.
    pub const GLOBAL: u64 = 1 << 60;
    /// IIRG = 10: domain-selective invalidation (DID in bits 47:32).
    pub const DOMAIN: u64 = 2 << 60;
    /// IIRG = 11: page-selective invalidation within a domain.
    pub const PAGE: u64 = 3 << 60;
    /// DR, bit 49: drain reads.
    pub const DR: u64 = 1 << 49;
    /// DW, bit 48: drain writes.
    pub const DW: u64 = 1 << 48;

    /// The command for a global IOTLB invalidation, draining reads and writes where the unit
    /// supports it.
    pub const fn global(cap: Capability) -> u64 {
        let mut v = IVT | GLOBAL;
        if cap.read_draining() {
            v |= DR;
        }
        if cap.write_draining() {
            v |= DW;
        }
        v
    }

    /// The command for a domain-selective IOTLB invalidation.
    pub const fn domain(cap: Capability, did: u16) -> u64 {
        (global(cap) & !GLOBAL) | DOMAIN | ((did as u64) << 32)
    }

    /// The command for a page-selective IOTLB invalidation in domain `did`. The pages are
    /// named by the invalidate address register (IVA), written first.
    pub const fn page(cap: Capability, did: u16) -> u64 {
        (global(cap) & !GLOBAL) | PAGE | ((did as u64) << 32)
    }
}

/// Fault event control register (10.4.10).
pub mod fectl {
    /// IM, bit 31: interrupt mask.
    pub const IM: u32 = 1 << 31;
    /// IP, bit 30: interrupt pending (read-only).
    pub const IP: u32 = 1 << 30;
}

/// Protected memory enable register (10.4.16).
pub mod pmen {
    /// EPM, bit 31: enable protected memory regions.
    pub const EPM: u32 = 1 << 31;
    /// PRS, bit 0: protected region status.
    pub const PRS: u32 = 1;
}

/// How many bytes of register space a unit uses: the fixed registers, the IOTLB pair and all
/// fault recording registers, rounded up to whole 4 KiB pages, and at least `drhd_pages` pages
/// (the DRHD's size field, [`crate::dmar::Drhd::register_pages`]).
pub fn register_span(cap: Capability, ecap: ExtendedCapability, drhd_pages: u32) -> usize {
    let iotlb_end = ecap.iotlb_offset() + 16;
    let fault_end = cap.fault_recording_offset() + 16 * cap.fault_recording_count();
    let reported = drhd_pages as usize * 4096;
    iotlb_end
        .max(fault_end)
        .max(reported)
        .next_multiple_of(4096)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// CAP and ECAP read from QEMU 8.2.2's intel-iommu with `intremap=off` (39-bit width).
    pub(crate) const QEMU_CAP: Capability = Capability(0x00d2_008c_2226_0206);
    pub(crate) const QEMU_ECAP: ExtendedCapability = ExtendedCapability(0xf42);
    /// The same with `intremap=on,device-iotlb=on,aw-bits=48`.
    const QEMU_CAP_48: Capability = Capability(0x00d2_008c_222f_0606);
    const QEMU_ECAP_IR: ExtendedCapability = ExtendedCapability(0x00f0_0f4e);

    #[test]
    fn qemu_capabilities() {
        let cap = QEMU_CAP;
        assert_eq!(cap.domain_count(), 65536);
        assert!(!cap.required_write_buffer_flush());
        assert!(!cap.caching_mode());
        assert_eq!(cap.supported_agaws(), 0b10);
        assert_eq!(cap.max_guest_address_width(), 39);
        assert_eq!(cap.fault_recording_offset(), 0x220);
        assert_eq!(cap.fault_recording_count(), 1);
        assert!(cap.large_pages_2m() && cap.large_pages_1g());
        assert!(cap.page_selective_invalidation());
        assert_eq!(cap.max_address_mask(), 18);
        assert!(cap.read_draining() && cap.write_draining());

        let ecap = QEMU_ECAP;
        assert!(!ecap.coherent());
        assert!(ecap.queued_invalidation());
        assert!(!ecap.device_tlb());
        assert!(!ecap.interrupt_remapping());
        assert!(ecap.pass_through());
        assert_eq!(ecap.iotlb_offset(), 0xf0);
        assert_eq!(register_span(cap, ecap, 1), 4096);
        assert_eq!(register_span(cap, ecap, 4), 4 * 4096);
        assert_eq!(ecap.iotlb_invalidate_offset(), 0xf8);

        assert_eq!(QEMU_CAP_48.supported_agaws(), 0b110);
        assert_eq!(QEMU_CAP_48.max_guest_address_width(), 48);
        assert!(QEMU_ECAP_IR.interrupt_remapping() && QEMU_ECAP_IR.device_tlb());
        assert!(!QEMU_ECAP_IR.extended_interrupt_mode());
    }

    #[test]
    fn register_span_covers_far_fault_records() {
        // FRO = 0x200 (offset 0x2000), 8 records.
        let cap = Capability((0x200 << 24) | (7 << 40));
        assert_eq!(register_span(cap, ExtendedCapability(0x50 << 8), 1), 0x3000);
    }

    #[test]
    fn version_and_commands() {
        assert_eq!((Version(0x10).major(), Version(0x10).minor()), (1, 0));
        assert_eq!(ccmd::global(), 0xa000_0000_0000_0000);
        assert_eq!(iotlb::global(QEMU_CAP), 0x9003_0000_0000_0000);
        assert_eq!(iotlb::global(Capability(0)), 0x9000_0000_0000_0000);
        assert_eq!(iotlb::domain(Capability(0), 5), 0xa000_0005_0000_0000);
        assert_eq!(global::PERSISTENT, 0x96ff_ffff);
    }
}
