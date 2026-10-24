//! Register offsets and fixed values used by the RTL8125B driver.
//!
//! All offsets are byte offsets into the memory BAR. The RTL8125 exposes
//! 8-, 16- and 32-bit registers, so the driver must choose the access width
//! that matches the register rather than always using 32-bit accesses.

/// Station address, low and high words.
pub const MAC_ADDR: u16 = 0x0000;
/// Multicast hash registers, lower and upper halves.
pub const MAR0: u16 = 0x0008;
/// Multicast hash registers, lower and upper halves.
pub const MAR4: u16 = 0x000C;
/// Command register (8-bit).
pub const COMMAND: u16 = 0x0037;
/// Configuration write unlock register (8-bit, "CFG9346").
pub const CFG9346: u16 = 0x0050;
/// Config register 3 (8-bit): wake-on-LAN control.
pub const CFG3: u16 = 0x0054;
/// Transmit configuration (32-bit). The upper bits hold the MAC version code
/// until the driver overwrites the register with [`TXCFG_CONFIG`].
pub const TXCFG: u16 = 0x0040;
/// Receive configuration (32-bit).
pub const RXCFG: u16 = 0x0044;
/// Config register 1 (8-bit).
pub const CFG1: u16 = 0x0052;
/// Config register 2 (8-bit).
pub const CFG2: u16 = 0x0053;
/// Config register 5 (8-bit).
pub const CFG5: u16 = 0x0056;
/// RX maximum size (16-bit).
pub const RX_MAX_SIZE: u16 = 0x00DA;
/// PHY status (32-bit on the RTL8125).
pub const PHY_STATUS: u16 = 0x006C;
/// Power-management control (8-bit).
pub const PMCH: u16 = 0x006F;
/// Interrupt configuration 0 (8-bit): selects the new interrupt map.
pub const INT_CFG0: u16 = 0x0034;
/// Interrupt configuration 1 (16-bit).
pub const INT_CFG1: u16 = 0x007A;
/// Transmit descriptor fetch number (8-bit).
pub const TDFNR: u16 = 0x0057;
/// Transmit doorbell (16-bit on the RTL8125).
pub const TPPOLL: u16 = 0x0090;
/// Normal-priority queue doorbell bit.
pub const TPPOLL_NPQ: u16 = 0x0001;
/// EPHY access register (32-bit).
pub const EPHY_ACCESS: u16 = 0x0080;
/// MAC OCP data/access register.
pub const MAC_OCP: u16 = 0x00B0;
/// Direct PHY OCP access register.
pub const PHY_OCP: u16 = 0x00B8;
/// C+ command register (16-bit).
pub const CPLUS_COMMAND: u16 = 0x00E0;
/// Interrupt mitigation register (16-bit).
pub const INTR_MITIGATE: u16 = 0x00E2;
/// Transmit descriptor ring base, low and high words.
pub const TNPDS: u16 = 0x0020;
/// Receive descriptor ring base, low and high words.
pub const RDSAR: u16 = 0x00E4;
/// Power-management control (8-bit, alias used by the RTL8125 family).
pub const POWER_CONTROL_D0: u16 = 0x00D0;
/// Clock/power control register written during RTL8125 initialisation.
pub const CLOCK_CONTROL_F2: u16 = 0x00F2;
/// Clock-request control register.
pub const CLOCK_REQUEST_F1: u16 = 0x00F1;
/// Backup station address used by the RTL8125 family.
pub const BACKUP_MAC_ADDR: u16 = 0x19E0;
/// MCU command/status register (8-bit).
pub const MCU_COMMAND: u16 = 0x00D3;
/// Interrupt mask clear register for the new mapping.
pub const IMR_V2_CLEAR: u16 = 0x0D00;
/// Interrupt status register for the new mapping (32-bit).
pub const ISR_V2: u16 = 0x0D04;
/// Interrupt mask set register for the new mapping.
pub const IMR_V2_SET: u16 = 0x0D0C;
/// Interrupt mitigation control for the RTL8125B (32-bit).
pub const INTR_MITIGATE_8125B: u16 = 0x0A00;

/// Reset the MAC and DMA state.
pub const COMMAND_RESET: u8 = 1 << 4;
/// Enable the transmit path.
pub const COMMAND_TX_ENABLE: u8 = 1 << 2;
/// Enable the receive path.
pub const COMMAND_RX_ENABLE: u8 = 1 << 3;
/// Ask the MAC MCU to stop its packet filter.
pub const COMMAND_STOP_REQUEST: u8 = 1 << 7;

/// CFG9346 unlock value for configuration-register writes.
pub const CFG9346_UNLOCK: u8 = 0x80 | 0x40;

/// Initial transmit configuration for the RTL8125.
pub const TXCFG_CONFIG: u32 = 0x0300_0780;
/// Initial receive configuration for the RTL8125B.
pub const RXCFG_INITIAL: u32 = 0x40C0_0C00;
/// Accept unicast, multicast and broadcast frames.
pub const RXCFG_FILTER: u32 = 0x0000_000E;
/// C+ command value used by the RTL8125 initialisation.
pub const CPLUS_CONFIG: u16 = 0x2060;

/// RX maximum size for a 1500-byte MTU: 1500 + 14 header + 4 VLAN + 4 CRC.
pub const RX_MAX_SIZE_1500: u16 = 1522;

/// VLAN tag-insertion/removal bits in [`RXCFG`], disabled when VLAN offload is not used.
pub const RXCFG_VLAN_TAG_MASK: u32 = (1 << 22) | (1 << 23);

/// RTL8125B configuration bit set in [`CFG2`] during MAC initialisation.
pub const CFG2_RTL8125_BIT5: u8 = 1 << 5;
/// ASPM clock-request lock bit in [`CFG2`].
pub const CFG2_ASPM_LOCK: u8 = 1 << 7;
/// ASPM clock-request lock bit in [`CFG5`].
pub const CFG5_ASPM_LOCK: u8 = 1 << 0;
/// Force-clock-request bit in [`CLOCK_REQUEST_F1`].
pub const CLOCK_REQUEST_FORCE: u8 = 1 << 7;
/// Wake-on-LAN link bit in [`CFG3`].
pub const CFG3_WOL_LINK: u8 = 1 << 4;
/// Wake-on-LAN bits in [`CFG5`].
pub const CFG5_WOL_MASK: u8 = 0x40 | 0x20 | 0x10 | 0x02;

/// FIFO-empty bits in [`MCU_COMMAND`].
pub const MCU_COMMAND_FIFO_EMPTY: u8 = (1 << 5) | (1 << 4);
/// Out-of-box bit in [`MCU_COMMAND`], cleared once the driver owns the MAC.
pub const MCU_COMMAND_NOW_IS_OOB: u8 = 1 << 7;
/// FIFO-idle bits in [`INTR_MITIGATE`] after the MAC MCU has stopped.
pub const INTR_MITIGATE_IDLE: u16 = (1 << 0) | (1 << 1) | (1 << 8);

/// Interrupt mitigation value written to the RTL8125B.
pub const INTR_MITIGATE_VALUE: u32 = 0x0063_0063;

/// New interrupt-map enable bit in [`INT_CFG0`].
pub const INT_CFG0_ENABLE: u8 = 1 << 0;
/// New interrupt-map timeout bypass bit.
pub const INT_CFG0_TIMEOUT0_BYPASS: u8 = 1 << 1;
/// New interrupt-map mitigation bypass bit.
pub const INT_CFG0_MITIGATION_BYPASS: u8 = 1 << 2;

/// RX queue 0 interrupt status/mask bit.
pub const ISR_RXQ0_OK: u32 = 1 << 0;
/// TX queue 0 interrupt status/mask bit.
pub const ISR_TXQ0_OK: u32 = 1 << 16;

/// Link-status bit in [`PHY_STATUS`].
pub const PHY_STATUS_LINK: u32 = 1 << 1;
/// Full-duplex bit in [`PHY_STATUS`].
pub const PHY_STATUS_FULL_DUP: u32 = 1 << 0;
/// 10 Mb/s link bit in [`PHY_STATUS`].
pub const PHY_STATUS_10M: u32 = 1 << 2;
/// 100 Mb/s link bit in [`PHY_STATUS`].
pub const PHY_STATUS_100M: u32 = 1 << 3;
/// 1000 Mb/s link bit in [`PHY_STATUS`].
pub const PHY_STATUS_1000M: u32 = 1 << 4;
/// 2500 Mb/s link bit in [`PHY_STATUS`].
pub const PHY_STATUS_2500M: u32 = 1 << 10;
/// 5000 Mb/s link bit in [`PHY_STATUS`].
pub const PHY_STATUS_5000M: u32 = 1 << 12;
/// 10000 Mb/s link bit in [`PHY_STATUS`].
pub const PHY_STATUS_10000M: u32 = 1 << 14;

/// MAC MCU RAM window base used by the patch writer.
pub const MCU_RAM_BASE: u16 = 0xF800;
/// MAC MCU page-select register.
pub const MCU_PAGE_SELECT: u16 = 0xE446;
/// MAC MCU patch-version register on page 2.
pub const MCU_VERSION_PAGE2: u16 = 0xF9F8;
/// MAC MCU breakpoint-enable register.
pub const MCU_BREAKPOINT: u16 = 0xFC48;
/// MAC MCU patch-enable register.
pub const MCU_PATCH_ENABLE: u16 = 0xFC26;
/// Words per MAC MCU patch-RAM page.
pub const MCU_PAGE_WORDS: usize = 256;

/// PHY state register read to wait for the PHY power-up state.
pub const PHY_STATE: u16 = 0xA420;
/// PHY state value after power-up.
pub const PHY_STATE_READY: u16 = 3;
/// PHY MCU patch-request register.
pub const PHY_MCU_REQUEST: u16 = 0xB820;
/// PHY MCU status register.
pub const PHY_MCU_STATUS: u16 = 0xB800;
/// Request or busy bit in [`PHY_MCU_REQUEST`]/[`PHY_MCU_STATUS`].
pub const PHY_MCU_BIT: u16 = 1 << 4;
/// Completion bit polled in [`PHY_MCU_STATUS`].
pub const PHY_MCU_READY: u16 = 1 << 6;

/// Standard MII BMCR register number.
pub const MII_BMCR: u8 = 0x00;
/// Standard MII BMSR register number.
pub const MII_BMSR: u8 = 0x01;
/// BMCR auto-negotiation enable.
pub const BMCR_AUTOEN: u16 = 0x1000;
/// MII page-select register number (MDIO register 31).
pub const MII_PAGE_SELECT: u8 = 0x1F;

/// Negotiated link speed in Mb/s for a [`PHY_STATUS`] value.
///
/// The RTL8125 reports the negotiated speed as one of the mutually exclusive
/// speed bits. A value of zero means the speed is not known, which is the
/// case while the link is down.
#[must_use]
pub const fn link_speed_mbps(status: u32) -> u32 {
    if status & PHY_STATUS_10000M != 0 {
        10_000
    } else if status & PHY_STATUS_5000M != 0 {
        5_000
    } else if status & PHY_STATUS_2500M != 0 {
        2_500
    } else if status & PHY_STATUS_1000M != 0 {
        1_000
    } else if status & PHY_STATUS_100M != 0 {
        100
    } else if status & PHY_STATUS_10M != 0 {
        10
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_speed_follows_the_status_bits() {
        assert_eq!(link_speed_mbps(PHY_STATUS_2500M), 2_500);
        assert_eq!(link_speed_mbps(PHY_STATUS_1000M), 1_000);
        assert_eq!(link_speed_mbps(PHY_STATUS_100M), 100);
        assert_eq!(link_speed_mbps(PHY_STATUS_10M), 10);
        assert_eq!(link_speed_mbps(0), 0);
    }

    #[test]
    fn a_faster_speed_bit_wins() {
        assert_eq!(link_speed_mbps(PHY_STATUS_2500M | PHY_STATUS_1000M), 2_500);
    }
}
