//! Deciding whether, and how, the kernel enables a unit.
//!
//! The first step of the IOMMU work puts every device in one identity domain: each device's
//! context entry points to second-stage tables that map every IOVA in `[0, top)` to the same
//! physical address, where `top` covers all of RAM, the firmware's reserved ranges and the
//! RMRRs. DMA then behaves exactly as without translation, but the root, context and page
//! tables, the invalidation sequence and the fault reporting all run on real hardware.
//!
//! [`plan_unit`] checks a unit's registers against what the kernel code can drive and returns
//! a [`UnitPlan`], or a [`SkipReason`] the kernel logs.
//!
//! Devices can also be moved out of the identity domain into DMA domains of their own.
//! [`domain_layout`] picks the table layout such domains use so that every enabled unit can
//! walk them, and [`unit_coverage`] with [`select_unit`] find the unit that translates a
//! device's requests (section 8.3, "DMA Remapping Hardware Unit Definition Structure");
//! [`unit_coverage_with`] also resolves scopes that name bridges.

use core::fmt;

use crate::Bdf;
use crate::dmar::{BusRange, Drhd};
use crate::regs::{Capability, ExtendedCapability, global};
use crate::table::{Agaw, LargePages, PageSize};

/// The domain ID of the identity domain. Domain 0 is avoided because units in caching mode
/// reserve it.
pub const IDENTITY_DOMAIN: u16 = 1;

/// How the kernel will set up one unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitPlan {
    /// The address width, and so the number of table levels, for the context entries.
    pub agaw: Agaw,
    /// The leaf page size for the identity map.
    pub page_size: PageSize,
    /// The IOVA range `[0, top)` to identity-map, rounded up to `page_size`.
    pub top: u64,
    /// Table updates must be flushed from the CPU caches (ECAP.C is clear).
    pub flush_cpu_caches: bool,
    /// The write buffer must be flushed after table updates (CAP.RWBF).
    pub flush_write_buffer: bool,
    /// Invalidate through the invalidation queue (ECAP.QI) rather than the registers.
    pub queued_invalidation: bool,
}

/// Why the kernel leaves a unit alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// Firmware already turned translation on; taking over a live unit is not supported yet.
    AlreadyEnabled,
    /// Queued invalidation or interrupt remapping is already on, so the register-based
    /// invalidation this code uses is not allowed.
    QueuedInvalidationOrRemappingOn,
    /// The unit only supports scalable mode.
    ScalableModeOnly,
    /// No supported address width covers the memory that must be mapped.
    AddressWidth {
        /// Bits needed to cover the memory.
        needed: u8,
        /// CAP.SAGAW.
        sagaw: u8,
        /// CAP.MGAW in bits.
        mgaw: u8,
    },
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyEnabled => f.write_str("translation already enabled by firmware"),
            Self::QueuedInvalidationOrRemappingOn => {
                f.write_str("queued invalidation or interrupt remapping already enabled")
            }
            Self::ScalableModeOnly => f.write_str("unit supports only scalable mode"),
            Self::AddressWidth {
                needed,
                sagaw,
                mgaw,
            } => write!(
                f,
                "no address width covers {needed} bits (SAGAW {sagaw:#07b}, MGAW {mgaw})"
            ),
        }
    }
}

impl core::error::Error for SkipReason {}

/// Bits needed to address every byte below `top` (at least 12).
pub fn address_bits(top: u64) -> u8 {
    let bits = 64 - top.saturating_sub(1).leading_zeros();
    // `bits` is at most 64, so the cast cannot truncate.
    (bits as u8).max(12)
}

/// Decides how to enable a unit that must identity-map `[0, top)`, given its CAP, ECAP and
/// GSTS registers.
///
/// # Errors
///
/// A [`SkipReason`] when the unit is in a state or of a kind this code does not drive.
///
/// # Examples
///
/// ```
/// use ferric_vtd::plan::plan_unit;
/// use ferric_vtd::regs::{Capability, ExtendedCapability};
/// use ferric_vtd::table::{Agaw, PageSize};
///
/// // QEMU 8.2 intel-iommu, 4 GiB of physical address space to cover.
/// let plan = plan_unit(Capability(0x00d2_008c_2226_0206), ExtendedCapability(0xf42), 0, 4 << 30)?;
/// assert_eq!(plan.agaw, Agaw::Bits39);
/// assert_eq!(plan.page_size, PageSize::Size2M);
/// assert!(plan.flush_cpu_caches);
/// # Ok::<(), ferric_vtd::plan::SkipReason>(())
/// ```
pub fn plan_unit(
    cap: Capability,
    ecap: ExtendedCapability,
    gsts: u32,
    top: u64,
) -> Result<UnitPlan, SkipReason> {
    if gsts & global::TE != 0 {
        return Err(SkipReason::AlreadyEnabled);
    }
    if gsts & (global::QIE | global::IRE) != 0 {
        return Err(SkipReason::QueuedInvalidationOrRemappingOn);
    }
    let sagaw = cap.supported_agaws();
    if sagaw == 0 && ecap.scalable_mode() {
        return Err(SkipReason::ScalableModeOnly);
    }
    let page_size = if cap.large_pages_2m() {
        PageSize::Size2M
    } else {
        PageSize::Size4K
    };
    let top = top.max(1).next_multiple_of(page_size.bytes());
    let needed = address_bits(top);
    let mgaw = cap.max_guest_address_width();
    let agaw = Agaw::smallest_covering(sagaw, needed)
        .filter(|_| needed <= mgaw)
        .ok_or(SkipReason::AddressWidth {
            needed,
            sagaw,
            mgaw,
        })?;
    Ok(UnitPlan {
        agaw,
        page_size,
        top,
        flush_cpu_caches: !ecap.coherent(),
        flush_write_buffer: cap.required_write_buffer_flush(),
        queued_invalidation: ecap.queued_invalidation(),
    })
}

/// The domain ID the first DMA domain gets; IDs below it are [`IDENTITY_DOMAIN`] and the ID 0
/// that caching-mode units reserve.
pub const FIRST_DMA_DOMAIN: u16 = IDENTITY_DOMAIN + 1;

/// How DMA domains are laid out so that every enabled unit can use them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DomainLayout {
    /// The table width: the smallest one every unit supports.
    pub agaw: Agaw,
    /// The leaf sizes every unit supports.
    pub large_pages: LargePages,
    /// One past the highest IOVA every unit translates: the table width, capped by the
    /// smallest MGAW.
    pub iova_limit: u64,
    /// One past the highest domain ID every unit supports (the smallest CAP.ND count).
    pub domain_id_limit: u32,
    /// Some unit does not snoop CPU caches on page walks (ECAP.C clear), so table updates
    /// must be flushed with `clflush`.
    pub flush_cpu_caches: bool,
}

/// The layout for DMA domains over units with these capabilities, or `None` when there are
/// no units or they share no table width.
///
/// # Examples
///
/// ```
/// use ferric_vtd::plan::domain_layout;
/// use ferric_vtd::regs::{Capability, ExtendedCapability};
/// use ferric_vtd::table::{Agaw, LargePages};
///
/// let qemu = (Capability(0x00d2_008c_2226_0206), ExtendedCapability(0xf42));
/// let layout = domain_layout([qemu]).expect("one unit");
/// assert_eq!(layout.agaw, Agaw::Bits39);
/// assert_eq!(layout.large_pages, LargePages::Up1G);
/// assert_eq!(layout.iova_limit, 1 << 39);
/// ```
pub fn domain_layout(
    units: impl IntoIterator<Item = (Capability, ExtendedCapability)>,
) -> Option<DomainLayout> {
    let mut sagaw = 0x1f;
    let mut mgaw = u8::MAX;
    let mut large_pages = LargePages::Up1G;
    let mut domain_id_limit = u32::MAX;
    let mut flush_cpu_caches = false;
    let mut any = false;
    for (cap, ecap) in units {
        any = true;
        sagaw &= cap.supported_agaws();
        mgaw = mgaw.min(cap.max_guest_address_width());
        large_pages = large_pages.min(crate::table::large_pages(cap));
        domain_id_limit = domain_id_limit.min(cap.domain_count());
        flush_cpu_caches |= !ecap.coherent();
    }
    let agaw = Agaw::ALL
        .into_iter()
        .find(|a| any && a.supported_by(sagaw))?;
    // A unit translates at most MGAW bits even when its tables have more levels.
    let iova_limit = agaw
        .limit()
        .min(1u64.checked_shl(u32::from(mgaw)).unwrap_or(u64::MAX));
    Some(DomainLayout {
        agaw,
        large_pages,
        iova_limit,
        domain_id_limit,
        flush_cpu_caches,
    })
}

/// How a DRHD relates to one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// A device scope names the device directly.
    Listed,
    /// The unit has INCLUDE_PCI_ALL for the device's segment: it serves the device unless
    /// another unit lists it.
    IncludeAll,
    /// The unit has scopes that need configuration-space reads to resolve (bridges, or paths
    /// of more than one hop), so it may serve the device.
    Unknown,
    /// The unit does not serve the device.
    No,
}

/// Whether the unit `drhd` describes serves `bdf` in PCI segment `segment`, without knowing
/// any bridge: scopes whose paths pass through bridges, and sub-hierarchy scopes, make the
/// answer [`Coverage::Unknown`] unless they name `bdf` itself.
pub fn unit_coverage(drhd: &Drhd<'_>, segment: u16, bdf: Bdf) -> Coverage {
    unit_coverage_with(drhd, segment, bdf, |_| None)
}

/// Whether the unit `drhd` describes serves `bdf` in PCI segment `segment`, resolving the
/// bridges in its scopes through `bridge_buses` (see [`DeviceScope::covers`](crate::dmar::DeviceScope::covers)).
pub fn unit_coverage_with(
    drhd: &Drhd<'_>,
    segment: u16,
    bdf: Bdf,
    bridge_buses: impl Fn(Bdf) -> Option<BusRange>,
) -> Coverage {
    if drhd.segment != segment {
        return Coverage::No;
    }
    let mut unknown = false;
    for scope in drhd.scopes.iter() {
        let Ok(scope) = scope else {
            unknown = true;
            break;
        };
        match scope.covers(bdf, &bridge_buses) {
            Some(true) => return Coverage::Listed,
            Some(false) => {}
            None => unknown = true,
        }
    }
    if drhd.include_pci_all() {
        Coverage::IncludeAll
    } else if unknown {
        Coverage::Unknown
    } else {
        Coverage::No
    }
}

/// Why no unit could be picked for a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelectError {
    /// No unit serves the device: its DMA is not remapped.
    NotCovered,
    /// A unit with bridge scopes might serve the device, which only configuration-space reads
    /// can settle.
    Ambiguous,
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotCovered => "no remapping unit serves the device",
            Self::Ambiguous => "a remapping unit's bridge scopes may cover the device",
        })
    }
}

impl core::error::Error for SelectError {}

/// Picks the unit that serves a device, from each unit's [`Coverage`] in DMAR order: the unit
/// that lists it, else the INCLUDE_PCI_ALL unit, unless some unit's scopes cannot be resolved.
/// Returns the index of the unit.
///
/// # Errors
///
/// [`SelectError::NotCovered`] when no unit serves the device, [`SelectError::Ambiguous`] when
/// one that cannot be resolved might.
///
/// # Examples
///
/// ```
/// use ferric_vtd::plan::{select_unit, Coverage};
/// assert_eq!(select_unit([Coverage::No, Coverage::IncludeAll]), Ok(1));
/// assert_eq!(select_unit([Coverage::IncludeAll, Coverage::Listed]), Ok(1));
/// ```
pub fn select_unit(coverage: impl IntoIterator<Item = Coverage>) -> Result<usize, SelectError> {
    let mut include_all = None;
    let mut unknown = false;
    for (i, c) in coverage.into_iter().enumerate() {
        match c {
            Coverage::Listed => return Ok(i),
            Coverage::IncludeAll => include_all = include_all.or(Some(i)),
            Coverage::Unknown => unknown = true,
            Coverage::No => {}
        }
    }
    match (unknown, include_all) {
        (true, _) => Err(SelectError::Ambiguous),
        (false, Some(i)) => Ok(i),
        (false, None) => Err(SelectError::NotCovered),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QEMU_CAP: Capability = Capability(0x00d2_008c_2226_0206);
    const QEMU_ECAP: ExtendedCapability = ExtendedCapability(0xf42);

    #[test]
    fn address_bits_rounds_up() {
        assert_eq!(address_bits(0), 12);
        assert_eq!(address_bits(1 << 32), 32);
        assert_eq!(address_bits((1 << 32) + 1), 33);
        assert_eq!(address_bits(u64::MAX), 64);
    }

    #[test]
    fn qemu_plan() {
        let p = plan_unit(QEMU_CAP, QEMU_ECAP, 0, 0x8000_0001).unwrap();
        assert_eq!(p.agaw, Agaw::Bits39);
        assert_eq!(p.page_size, PageSize::Size2M);
        assert_eq!(p.top, 0x8020_0000);
        assert!(p.flush_cpu_caches);
        assert!(!p.flush_write_buffer);
        assert!(p.queued_invalidation);
    }

    #[test]
    fn skips() {
        assert_eq!(
            plan_unit(QEMU_CAP, QEMU_ECAP, global::TE, 1 << 30),
            Err(SkipReason::AlreadyEnabled)
        );
        assert_eq!(
            plan_unit(QEMU_CAP, QEMU_ECAP, global::QIE, 1 << 30),
            Err(SkipReason::QueuedInvalidationOrRemappingOn)
        );
        // 1 TiB of memory does not fit QEMU's 39-bit tables.
        assert_eq!(
            plan_unit(QEMU_CAP, QEMU_ECAP, 0, 1 << 40),
            Err(SkipReason::AddressWidth {
                needed: 40,
                sagaw: 0b10,
                mgaw: 39
            })
        );
        // A 48-bit SAGAW limited by a 39-bit MGAW.
        let cap = Capability((0b100 << 8) | (38 << 16));
        assert!(matches!(
            plan_unit(cap, ExtendedCapability(1), 0, 1 << 40),
            Err(SkipReason::AddressWidth { .. })
        ));
        assert_eq!(
            plan_unit(Capability(0), ExtendedCapability(1 << 43), 0, 1 << 30),
            Err(SkipReason::ScalableModeOnly)
        );
    }

    #[test]
    fn small_page_fallback_and_coherent_walks() {
        let cap = Capability((0b100 << 8) | (47 << 16) | (1 << 4));
        let p = plan_unit(cap, ExtendedCapability(1), 0, 1 << 40).unwrap();
        assert_eq!(p.agaw, Agaw::Bits48);
        assert_eq!(p.page_size, PageSize::Size4K);
        assert!(!p.flush_cpu_caches);
        assert!(p.flush_write_buffer);
    }

    #[test]
    fn layout_takes_the_common_subset() {
        let qemu = (QEMU_CAP, QEMU_ECAP);
        // A 48/57-bit unit with 2 MiB pages only, coherent walks, 256 domain IDs and MGAW 48.
        let other = (
            Capability((0b1100 << 8) | (47 << 16) | (1 << 34) | 2),
            ExtendedCapability(1),
        );
        let l = domain_layout([other]).unwrap();
        assert_eq!(l.agaw, Agaw::Bits48);
        assert_eq!(l.large_pages, LargePages::Up2M);
        assert_eq!(l.iova_limit, 1 << 48);
        assert_eq!(l.domain_id_limit, 256);
        assert!(!l.flush_cpu_caches);
        // QEMU's 39-bit-only unit shares no width with it.
        assert_eq!(domain_layout([qemu, other]), None);
        let wide = (Capability((0b110 << 8) | (40 << 16)), ExtendedCapability(1));
        let l = domain_layout([qemu, wide]).unwrap();
        assert_eq!(l.agaw, Agaw::Bits39);
        assert_eq!(l.large_pages, LargePages::None);
        assert_eq!(l.domain_id_limit, 16);
        assert!(l.flush_cpu_caches);
        // MGAW below the table width caps the IOVA space.
        let narrow = (Capability((0b100 << 8) | (41 << 16)), ExtendedCapability(1));
        assert_eq!(domain_layout([narrow]).unwrap().iova_limit, 1 << 42);
        assert_eq!(domain_layout([]), None);
    }

    #[test]
    fn coverage_from_qemu_dmar() {
        use crate::dmar::{Dmar, Structure};
        let bytes = include_bytes!("../tests/fixtures/qemu-8.2-q35-intremap-off.dmar");
        let dmar = Dmar::parse(bytes).unwrap();
        let drhd = dmar
            .structures()
            .find_map(|s| match s {
                Ok(Structure::Drhd(d)) => Some(d),
                _ => None,
            })
            .unwrap();
        // QEMU lists each device on bus 0 rather than setting INCLUDE_PCI_ALL.
        let nic = Bdf::new(0, 2, 0);
        assert_eq!(unit_coverage(&drhd, 0, nic), Coverage::Listed);
        assert_eq!(unit_coverage(&drhd, 1, nic), Coverage::No);
        assert_eq!(select_unit([unit_coverage(&drhd, 0, nic)]), Ok(0));
        let empty_slot = Bdf::new(0, 0x1e, 0);
        assert_eq!(unit_coverage(&drhd, 0, empty_slot), Coverage::No);
        assert_eq!(
            select_unit([unit_coverage(&drhd, 0, empty_slot)]),
            Err(SelectError::NotCovered)
        );
    }

    #[test]
    fn unit_selection() {
        use Coverage::*;
        assert_eq!(select_unit([Listed, IncludeAll]), Ok(0));
        assert_eq!(select_unit([Unknown, Listed]), Ok(1));
        assert_eq!(
            select_unit([Unknown, IncludeAll]),
            Err(SelectError::Ambiguous)
        );
        assert_eq!(select_unit([No, No]), Err(SelectError::NotCovered));
        assert_eq!(select_unit([]), Err(SelectError::NotCovered));
    }
}
