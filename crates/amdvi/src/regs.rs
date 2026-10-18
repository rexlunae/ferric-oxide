//! MMIO registers (section 3.4).
//!
//! The IOMMU's registers sit at the base address its IVHD names. The kernel uses the first
//! 16 KiB ([`MMIO_SPAN`]): the device table, command buffer and event log base registers, the
//! control register, the extended feature register, and the ring head and tail pointers and
//! the status register at 2000h.

/// Bytes of the register set the kernel maps.
pub const MMIO_SPAN: usize = 0x4000;

/// Device Table Base Address register: base in bits 51:12, size in bits 8:0.
pub const DEVICE_TABLE_BASE: usize = 0x0000;
/// Command Buffer Base Address register: base in bits 51:12, ComLen in bits 59:56.
pub const COMMAND_BUFFER_BASE: usize = 0x0008;
/// Event Log Base Address register: base in bits 51:12, EventLen in bits 59:56.
pub const EVENT_LOG_BASE: usize = 0x0010;
/// IOMMU Control register.
pub const CONTROL: usize = 0x0018;
/// Exclusion Base register.
pub const EXCLUSION_BASE: usize = 0x0020;
/// Exclusion Range Limit register.
pub const EXCLUSION_LIMIT: usize = 0x0028;
/// Extended Feature register.
pub const EXTENDED_FEATURES: usize = 0x0030;
/// Command Buffer Head Pointer register: byte offset in bits 18:4.
pub const COMMAND_HEAD: usize = 0x2000;
/// Command Buffer Tail Pointer register.
pub const COMMAND_TAIL: usize = 0x2008;
/// Event Log Head Pointer register.
pub const EVENT_HEAD: usize = 0x2010;
/// Event Log Tail Pointer register.
pub const EVENT_TAIL: usize = 0x2018;
/// IOMMU Status register.
pub const STATUS: usize = 0x2020;

/// Bits 51:12 of a base register or table entry: a 4 KiB-aligned physical address.
pub const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// The bits of a ring head or tail pointer: a byte offset of a 16-byte entry.
pub const RING_POINTER_MASK: u64 = 0x0007_fff0;

/// Control register bits (section 3.4.3).
pub mod control {
    /// Bit 0, IommuEn: translation and the rings are on.
    pub const IOMMU_EN: u64 = 1 << 0;
    /// Bit 1, HtTunEn.
    pub const HT_TUN_EN: u64 = 1 << 1;
    /// Bit 2, EventLogEn.
    pub const EVENT_LOG_EN: u64 = 1 << 2;
    /// Bit 3, EventIntEn: an interrupt when an event is logged.
    pub const EVENT_INT_EN: u64 = 1 << 3;
    /// Bit 4, ComWaitIntEn: an interrupt when a COMPLETION_WAIT with I set completes.
    pub const COM_WAIT_INT_EN: u64 = 1 << 4;
    /// Bit 8, PassPW.
    pub const PASS_PW: u64 = 1 << 8;
    /// Bit 9, ResPassPW.
    pub const RES_PASS_PW: u64 = 1 << 9;
    /// Bit 10, Coherent: device table reads are snooped.
    pub const COHERENT: u64 = 1 << 10;
    /// Bit 11, Isoc.
    pub const ISOC: u64 = 1 << 11;
    /// Bit 12, CmdBufEn.
    pub const CMD_BUF_EN: u64 = 1 << 12;
    /// Bit 13, PPRLogEn.
    pub const PPR_LOG_EN: u64 = 1 << 13;
    /// Bit 15, PPREn.
    pub const PPR_EN: u64 = 1 << 15;
    /// Bit 16, GTEn: guest translation.
    pub const GT_EN: u64 = 1 << 16;
    /// Bit 17, GAEn: guest virtual APIC.
    pub const GA_EN: u64 = 1 << 17;
}

/// Status register bits (section 3.4.13). The overflow and interrupt bits are cleared by
/// writing 1.
pub mod status {
    /// Bit 0, EventOverflow: the event log was full and events were lost.
    pub const EVENT_OVERFLOW: u64 = 1 << 0;
    /// Bit 1, EventLogInt: an event was logged.
    pub const EVENT_LOG_INT: u64 = 1 << 1;
    /// Bit 2, ComWaitInt: a COMPLETION_WAIT with I set completed.
    pub const COM_WAIT_INT: u64 = 1 << 2;
    /// Bit 3, EventLogRun: the event log is running.
    pub const EVENT_LOG_RUN: u64 = 1 << 3;
    /// Bit 4, CmdBufRun: the command buffer is running; it stops on an illegal command.
    pub const CMD_BUF_RUN: u64 = 1 << 4;
    /// The bits cleared by writing 1 that the kernel acknowledges.
    pub const CLEAR: u64 = EVENT_OVERFLOW | EVENT_LOG_INT | COM_WAIT_INT;
}

/// The control register value that enables an IOMMU with the command buffer and the event log,
/// carrying over the IVHD's tunnel, posted-write and isochronous flags. Device table reads are
/// made coherent. Guest translation, guest APIC, PPR and interrupt remapping stay off.
pub const fn enable_control(
    flags: crate::ivrs::IvhdFlags,
    event_interrupt: bool,
    current: u64,
) -> u64 {
    let keep = current
        & !(control::IOMMU_EN
            | control::HT_TUN_EN
            | control::EVENT_LOG_EN
            | control::EVENT_INT_EN
            | control::COM_WAIT_INT_EN
            | control::PASS_PW
            | control::RES_PASS_PW
            | control::ISOC
            | control::CMD_BUF_EN
            | control::PPR_LOG_EN
            | control::PPR_EN
            | control::GT_EN
            | control::GA_EN);
    let mut value =
        keep | control::IOMMU_EN | control::EVENT_LOG_EN | control::CMD_BUF_EN | control::COHERENT;
    if event_interrupt {
        value |= control::EVENT_INT_EN;
    }
    if flags.ht_tunnel() {
        value |= control::HT_TUN_EN;
    }
    if flags.pass_pw() {
        value |= control::PASS_PW;
    }
    if flags.res_pass_pw() {
        value |= control::RES_PASS_PW;
    }
    if flags.isoc() {
        value |= control::ISOC;
    }
    value
}

/// The number of entries in a command buffer or event log: a power of two from 256 (one 4 KiB
/// page of 16-byte entries) to 32768.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingSize {
    log2: u8,
}

impl RingSize {
    /// 256 entries, one page.
    pub const ONE_PAGE: Self = Self { log2: 8 };

    /// `2^log2` entries, for `log2` in 8..=15.
    pub const fn new(log2: u8) -> Option<Self> {
        if log2 >= 8 && log2 <= 15 {
            Some(Self { log2 })
        } else {
            None
        }
    }

    /// The number of entries.
    pub const fn entries(self) -> u32 {
        1 << self.log2
    }

    /// The size in bytes.
    pub const fn bytes(self) -> usize {
        16 << self.log2
    }

    /// The ComLen or EventLen field value.
    pub const fn length_field(self) -> u64 {
        self.log2 as u64
    }
}

/// The Device Table Base Address register value for a table at `phys` covering every device ID
/// (2 MiB; the size field counts 4 KiB pages minus one).
pub const fn device_table_base(phys: u64) -> u64 {
    (phys & ADDR_MASK) | 0x1ff
}

/// The Command Buffer or Event Log Base Address register value for a ring of `size` at `phys`.
pub const fn ring_base(phys: u64, size: RingSize) -> u64 {
    (phys & ADDR_MASK) | (size.length_field() << 56)
}

/// The Extended Feature register (section 3.4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtendedFeatures(pub u64);

impl ExtendedFeatures {
    /// Bit 0, PreFSup: the prefetch command.
    pub const fn prefetch(self) -> bool {
        self.0 & 1 != 0
    }

    /// Bit 1, PPRSup: peripheral page requests.
    pub const fn ppr(self) -> bool {
        self.0 & 2 != 0
    }

    /// Bit 2, XTSup: x2APIC interrupt destinations.
    pub const fn x2apic(self) -> bool {
        self.0 & 4 != 0
    }

    /// Bit 3, NXSup: no-execute protection.
    pub const fn no_execute(self) -> bool {
        self.0 & 8 != 0
    }

    /// Bit 4, GTSup: guest translation.
    pub const fn guest_translation(self) -> bool {
        self.0 & 0x10 != 0
    }

    /// Bit 6, IASup: the INVALIDATE_IOMMU_ALL command.
    pub const fn invalidate_all(self) -> bool {
        self.0 & 0x40 != 0
    }

    /// Bit 7, GASup: guest virtual APIC.
    pub const fn guest_apic(self) -> bool {
        self.0 & 0x80 != 0
    }

    /// Bit 8, HESup: hardware error registers.
    pub const fn hardware_errors(self) -> bool {
        self.0 & 0x100 != 0
    }

    /// Bit 9, PCSup: performance counters.
    pub const fn performance_counters(self) -> bool {
        self.0 & 0x200 != 0
    }

    /// Bits 11:10, HATS: the deepest host page tables the IOMMU walks, as levels (4, 5 or 6),
    /// or `None` for the reserved encoding.
    pub const fn host_levels(self) -> Option<u8> {
        match (self.0 >> 10) & 3 {
            0 => Some(4),
            1 => Some(5),
            2 => Some(6),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ivrs::IvhdFlags;

    /// QEMU 8.2's and 10.2's extended feature register (bit 2 set with `xtsup=on` on 10.2).
    pub(crate) const QEMU_EFR: ExtendedFeatures = ExtendedFeatures(0x29d3);

    #[test]
    fn qemu_features() {
        let efr = QEMU_EFR;
        assert!(efr.prefetch() && efr.ppr() && efr.invalidate_all());
        assert!(!efr.x2apic() && !efr.no_execute() && !efr.performance_counters());
        assert!(efr.guest_translation() && efr.guest_apic() && efr.hardware_errors());
        assert_eq!(efr.host_levels(), Some(6));
        assert!(ExtendedFeatures(0x29d7).x2apic());
        assert_eq!(ExtendedFeatures(3 << 10).host_levels(), None);
    }

    #[test]
    fn base_registers() {
        assert_eq!(device_table_base(0x1_2340_0000), 0x1_2340_01ff);
        assert_eq!(RingSize::ONE_PAGE.bytes(), 4096);
        assert_eq!(RingSize::new(13).unwrap().entries(), 8192);
        assert_eq!(RingSize::new(7), None);
        assert_eq!(RingSize::new(16), None);
        assert_eq!(
            ring_base(0x90_0000, RingSize::ONE_PAGE),
            0x0800_0000_0090_0000
        );
    }

    #[test]
    fn control_value() {
        // QEMU's IVHD flags: HtTunEn, IotlbSup, PreFSup, PPRSup.
        let flags = IvhdFlags(0xd1);
        let v = enable_control(flags, true, 0);
        assert_eq!(
            v,
            control::IOMMU_EN
                | control::HT_TUN_EN
                | control::EVENT_LOG_EN
                | control::EVENT_INT_EN
                | control::CMD_BUF_EN
                | control::COHERENT
        );
        // Firmware's invalidation timeout (bits 7:5) is kept; PPR and guest bits are cleared.
        let v = enable_control(
            IvhdFlags(0),
            false,
            0b101 << 5 | control::PPR_EN | control::GT_EN,
        );
        assert_eq!(
            v,
            0b101 << 5
                | control::IOMMU_EN
                | control::EVENT_LOG_EN
                | control::CMD_BUF_EN
                | control::COHERENT
        );
    }
}
