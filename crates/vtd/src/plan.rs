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

use core::fmt;

use crate::regs::{Capability, ExtendedCapability, global};
use crate::table::{Agaw, PageSize};

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
    })
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
}
