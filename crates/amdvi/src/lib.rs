//! AMD I/O Virtualization Technology (AMD-Vi, the AMD IOMMU) for the Ferric Oxide kernel.
//!
//! An AMD-Vi IOMMU sits between PCI devices and memory. For each request it looks the
//! requesting device up in a *device table* of 65 536 entries, indexed by the 16-bit device ID
//! (bus, device and function), and translates the device's DMA address (an IOVA) through the
//! *host page tables* the entry names. Software talks to it through a ring of commands in
//! memory (the *command buffer*) and reads what went wrong from a second ring the IOMMU writes
//! (the *event log*). The firmware describes the IOMMUs in the ACPI IVRS table.
//!
//! This crate holds everything that does not touch the hardware, so it can be tested on the
//! host, the same split as `ferric-vtd` for Intel VT-d:
//!
//! - [`ivrs`] parses the IVRS table: the IOMMU definition blocks (IVHD types 10h, 11h and 40h)
//!   with their device entries (ranges, aliases, special devices, ACPI devices) and the memory
//!   definitions (IVMD) that must stay mapped one-to-one.
//! - [`regs`] names the MMIO registers and encodes and decodes the control, status, extended
//!   feature and ring base registers.
//! - [`dte`] encodes device table entries.
//! - [`table`] encodes host page-table entries; the walk is the radix-table code in
//!   `ferric_iommu::table`, which VT-d shares.
//! - [`command`] encodes commands (COMPLETION_WAIT, INVALIDATE_DEVTAB_ENTRY,
//!   INVALIDATE_IOMMU_PAGES, INVALIDATE_INTERRUPT_TABLE, INVALIDATE_IOMMU_ALL) and models the
//!   command buffer's head and tail.
//! - [`event`] decodes event log entries and models the event log's head and tail.
//! - [`plan`] decides, from an IOMMU's registers, whether and how the kernel can enable it.
//!
//! The kernel side (`crates/kernel/src/arch/x86_64/iommu/amdvi.rs`) maps the registers,
//! allocates the tables and rings and runs the enable sequence. See `io/iommu-design.md` in the
//! project files for the plan.
//!
//! Specification: AMD I/O Virtualization Technology (IOMMU) Specification, publication
//! #48882 (revision 3.x numbering): section 2.2 (device table and I/O page tables), 2.4
//! (commands), 2.5 (event log), chapter 3 (registers) and chapter 5 (IVRS).
//!
//! # Examples
//!
//! ```
//! use ferric_amdvi::ivrs::Ivrs;
//!
//! // QEMU 8.2's q35 IVRS table with `-device amd-iommu,intremap=off`.
//! let bytes = include_bytes!("../tests/fixtures/qemu-8.2-q35-intremap-off.ivrs");
//! let ivrs = Ivrs::parse(bytes)?;
//! let units = ivrs.units().map(|ivhd| ivhd.base).collect::<Vec<_>>();
//! assert_eq!(units, [0xfed8_0000]);
//! # Ok::<(), ferric_amdvi::ivrs::IvrsError>(())
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod command;
pub mod dte;
pub mod event;
pub mod ivrs;
pub mod plan;
pub mod regs;
pub mod table;

use core::fmt;

/// A 16-bit device ID: the PCI requester ID the IOMMU indexes its device table with, bus in
/// bits 15:8, device in 7:3 and function in 2:0 (section 2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub u16);

impl DeviceId {
    /// The device ID of bus `bus`, device `device` (0-31) and function `function` (0-7);
    /// higher bits of `device` and `function` are dropped.
    pub const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self(((bus as u16) << 8) | (((device & 0x1f) as u16) << 3) | (function & 7) as u16)
    }

    /// The bus number.
    pub const fn bus(self) -> u8 {
        (self.0 >> 8) as u8
    }

    /// The device number.
    pub const fn device(self) -> u8 {
        ((self.0 >> 3) & 0x1f) as u8
    }

    /// The function number.
    pub const fn function(self) -> u8 {
        (self.0 & 7) as u8
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}.{}",
            self.bus(),
            self.device(),
            self.function()
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;

    use super::DeviceId;

    #[test]
    fn device_id_fields() {
        let id = DeviceId::new(0x3a, 0x1f, 7);
        assert_eq!(id.0, 0x3aff);
        assert_eq!((id.bus(), id.device(), id.function()), (0x3a, 0x1f, 7));
        assert_eq!(DeviceId(0x0080).to_string(), "00:10.0");
    }
}
