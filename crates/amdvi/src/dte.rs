//! Device table entries (section 2.2.2).
//!
//! The device table holds one 256-bit [`DeviceTableEntry`] for each of the 65 536 device IDs
//! ([`DEVICE_TABLE_BYTES`], 2 MiB). The fields this crate sets:
//!
//! - word 0: V (bit 0, the entry is valid), TV (bit 1, the translation fields are valid), Mode
//!   (bits 11:9, the number of page-table levels, 0 for no translation), the host page-table
//!   root (bits 51:12), and IR and IW (bits 61, 62), which allow reads and writes at all;
//! - word 1: the domain ID (bits 15:0), which tags the IOMMU's cached translations, and SysMgt
//!   (bits 41:40) from the IVHD's DTE setting;
//! - word 2: the interrupt pass-through bits from the IVHD's DTE setting (56-58, 62, 63). IV
//!   (bit 0) stays clear: interrupt remapping is not used yet, and the IOMMU forwards the
//!   device's interrupts unchanged.
//!
//! Three kinds of entry are used:
//!
//! - [`DeviceTableEntry::passthrough`]: Mode 0 with IR and IW, so DMA is not translated, the
//!   identity domain of `kernel.iommu = "auto"`;
//! - [`DeviceTableEntry::translated`]: Mode 3, 4 or 5 with IR and IW, so the page tables alone
//!   decide, a DMA domain;
//! - [`DeviceTableEntry::blocked`]: translated through a table with nothing in it and IR and IW
//!   clear, so every request faults. A Mode 0 entry with IR and IW clear would block too, but a
//!   page-table walk that finds nothing is the case every implementation reports.
//!
//! An entry the IOMMU may be using is changed by first storing a blocked word 0 (one aligned
//! 64-bit store, so the IOMMU never sees half of it), then invalidating the entry and the old
//! domain's pages, then writing words 3 to 1 and finally word 0 ([`DeviceTableEntry::words`]
//! lists them in that order), and invalidating again.

use ferric_iommu::table::{ADDR_MASK, Levels};

use crate::ivrs::DteSetting;

/// Bytes in a device table that covers every device ID.
pub const DEVICE_TABLE_BYTES: usize = 65_536 * 32;

/// Word 0 bit 0: the entry is valid.
const V: u64 = 1 << 0;
/// Word 0 bit 1: the translation fields are valid.
const TV: u64 = 1 << 1;
/// Word 0 bits 11:9: the paging mode.
const MODE_SHIFT: u32 = 9;
/// Word 0 bit 61: reads allowed.
const IR: u64 = 1 << 61;
/// Word 0 bit 62: writes allowed.
const IW: u64 = 1 << 62;

/// A device table entry, as its four 64-bit words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DeviceTableEntry(pub [u64; 4]);

impl DeviceTableEntry {
    /// An entry with V clear: the IOMMU does not translate or check the device's requests. The
    /// state firmware leaves the table in, never written by this crate's users.
    pub const INVALID: Self = Self([0; 4]);

    /// DMA from the device is not translated (Mode 0) but allowed, in domain `domain`.
    pub const fn passthrough(domain: u16, setting: DteSetting) -> Self {
        Self::with(V | TV | IR | IW, domain, setting)
    }

    /// DMA from the device is translated through the host page tables at `root`, which have
    /// `levels` levels, in domain `domain`.
    pub const fn translated(root: u64, levels: Levels, domain: u16, setting: DteSetting) -> Self {
        let word0 = V | TV | ((levels.count() as u64) << MODE_SHIFT) | (root & ADDR_MASK) | IR | IW;
        Self::with(word0, domain, setting)
    }

    /// Every DMA from the device faults: it is translated through `empty_root`, a table with
    /// no present entry of `levels` levels, with reads and writes refused, in domain `domain`.
    pub const fn blocked(
        empty_root: u64,
        levels: Levels,
        domain: u16,
        setting: DteSetting,
    ) -> Self {
        let word0 = V | TV | ((levels.count() as u64) << MODE_SHIFT) | (empty_root & ADDR_MASK);
        Self::with(word0, domain, setting)
    }

    const fn with(word0: u64, domain: u16, setting: DteSetting) -> Self {
        let word1 = domain as u64 | ((setting.sys_mgt() as u64) << 40);
        let mut word2 = 0;
        if setting.init_pass() {
            word2 |= 1 << 56;
        }
        if setting.eint_pass() {
            word2 |= 1 << 57;
        }
        if setting.nmi_pass() {
            word2 |= 1 << 58;
        }
        if setting.lint0_pass() {
            word2 |= 1 << 62;
        }
        if setting.lint1_pass() {
            word2 |= 1 << 63;
        }
        Self([word0, word1, word2, 0])
    }

    /// V: the entry is valid.
    pub const fn is_valid(self) -> bool {
        self.0[0] & V != 0
    }

    /// TV: the translation fields are valid.
    pub const fn translation_valid(self) -> bool {
        self.0[0] & TV != 0
    }

    /// The paging mode: 0 for no translation, otherwise the number of levels.
    pub const fn mode(self) -> u8 {
        ((self.0[0] >> MODE_SHIFT) & 7) as u8
    }

    /// The host page-table root.
    pub const fn root(self) -> u64 {
        self.0[0] & ADDR_MASK
    }

    /// The domain ID.
    pub const fn domain_id(self) -> u16 {
        self.0[1] as u16
    }

    /// IR: reads are allowed.
    pub const fn read(self) -> bool {
        self.0[0] & IR != 0
    }

    /// IW: writes are allowed.
    pub const fn write(self) -> bool {
        self.0[0] & IW != 0
    }

    /// Whether DMA from the device passes untranslated: an invalid entry, or a valid one with
    /// Mode 0 and both permissions.
    pub const fn is_passthrough(self) -> bool {
        !self.is_valid() || (self.mode() == 0 && self.read() && self.write())
    }

    /// Whether every DMA from the device is refused: neither reads nor writes are allowed, or
    /// the translation goes through `empty_root`.
    pub const fn is_blocked(self, empty_root: u64) -> bool {
        self.is_valid()
            && self.translation_valid()
            && ((!self.read() && !self.write()) || (self.mode() != 0 && self.root() == empty_root))
    }

    /// The entry's words with their indices, in the order to store them: word 3 first and
    /// word 0, which holds V, TV, Mode and the root, last.
    pub const fn words(self) -> [(usize, u64); 4] {
        [
            (3, self.0[3]),
            (2, self.0[2]),
            (1, self.0[1]),
            (0, self.0[0]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        let none = DteSetting(0);
        let pt = DeviceTableEntry::passthrough(1, none);
        assert_eq!(pt.0, [0x6000_0000_0000_0003, 1, 0, 0]);
        assert!(pt.is_passthrough() && !pt.is_blocked(0));
        let dom = DeviceTableEntry::translated(0x1234_5000, Levels::Four, 7, none);
        assert_eq!(dom.0, [0x6000_0000_1234_5803, 7, 0, 0]);
        assert_eq!(
            (dom.mode(), dom.root(), dom.domain_id()),
            (4, 0x1234_5000, 7)
        );
        assert!(!dom.is_passthrough() && !dom.is_blocked(0x9000));
        let blocked = DeviceTableEntry::blocked(0x9000, Levels::Four, 2, none);
        assert_eq!(blocked.0, [0x9803, 2, 0, 0]);
        assert!(blocked.is_blocked(0x9000) && !blocked.read() && !blocked.write());
        assert!(DeviceTableEntry::INVALID.is_passthrough());
        assert_eq!(DEVICE_TABLE_BYTES, 2 << 20);
    }

    #[test]
    fn ivhd_settings_are_copied() {
        // INITPass, NMIPass, SysMgt 2, Lint1Pass.
        let setting = DteSetting(0b1010_0101);
        let e = DeviceTableEntry::passthrough(3, setting);
        assert_eq!(e.0[1], 3 | (2 << 40));
        assert_eq!(e.0[2], (1 << 56) | (1 << 58) | (1 << 63));
        assert_eq!(e.words()[3], (0, e.0[0]));
        assert_eq!(e.words()[0], (3, 0));
    }
}
