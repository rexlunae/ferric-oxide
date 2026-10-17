//! Deciding whether, and how, the kernel enables an IOMMU.
//!
//! [`plan_unit`] checks an IOMMU's control and extended feature registers against what the
//! kernel can drive and returns a [`UnitPlan`], or a [`SkipReason`] the kernel logs.
//! [`domain_layout`] picks the page-table layout DMA domains use so that every enabled IOMMU
//! can walk them.
//!
//! Domain IDs: [`IDENTITY_DOMAIN`] tags the pass-through entries of devices no driver has
//! claimed, [`BLOCKED_DOMAIN`] the entries of blocked devices, and DMA domains start at
//! [`FIRST_DMA_DOMAIN`]. Domain 0 is left unused.

use core::fmt;
use core::ops::Range;

use ferric_iommu::table::{LargePages, Levels};

use crate::regs::{ExtendedFeatures, control};

/// The domain ID of pass-through device table entries.
pub const IDENTITY_DOMAIN: u16 = 1;

/// The domain ID of blocked device table entries.
pub const BLOCKED_DOMAIN: u16 = 2;

/// The first domain ID of a DMA domain.
pub const FIRST_DMA_DOMAIN: u16 = 3;

/// One past the last domain ID: the device table entry's field is 16 bits wide.
pub const DOMAIN_ID_LIMIT: u32 = 1 << 16;

/// The HyperTransport reserved range, `0xFD_0000_0000..0x100_0000_0000` (section 2.1.2):
/// requests there are system management and interrupt messages, never translated, so no IOVA
/// is handed out in it.
pub const HT_RESERVED: Range<u64> = 0xfd_0000_0000..0x100_0000_0000;

/// The depth of every DMA domain's page tables: four levels, 48 bits of IOVA, which every
/// IOMMU walks (HATS reports at least 4).
pub const DOMAIN_LEVELS: Levels = Levels::Four;

/// How the kernel will set up one IOMMU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitPlan {
    /// The deepest page tables the IOMMU walks.
    pub max_levels: u8,
    /// INVALIDATE_IOMMU_ALL is available (IASup).
    pub invalidate_all: bool,
    /// x2APIC interrupt destinations are available (XTSup).
    pub x2apic: bool,
}

/// Why the kernel leaves an IOMMU alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// Firmware left translation on (pre-boot DMA protection); taking over a live IOMMU is not
    /// supported yet.
    AlreadyEnabled,
    /// The extended feature register reports a reserved page-table depth.
    UnknownDepth,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyEnabled => f.write_str("firmware left translation enabled"),
            Self::UnknownDepth => f.write_str("the IOMMU reports a reserved page-table depth"),
        }
    }
}

impl core::error::Error for SkipReason {}

/// Checks whether the kernel can enable an IOMMU whose control register reads `control` and
/// extended feature register reads `efr`.
///
/// # Errors
///
/// The [`SkipReason`] when it cannot.
pub fn plan_unit(control: u64, efr: ExtendedFeatures) -> Result<UnitPlan, SkipReason> {
    if control & control::IOMMU_EN != 0 {
        return Err(SkipReason::AlreadyEnabled);
    }
    // Every defined HATS value is at least DOMAIN_LEVELS.
    let max_levels = efr.host_levels().ok_or(SkipReason::UnknownDepth)?;
    Ok(UnitPlan {
        max_levels,
        invalidate_all: efr.invalidate_all(),
        x2apic: efr.x2apic(),
    })
}

/// The page-table layout of DMA domains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DomainLayout {
    /// The tables' depth.
    pub levels: Levels,
    /// The leaf sizes mappings may use: the default page sizes of levels 2 and 3, which every
    /// IOMMU supports.
    pub large_pages: LargePages,
    /// One past the largest IOVA.
    pub iova_limit: u64,
}

/// The layout DMA domains use on IOMMUs with these plans, or `None` when there are none.
pub fn domain_layout(plans: impl IntoIterator<Item = UnitPlan>) -> Option<DomainLayout> {
    plans.into_iter().next().map(|_| DomainLayout {
        levels: DOMAIN_LEVELS,
        large_pages: LargePages::Up1G,
        iova_limit: DOMAIN_LEVELS.limit(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regs::tests::QEMU_EFR;

    #[test]
    fn qemu_unit() {
        let plan = plan_unit(0, QEMU_EFR).unwrap();
        assert_eq!(plan.max_levels, 6);
        assert!(plan.invalidate_all && !plan.x2apic);
        let layout = domain_layout([plan]).unwrap();
        assert_eq!(layout.levels, Levels::Four);
        assert_eq!(layout.iova_limit, 1 << 48);
        assert!(HT_RESERVED.end <= layout.iova_limit);
        assert_eq!(domain_layout([]), None);
    }

    #[test]
    fn skips() {
        assert_eq!(
            plan_unit(control::IOMMU_EN, QEMU_EFR),
            Err(SkipReason::AlreadyEnabled)
        );
        assert_eq!(
            plan_unit(0, ExtendedFeatures(3 << 10)),
            Err(SkipReason::UnknownDepth)
        );
        assert_eq!(plan_unit(0, ExtendedFeatures(0)).unwrap().max_levels, 4);
    }
}
