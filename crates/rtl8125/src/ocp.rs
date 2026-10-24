//! OCP access-command encoding for the RTL8125.
//!
//! The RTL8125 has two access windows:
//!
//! - MAC OCP through register `0x00B0`. A write command carries the register
//!   address in the upper half, the data in the lower half and bit 31 set. A
//!   read command carries only the address; the data appears in the same
//!   register.
//! - Direct PHY OCP through register `0x00B8`. The address is divided by two
//!   and shifted into the upper half. A write sets bit 31 and completes when
//!   the controller clears it; a read leaves bit 31 clear and completes when
//!   the controller sets it.
//!
//! Normal MII accesses are translated into direct PHY OCP addresses with
//! [`mapped_phy_address`]. Pages other than zero use the page number as the OCP
//! page and the MII register as the OCP register; page zero packs the standard
//! 32 MII registers into OCP addresses starting at `0xA400`.

/// Bit 31 of either OCP window: write request, or read-complete.
pub const FLAG: u32 = 1 << 31;

/// Encode a MAC OCP read command for `address`.
#[must_use]
pub const fn mac_read_command(address: u16) -> u32 {
    ((address as u32 / 2) << 16) & !FLAG
}

/// Encode a MAC OCP write command for `address` and `data`.
#[must_use]
pub const fn mac_write_command(address: u16, data: u16) -> u32 {
    ((address as u32 / 2) << 16) | data as u32 | FLAG
}

/// Encode a direct PHY OCP read command for `address`.
#[must_use]
pub const fn phy_read_command(address: u16) -> u32 {
    ((address as u32 / 2) << 16) & !FLAG
}

/// Encode a direct PHY OCP write command for `address` and `data`.
#[must_use]
pub const fn phy_write_command(address: u16, data: u16) -> u32 {
    ((address as u32 / 2) << 16) | data as u32 | FLAG
}

/// Translate an MII page and register number into a direct PHY OCP address.
///
/// This mirrors `MappingPhyOcpAddress` in FreeBSD `if_re`. Page zero maps the
/// standard registers into the `0xA400` window; other pages use the page
/// number shifted left by four, with registers below 16 encoded specially.
#[must_use]
pub const fn mapped_phy_address(page: u16, register: u8) -> u16 {
    let (ocp_page, ocp_register) = if page == 0 {
        (0x0A40 + (register / 8) as u16, 0x10 + (register % 8))
    } else {
        (page, register)
    };
    let ocp_page = ocp_page << 4;
    if ocp_register < 16 {
        0
    } else {
        ocp_page + (((ocp_register - 16) as u16) << 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_commands_split_address_and_data() {
        assert_eq!(mac_read_command(0xFC48), 0x7E24_0000);
        assert_eq!(mac_write_command(0xFC48, 0x1C3F), 0xFE24_1C3F);
    }

    #[test]
    fn phy_commands_split_address_and_data() {
        assert_eq!(phy_read_command(0xA420), 0x5210_0000);
        assert_eq!(phy_write_command(0xB820, 0x0010), 0xDC10_0010);
    }

    #[test]
    fn page_zero_maps_standard_mii_registers() {
        assert_eq!(mapped_phy_address(0, 0), 0xA400);
        assert_eq!(mapped_phy_address(0, 1), 0xA402);
        assert_eq!(mapped_phy_address(0, 7), 0xA40E);
        assert_eq!(mapped_phy_address(0, 8), 0xA410);
        assert_eq!(mapped_phy_address(0, 0x1F), 0xA43E);
    }

    #[test]
    fn explicit_pages_use_the_page_number() {
        assert_eq!(mapped_phy_address(0x0A43, 6), 0);
        assert_eq!(mapped_phy_address(0x0A43, 16), 0xA430);
        assert_eq!(mapped_phy_address(0x0A43, 17), 0xA432);
    }
}
