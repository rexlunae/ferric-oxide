//! Ferric Oxide: decoding the local APIC ID register (issue #214).
//!
//! The x2APIC ID MSR holds the 32-bit APIC ID as it is. The xAPIC ID register (MMIO offset
//! 0x20) keeps the 8-bit ID in bits 31:24 (Intel SDM Vol. 3A, "Local APIC ID"). The MADT, the
//! ICR destination field and interrupt messages name the ID itself, so a raw xAPIC read has to
//! be shifted down first. This file has no dependencies so the host tests
//! (`crates/kernel/tests`) can include it by path.

/// The APIC ID held in `raw`, a value read from the local APIC ID register in x2APIC mode
/// (`x2`) or xAPIC mode.
pub const fn decode(raw: u32, x2: bool) -> u32 {
    if x2 {
        raw
    } else {
        raw >> 24
    }
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn xapic_id_is_in_the_top_byte() {
        assert_eq!(decode(0x0000_0000, false), 0);
        assert_eq!(decode(0x0200_0000, false), 2);
        assert_eq!(decode(0xff00_0000, false), 0xff);
        // Bits 23:0 are reserved and do not leak into the ID.
        assert_eq!(decode(0x03ab_cdef, false), 3);
    }

    #[test]
    fn x2apic_id_is_the_whole_register() {
        assert_eq!(decode(0, true), 0);
        assert_eq!(decode(2, true), 2);
        assert_eq!(decode(0x0200_0000, true), 0x0200_0000);
        assert_eq!(decode(u32::MAX, true), u32::MAX);
    }
}
