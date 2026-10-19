//! The interface between the Ferric Oxide kernel's `iommu` scheme and its user-space clients,
//! and the PCI rules that decide which requester IDs a device's DMA can carry.
//!
//! The kernel's `iommu` scheme (`crates/kernel/src/scheme/iommu.rs`, built with `kernel.iommu` set to
//! `auto` or `enforce`) hands out DMA domains. pcid opens the scheme's root as root, which gives
//! it the *attach capability*. Opening [`abi::DOMAIN_PATH`] from that capability creates a domain
//! with empty translation tables, so every DMA from a device in it faults. pcid then moves
//! devices into the domain with [`abi::Request::Attach`] calls and passes the domain's file
//! descriptor to the driver, which records it as its own with [`abi::Request::Bind`]. From then
//! on `memory:translation` answers the driver with IOVAs in the domain, so its device can reach
//! only memory the driver translated. A driver of several functions gets a domain for each and
//! moves the later ones' devices into the first with [`abi::Request::Join`], so a process has
//! one domain. When the last descriptor of a domain is closed its
//! devices are blocked: their DMA faults until pcid attaches them to a new domain.
//!
//! - [`abi`] has the scheme's name and number, its paths, and the encoding of each call in the
//!   call metadata words.
//! - `alias` (with the `alloc` feature) works out the requester IDs a function's DMA can carry
//!   once it has passed the bridges above it, so all of them can be attached together.
//! - [`table`] walks and edits radix I/O page tables (VT-d second-stage and AMD-Vi host
//!   tables), with the entry encoding supplied by the vendor crate (`ferric-vtd`,
//!   `ferric-amdvi`).
//! - [`ring`] keeps the producer side of a ring of entries hardware consumes (VT-d's
//!   invalidation queue, AMD-Vi's command buffer).
//! - `iova` (with the `alloc` feature) allocates naturally aligned IOVA ranges for a domain and
//!   hands out domain IDs, for either vendor.
//! - `irte` (with the `alloc` feature) is pcid's record of the interrupt remapping entries it
//!   made for each driver channel, one per (CPU, vector), freed when the channel closes.
//! - `dma` (with the `alloc` feature) is the kernel's bookkeeping for the IOVA mappings that
//!   `memory:translation` makes in a bound domain: one IOVA per frame range, kept while some
//!   virtual mapping of the driver still reaches it.
//!
//! Specifications: PCI Express Base Specification (requester IDs, ports and the PCI Express
//! capability's device/port type field), and the PCI Express to PCI/PCI-X Bridge Specification
//! revision 1.0, section 2.3 (the requester ID a bridge gives to transactions it forwards
//! upstream for devices on its conventional PCI side). The page-table layout is that of the
//! Intel VT-d Architecture Specification (D51397), section 9.8, and the AMD I/O
//! Virtualization Technology (IOMMU) Specification (#48882), section 2.2.3.
//!
//! # Examples
//!
//! ```
//! use ferric_iommu::abi::{PciDevice, Request};
//!
//! let nic = PciDevice::new(0, 0x03, 0x00, 0).expect("a valid address");
//! let (words, len) = Request::Attach(nic).encode();
//! assert_eq!(Request::decode(&words[..len]), Ok(Request::Attach(nic)));
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod abi;
#[cfg(feature = "alloc")]
pub mod alias;
#[cfg(feature = "alloc")]
pub mod dma;
#[cfg(feature = "alloc")]
pub mod iova;
#[cfg(feature = "alloc")]
pub mod irte;
pub mod ring;
pub mod table;
