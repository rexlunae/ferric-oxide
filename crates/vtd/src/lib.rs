//! Intel Virtualization Technology for Directed I/O (VT-d) for the Ferric Oxide kernel.
//!
//! VT-d puts DMA remapping hardware units ("IOMMUs") between PCI devices and memory. Each unit
//! looks up the requesting device in a root table and a context table, and translates the
//! device's DMA address (an IOVA) through second-stage page tables, much like the CPU's MMU.
//!
//! This crate holds everything that does not touch the hardware, so it can be tested on the
//! host:
//!
//! - [`dmar`] parses the ACPI DMAR table: the remapping units (DRHD), reserved memory regions
//!   (RMRR), ATS reporting (ATSR) and the device scopes inside them.
//! - [`regs`] names the register offsets and decodes the capability and status registers.
//! - [`table`] encodes root, context and second-stage page-table entries and builds mappings
//!   in memory provided through the [`table::TableMemory`] trait.
//! - [`fault`] decodes the fault status and fault recording registers.
//! - [`plan`] decides, from a unit's registers, whether and how the kernel can enable it.
//! - [`invalidation`] encodes context-cache, IOTLB and interrupt entry cache invalidations,
//!   as queued-invalidation descriptors or register writes, and models the invalidation
//!   queue's head and tail.
//! - [`interrupt`] encodes interrupt remapping table entries, remappable-format MSI and I/O
//!   APIC messages and the IRTA register, and decides whether a unit can remap interrupts.
//! - `iova` (with the `alloc` feature, on by default) allocates naturally aligned IOVA ranges
//!   for a domain and hands out domain IDs.
//!
//! The kernel side (`kernel/src/arch/x86_64/iommu.rs`) maps the registers, allocates the table
//! pages and performs the enable sequence. See `io/iommu-design.md` in the project files for
//! the plan this is the first step of.
//!
//! Specification: Intel Virtualization Technology for Directed I/O, Architecture
//! Specification (order number D51397). Section numbers below are those of revisions 3.x and
//! 4.x: chapter 8 (DMAR ACPI structures), chapter 9 (translation structure formats), chapter 10
//! (register descriptions), section 6.5 (invalidation), chapter 7 and appendix A (faults), chapter 5 and section 9.10 (interrupt remapping).
//! Revision 4 renamed "second-level" to "second-stage"; the formats are unchanged.
//!
//! # Examples
//!
//! ```
//! use ferric_vtd::dmar::{Dmar, Structure};
//!
//! // QEMU 8.2's q35 DMAR table with `-device intel-iommu,intremap=off`.
//! let bytes = include_bytes!("../tests/fixtures/qemu-8.2-q35-intremap-off.dmar");
//! let dmar = Dmar::parse(bytes)?;
//! assert_eq!(dmar.host_address_width(), 39);
//! let units = dmar
//!     .structures()
//!     .filter_map(|s| match s {
//!         Ok(Structure::Drhd(drhd)) => Some(drhd.register_base),
//!         _ => None,
//!     })
//!     .collect::<Vec<_>>();
//! assert_eq!(units, [0xfed9_0000]);
//! # Ok::<(), ferric_vtd::dmar::DmarError>(())
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod dmar;
pub mod fault;
pub mod interrupt;
pub mod invalidation;
#[cfg(feature = "alloc")]
pub mod iova;
pub mod plan;
pub mod regs;
pub mod table;

use core::fmt;

/// A PCI requester ID within one segment: bus, device and function, as VT-d's source-id field
/// (section 3.4.1) carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bdf {
    /// Bus number.
    pub bus: u8,
    /// Device number (0-31).
    pub device: u8,
    /// Function number (0-7).
    pub function: u8,
}

impl Bdf {
    /// Builds a BDF, keeping only the valid bits of `device` (5) and `function` (3).
    pub const fn new(bus: u8, device: u8, function: u8) -> Self {
        Self {
            bus,
            device: device & 0x1f,
            function: function & 0x7,
        }
    }

    /// Decodes a 16-bit source-id: bus in bits 15:8, device in 7:3, function in 2:0.
    pub const fn from_source_id(sid: u16) -> Self {
        Self::new((sid >> 8) as u8, (sid >> 3) as u8, sid as u8)
    }

    /// Encodes this BDF as a 16-bit source-id.
    pub const fn source_id(self) -> u16 {
        ((self.bus as u16) << 8) | ((self.device as u16) << 3) | self.function as u16
    }

    /// The device-and-function byte that indexes a context table (section 9.3).
    pub const fn devfn(self) -> u8 {
        (self.device << 3) | self.function
    }
}

impl fmt::Display for Bdf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

#[cfg(test)]
mod tests {
    use super::Bdf;

    #[test]
    fn bdf_round_trips_through_source_id() {
        let bdf = Bdf::new(0x3a, 0x1f, 7);
        assert_eq!(bdf.source_id(), 0x3aff);
        assert_eq!(Bdf::from_source_id(0x3aff), bdf);
        assert_eq!(Bdf::from_source_id(0x0008), Bdf::new(0, 1, 0));
        assert_eq!(bdf.devfn(), 0xff);
    }

    #[test]
    fn bdf_display_matches_lspci() {
        extern crate std;
        use std::string::ToString;
        assert_eq!(Bdf::new(0, 0x1f, 3).to_string(), "00:1f.3");
    }
}
