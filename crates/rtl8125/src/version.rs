//! MAC identification for the RTL8125 family.
//!
//! Realtek keeps a MAC version code in the upper bits of the transmit
//! configuration register (`0x0040`). The FreeBSD driver reads that register
//! and matches the value against a mask that ignores the configuration bits
//! below bit 16; the same mask identifies the RTL8168 generations, so the
//! register is shared between the families.
//!
//! The value read from the RTL8125B is `0x6410_0000`, which is the XID `641`
//! that the Linux driver prints. The "v2" register at `0x60B0` that the
//! reference driver uses belongs to a later generation of the family and does
//! not exist on the RTL8125B.

/// A MAC version recognised from the transmit configuration register.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MacVersion {
    /// RTL8125A (MACFG 82).
    Rtl8125A,
    /// RTL8125B (MACFG 83).
    Rtl8125B,
}

/// Mask applied to the transmit configuration value before matching.
pub const VERSION_MASK: u32 = 0xFCF0_0000;

/// Value of the transmit configuration register on the RTL8125A.
pub const RTL8125A_TXCFG: u32 = 0x6400_0000;

/// Value of the transmit configuration register on the RTL8125B.
pub const RTL8125B_TXCFG: u32 = 0x6410_0000;

/// Identify the MAC from the value read at register `0x0040`.
///
/// The lower bits carry the transmit configuration and are ignored. A value of
/// `0xFFFF_FFFF`, which is what an unclocked or unresponsive function returns,
/// does not match any version.
#[must_use]
pub const fn detect(txcfg: u32) -> Option<MacVersion> {
    match txcfg & VERSION_MASK {
        RTL8125A_TXCFG => Some(MacVersion::Rtl8125A),
        RTL8125B_TXCFG => Some(MacVersion::Rtl8125B),
        _ => None,
    }
}

/// Whether this driver implements the initialisation sequence for the version.
///
/// The A variant needs a different MAC and PHY patch table, so it is recognised
/// but rejected by the driver instead of being probed with the B sequence.
#[must_use]
pub const fn is_supported(version: MacVersion) -> bool {
    matches!(version, MacVersion::Rtl8125B)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_8125b_with_configuration_bits() {
        assert_eq!(detect(RTL8125B_TXCFG), Some(MacVersion::Rtl8125B));
        assert_eq!(detect(RTL8125B_TXCFG | 0x0780), Some(MacVersion::Rtl8125B));
        assert!(is_supported(MacVersion::Rtl8125B));
    }

    #[test]
    fn detects_8125a_but_does_not_claim_support() {
        assert_eq!(detect(RTL8125A_TXCFG), Some(MacVersion::Rtl8125A));
        assert!(!is_supported(MacVersion::Rtl8125A));
    }

    #[test]
    fn rejects_other_and_unresponsive_values() {
        assert_eq!(detect(0), None);
        assert_eq!(detect(0x8168_0000), None);
        assert_eq!(detect(u32::MAX), None);
    }
}
