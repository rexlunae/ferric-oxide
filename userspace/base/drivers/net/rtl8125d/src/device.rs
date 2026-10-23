//! RTL8125B controller initialisation and packet rings.
//!
//! The RTL8125 is not a register-compatible extension of the RTL8168. Its MAC
//! version code sits in the upper bits of the transmit configuration register,
//! its interrupt status is a 32-bit register behind a new interrupt map, its
//! transmit doorbell differs, and its MAC MCU has patch RAM that must be
//! loaded before the controller behaves reliably. The sequence here follows the
//! permissively licensed FreeBSD `if_re` driver for the RTL8125B (MACFG 83),
//! using the legacy 16-byte descriptor format that `if_re` selects by clearing
//! bit 0 of MAC OCP register `0xEB58`.
//!
//! Only the RTL8125B revision is claimed. The MAC and PHY patch tables and the
//! EPHY register sequence are hardware data and live in
//! `ferric-rtl8125::firmware`; the register and descriptor encodings live
//! beside them in the same crate, where they are covered by host tests. This
//! module supplies the MMIO/DMA mechanics and the Redox network adapter.
//!
//! The register-level work lives in [`Controller`], which talks to the
//! controller through the [`Regs`] trait, so the whole initialisation sequence
//! runs against a model of the controller in the host tests below.
//!
//! The driver supports one RX queue and one TX queue, 1500-byte frames, and
//! the controller's legacy interrupt pin. It does not implement checksum or
//! segmentation offload, jumbo frames, VLAN stripping, multiple queues,
//! per-queue MSI-X vectors, or power-management wakeup. The receive path
//! removes the Ethernet CRC, which the descriptor length includes, and the
//! network stack filters multicast groups in software.
//!
//! Hardware validation is still pending: QEMU does not emulate the RTL8125, so
//! this path has been compiled, reviewed against the reference sequence and
//! exercised against a register model, but not run on the controller. The boot
//! log must confirm that the MAC and PHY patch versions are accepted before the
//! driver can be considered functional.

use std::mem::size_of;
use std::time::Duration;

use common::dma::Dma;
use common::io::{Io, Mmio};
use common::timeout::Timeout;
use driver_network::NetworkAdapter;
use ferric_rtl8125::{desc, firmware, ocp, regs, version};
use syscall::error::{EAGAIN, EIO, EMSGSIZE, ENODEV, Error, Result};

const RX_RING_LEN: usize = 64;
const TX_RING_LEN: usize = 16;
/// Receive buffer size: an MTU-sized frame plus header, VLAN tag and CRC.
const RX_BUFFER_SIZE: usize = 1522;
/// Transmit buffer size: an MTU-sized frame plus header and CRC.
const TX_BUFFER_SIZE: usize = 2048;

/// The legacy receive descriptor's length includes the Ethernet CRC.
const ETHERNET_CRC_LEN: usize = 4;

/// Length of an Ethernet frame ready for the network scheme.
fn received_length(ctrl: u32) -> usize {
    desc::rx_length(ctrl).saturating_sub(ETHERNET_CRC_LEN)
}

/// One 16-byte descriptor in the RTL8125's legacy format.
#[repr(C, packed)]
struct Descriptor {
    ctrl: Mmio<u32>,
    vlan: Mmio<u32>,
    buffer_low: Mmio<u32>,
    buffer_high: Mmio<u32>,
}

const _: [(); desc::DESCRIPTOR_SIZE] = [(); size_of::<Descriptor>()];

/// Access to the controller's register file.
///
/// The production implementation reads and writes the memory-mapped BAR. The
/// host tests implement the trait with a model of the controller, so the
/// initialisation sequence can be checked without hardware.
trait Regs {
    /// Read an 8-bit register.
    fn read8(&self, offset: u16) -> u8;
    /// Write an 8-bit register.
    fn write8(&self, offset: u16, value: u8);
    /// Read a 16-bit register.
    fn read16(&self, offset: u16) -> u16;
    /// Write a 16-bit register.
    fn write16(&self, offset: u16, value: u16);
    /// Read a 32-bit register.
    fn read32(&self, offset: u16) -> u32;
    /// Write a 32-bit register.
    fn write32(&self, offset: u16, value: u32);

    /// Clear some bits of an 8-bit register and set others.
    fn update8(&self, offset: u16, clear: u8, set: u8) {
        self.write8(offset, (self.read8(offset) & !clear) | set);
    }

    /// Clear some bits of a 16-bit register and set others.
    fn update16(&self, offset: u16, clear: u16, set: u16) {
        self.write16(offset, (self.read16(offset) & !clear) | set);
    }

    /// Clear some bits of a 32-bit register and set others.
    fn update32(&self, offset: u16, clear: u32, set: u32) {
        self.write32(offset, (self.read32(offset) & !clear) | set);
    }
}

/// MMIO access to the RTL8125 register file.
struct MmioRegisters {
    base: *mut u8,
}

impl MmioRegisters {
    /// Create an accessor for a mapped register BAR.
    ///
    /// # Safety
    ///
    /// `base` must point to an uncacheable mapping of at least 64 KiB that
    /// remains mapped for the lifetime of the accessor.
    unsafe fn new(base: usize) -> Self {
        Self {
            base: base as *mut u8,
        }
    }
}

impl Regs for MmioRegisters {
    #[inline]
    fn read8(&self, offset: u16) -> u8 {
        // SAFETY: the constructor guarantees a complete register mapping, and
        // every caller passes an offset inside it.
        unsafe { (*(self.base.add(offset as usize).cast::<Mmio<u8>>())).read() }
    }

    #[inline]
    fn write8(&self, offset: u16, value: u8) {
        // SAFETY: see `read8`.
        unsafe { (*(self.base.add(offset as usize).cast::<Mmio<u8>>())).write(value) }
    }

    #[inline]
    fn read16(&self, offset: u16) -> u16 {
        // SAFETY: see `read8`; 16-bit registers are aligned.
        unsafe { (*(self.base.add(offset as usize).cast::<Mmio<u16>>())).read() }
    }

    #[inline]
    fn write16(&self, offset: u16, value: u16) {
        // SAFETY: see `read8`; 16-bit registers are aligned.
        unsafe { (*(self.base.add(offset as usize).cast::<Mmio<u16>>())).write(value) }
    }

    #[inline]
    fn read32(&self, offset: u16) -> u32 {
        // SAFETY: see `read8`; 32-bit registers are aligned.
        unsafe { (*(self.base.add(offset as usize).cast::<Mmio<u32>>())).read() }
    }

    #[inline]
    fn write32(&self, offset: u16, value: u32) {
        // SAFETY: see `read8`; 32-bit registers are aligned.
        unsafe {
            (*(self.base.add(offset as usize).cast::<Mmio<u32>>())).write(value);
        }
    }
}

/// The controller's register-level state.
///
/// Everything the driver does that does not need DMA memory lives here: the
/// out-of-box exit, the MAC and PHY patch sequences, the common register
/// setup, and the interrupt status handling.
struct Controller<R: Regs> {
    regs: R,
    mac_address: [u8; 6],
    phy_page: u16,
    link_up: bool,
}

impl<R: Regs> Controller<R> {
    fn new(regs: R) -> Self {
        Self {
            regs,
            mac_address: [0; 6],
            phy_page: 0,
            link_up: false,
        }
    }

    /// Read and acknowledge the interrupt status.
    ///
    /// Returns true when the status contains a receive or transmit event that
    /// the network scheme should service.
    pub fn irq(&mut self) -> bool {
        let status = self.regs.read32(regs::ISR_V2);
        if status == 0 || status == u32::MAX {
            return false;
        }
        self.regs.write32(regs::ISR_V2, status);

        let phy_status = self.regs.read32(regs::PHY_STATUS);
        let link_up = phy_status & regs::PHY_STATUS_LINK != 0;
        if link_up != self.link_up {
            if link_up {
                self.link_up_patch(phy_status);
                log::info!(
                    "RTL8125B link is up at {} Mb/s",
                    regs::link_speed_mbps(phy_status)
                );
            } else {
                log::info!("RTL8125B link is down");
            }
            self.link_up = link_up;
        }

        status & (regs::ISR_RXQ0_OK | regs::ISR_TXQ0_OK) != 0
    }

    /// Apply the transmit settings the reference driver uses once a link is up.
    ///
    /// The inter-frame gap depends on the negotiated duplex mode.
    fn link_up_patch(&self, phy_status: u32) {
        if phy_status & regs::PHY_STATUS_FULL_DUP != 0 {
            self.regs
                .update32(regs::TXCFG, 1 << 19, (1 << 24) | (1 << 25));
        } else {
            self.regs
                .update32(regs::TXCFG, (1 << 19) | (1 << 24), 1 << 25);
        }
    }

    /// Initialise the controller.
    ///
    /// The descriptor rings are programmed separately, because they need DMA
    /// memory that this layer does not own.
    fn init(&mut self) -> Result<()> {
        self.mac_address = self.read_mac_address()?;
        log::info!(
            "MAC {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.mac_address[0],
            self.mac_address[1],
            self.mac_address[2],
            self.mac_address[3],
            self.mac_address[4],
            self.mac_address[5],
        );

        self.exit_oob()?;
        self.stop_txrx()?;

        // EEE+ is not negotiated by this driver; clear the firmware's enable
        // bit before patching the MAC.
        self.mac_update(0xE080, 1 << 1, 0);

        self.cfg_unlock();
        // Do not leave wake-on-LAN armed while the driver owns the device.
        self.mac_update(0xC0B6, 1 << 0, 0);
        self.aspm_clkreq_lock(false);
        self.force_clkreq(false);
        self.regs.update8(regs::CFG2, 0, regs::CFG2_RTL8125_BIT5);
        self.cfg_lock();

        self.mac_mcu_config();
        self.phy_power_up()?;
        self.phy_config()?;
        self.ephy_config()?;
        self.hw_common();

        self.set_mac_address();

        Ok(())
    }

    fn read_mac_address(&self) -> Result<[u8; 6]> {
        let mut mac = self.read_mac_from(regs::BACKUP_MAC_ADDR);
        if !is_valid_mac(&mac) {
            log::warn!(
                "RTL8125B backup MAC address is invalid; trying the station-address register"
            );
            mac = self.read_mac_from(regs::MAC_ADDR);
        }
        if !is_valid_mac(&mac) {
            log::error!("RTL8125B has no valid unicast MAC address");
            return Err(Error::new(ENODEV));
        }
        Ok(mac)
    }

    fn read_mac_from(&self, offset: u16) -> [u8; 6] {
        let low = self.regs.read32(offset);
        let high = self.regs.read16(offset + 4);
        [
            low as u8,
            (low >> 8) as u8,
            (low >> 16) as u8,
            (low >> 24) as u8,
            high as u8,
            (high >> 8) as u8,
        ]
    }

    fn set_mac_address(&self) {
        let low = u32::from_le_bytes([
            self.mac_address[0],
            self.mac_address[1],
            self.mac_address[2],
            self.mac_address[3],
        ]);
        let high = u16::from_le_bytes([self.mac_address[4], self.mac_address[5]]);

        self.cfg_unlock();
        self.regs.write32(regs::MAC_ADDR, low);
        self.regs.write16(regs::MAC_ADDR + 4, high);
        self.cfg_lock();
    }

    fn cfg_unlock(&self) {
        self.regs.write8(regs::CFG9346, regs::CFG9346_UNLOCK);
    }

    fn cfg_lock(&self) {
        self.regs.write8(regs::CFG9346, 0);
    }

    fn sleep(millis: u64) {
        std::thread::sleep(Duration::from_millis(millis));
    }

    fn wait_bits8(&self, offset: u16, mask: u8, timeout_ms: u64) -> bool {
        let timeout = Timeout::from_millis(timeout_ms);
        loop {
            if self.regs.read8(offset) & mask == mask {
                return true;
            }
            if timeout.run().is_err() {
                return false;
            }
        }
    }

    fn wait_bits16(&self, offset: u16, mask: u16, timeout_ms: u64) -> bool {
        let timeout = Timeout::from_millis(timeout_ms);
        loop {
            if self.regs.read16(offset) & mask == mask {
                return true;
            }
            if timeout.run().is_err() {
                return false;
            }
        }
    }

    /// Take the MAC out of the firmware's out-of-box state.
    ///
    /// The RTL8125B needs its packet filter stopped and its FIFOs drained
    /// before the MCU patch RAM can be rewritten. Timeouts are logged but not
    /// fatal, matching the reference: a controller that is already idle can
    /// legitimately skip these handshakes, and the following reset still puts
    /// the MAC in a known state.
    fn exit_oob(&self) -> Result<()> {
        self.cfg_lock();
        self.regs.update8(regs::CLOCK_CONTROL_F2, 0, 1 << 3);
        Self::sleep(2);

        let command = self.regs.read8(regs::COMMAND);
        if command & (regs::COMMAND_TX_ENABLE | regs::COMMAND_RX_ENABLE) != 0 {
            std::thread::sleep(Duration::from_micros(100));
            self.regs.write8(
                regs::COMMAND,
                command & !(regs::COMMAND_TX_ENABLE | regs::COMMAND_RX_ENABLE),
            );
        }
        self.regs
            .update8(regs::COMMAND, 0, regs::COMMAND_STOP_REQUEST);

        if !self.wait_bits8(regs::MCU_COMMAND, regs::MCU_COMMAND_FIFO_EMPTY, 150) {
            log::warn!("RTL8125B MAC FIFOs did not report empty");
        }
        if !self.wait_bits16(regs::INTR_MITIGATE, regs::INTR_MITIGATE_IDLE, 150) {
            log::warn!("RTL8125B interrupt mitigation did not report idle");
        }

        self.mac_update(0xC0B4, 1 << 0, 0);
        self.mac_update(0xC0B4, 0, 1 << 0);
        self.regs
            .update8(regs::MCU_COMMAND, regs::MCU_COMMAND_NOW_IS_OOB, 0);

        self.mac_update(0xE8DE, 1 << 14, 0);
        self.wait_bits16(0xD2, 1 << 9, 10);
        self.mac_ocp_write(0xC0AA, 0x07D0);
        self.mac_ocp_write(0xC0A6, 0x01B5);
        self.mac_ocp_write(0xC01E, 0x5555);
        self.wait_bits16(0xD2, 1 << 9, 10);

        if self.mac_ocp_read(0xD42C) & (1 << 8) != 0 {
            let mut resumed = false;
            for _ in 0..100 {
                if self.real_ocp_phy_read(regs::PHY_STATE)? & 0x7 == 2 {
                    resumed = true;
                    break;
                }
                Self::sleep(1);
            }
            if !resumed {
                log::warn!("RTL8125B PHY did not leave UPS resume state");
            }
            self.mac_update(0xD42C, 1 << 8, 0);
            self.phy_update(0xA466, 1 << 0, 0)?;
            self.phy_update(0xA468, (1 << 3) | (1 << 1), 0)?;
        }

        Ok(())
    }

    fn stop_txrx(&self) -> Result<()> {
        self.regs.update32(regs::RXCFG, 0x3F, 0);
        self.regs.write32(regs::IMR_V2_CLEAR, u32::MAX);
        self.regs.write32(regs::ISR_V2, u32::MAX);
        self.reset()
    }

    fn reset(&self) -> Result<()> {
        Self::sleep(2);
        std::thread::sleep(Duration::from_micros(200));
        self.regs.write8(regs::COMMAND, regs::COMMAND_RESET);

        let timeout = Timeout::from_millis(100);
        while self.regs.read8(regs::COMMAND) & regs::COMMAND_RESET != 0 {
            timeout.run().map_err(|()| Error::new(EIO))?;
        }
        Ok(())
    }

    fn mac_ocp_read(&self, address: u16) -> u16 {
        self.regs
            .write32(regs::MAC_OCP, ocp::mac_read_command(address));
        self.regs.read32(regs::MAC_OCP) as u16
    }

    fn mac_ocp_write(&self, address: u16, value: u16) {
        self.regs
            .write32(regs::MAC_OCP, ocp::mac_write_command(address, value));
    }

    fn mac_update(&self, address: u16, clear: u16, set: u16) {
        self.mac_ocp_write(address, (self.mac_ocp_read(address) & !clear) | set);
    }

    fn real_ocp_phy_read(&self, address: u16) -> Result<u16> {
        self.regs
            .write32(regs::PHY_OCP, ocp::phy_read_command(address));

        let timeout = Timeout::from_millis(100);
        loop {
            let value = self.regs.read32(regs::PHY_OCP);
            if value & ocp::FLAG != 0 {
                return Ok(value as u16);
            }
            timeout.run().map_err(|()| Error::new(EIO))?;
        }
    }

    fn real_ocp_phy_write(&self, address: u16, value: u16) -> Result<()> {
        self.regs
            .write32(regs::PHY_OCP, ocp::phy_write_command(address, value));

        let timeout = Timeout::from_millis(100);
        loop {
            if self.regs.read32(regs::PHY_OCP) & ocp::FLAG == 0 {
                return Ok(());
            }
            timeout.run().map_err(|()| Error::new(EIO))?;
        }
    }

    fn phy_update(&self, address: u16, clear: u16, set: u16) -> Result<()> {
        let value = self.real_ocp_phy_read(address)?;
        self.real_ocp_phy_write(address, (value & !clear) | set)
    }

    fn mdio_read(&self, register: u8) -> Result<u16> {
        let address = ocp::mapped_phy_address(self.phy_page, register);
        if !address.is_multiple_of(2) {
            let low = (self.real_ocp_phy_read(address)? & 0xFF00) >> 8;
            let high = (self.real_ocp_phy_read(address + 1)? & 0x00FF) << 8;
            Ok(low | high)
        } else {
            self.real_ocp_phy_read(address)
        }
    }

    fn mdio_write(&mut self, register: u8, value: u16) -> Result<()> {
        if register == regs::MII_PAGE_SELECT {
            self.phy_page = value;
            return Ok(());
        }

        let address = ocp::mapped_phy_address(self.phy_page, register);
        if !address.is_multiple_of(2) {
            let mut current = self.real_ocp_phy_read(address)?;
            current = (current & 0x00FF) | (value << 8);
            self.real_ocp_phy_write(address, current)?;

            current = self.real_ocp_phy_read(address + 1)?;
            current = (current & 0xFF00) | (value >> 8);
            self.real_ocp_phy_write(address + 1, current)
        } else {
            self.real_ocp_phy_write(address, value)
        }
    }

    /// Write an EPHY register through the `0x0080` access window.
    ///
    /// The RTL8125B's external PHY is also reachable through a legacy EPHY
    /// register window used by the 8125B PHY tuning sequence. A write sets bit
    /// 31 and completes when the controller clears it.
    fn ephy_write(&self, address: u16, value: u16) -> Result<()> {
        self.regs.write32(
            regs::EPHY_ACCESS,
            ocp::FLAG | (u32::from(address) << 16) | u32::from(value),
        );

        let timeout = Timeout::from_millis(10);
        loop {
            if self.regs.read32(regs::EPHY_ACCESS) & ocp::FLAG == 0 {
                return Ok(());
            }
            timeout.run().map_err(|()| Error::new(EIO))?;
        }
    }

    fn ephy_config(&self) -> Result<()> {
        for &(address, value) in firmware::EPHY_CONFIG_8125B {
            self.ephy_write(address, value)?;
        }
        Ok(())
    }

    fn disable_mcu_bps(&self) {
        self.cfg_unlock();
        self.aspm_clkreq_lock(false);
        self.cfg_lock();

        self.mac_ocp_write(regs::MCU_BREAKPOINT, 0);
        for address in (0xFC28..0xFC48).step_by(2) {
            self.mac_ocp_write(address, 0);
        }
        Self::sleep(3);
        self.mac_ocp_write(regs::MCU_PATCH_ENABLE, 0);
    }

    fn mac_mcu_config(&self) {
        self.disable_mcu_bps();

        for (index, word) in firmware::MAC_PATCH_8125B.iter().enumerate() {
            let offset = index % regs::MCU_PAGE_WORDS;
            if offset == 0 {
                self.switch_mac_mcu_page((index / regs::MCU_PAGE_WORDS) as u16);
            }
            self.mac_ocp_write(regs::MCU_RAM_BASE + (offset as u16) * 2, *word);
        }
        self.set_mac_patch_version(firmware::MAC_PATCH_8125B_VERSION);
        log::info!(
            "RTL8125B MAC patch version {:#018X} written",
            firmware::MAC_PATCH_8125B_VERSION
        );

        for &(address, value) in firmware::MAC_PATCH_8125B_CONFIG {
            self.mac_ocp_write(address, value);
        }
    }

    fn switch_mac_mcu_page(&self, page: u16) {
        self.mac_update(regs::MCU_PAGE_SELECT, 0x3, page & 0x3);
    }

    fn set_mac_patch_version(&self, mut version: u64) {
        self.switch_mac_mcu_page(2);
        for index in (0..8u16).step_by(2) {
            self.mac_ocp_write(regs::MCU_VERSION_PAGE2 + 6 - index, version as u16);
            version >>= 16;
        }
        self.switch_mac_mcu_page(0);
    }

    fn phy_power_up(&mut self) -> Result<()> {
        self.regs.update8(regs::PMCH, 0, 0xC0);

        self.mdio_write(regs::MII_PAGE_SELECT, 0)?;
        self.mdio_write(regs::MII_BMCR, regs::BMCR_AUTOEN)?;
        let bmsr = self.mdio_read(regs::MII_BMSR)?;
        log::debug!("RTL8125B PHY status {bmsr:#06X}");

        for _ in 0..100 {
            if self.real_ocp_phy_read(regs::PHY_STATE)? & 0x7 == regs::PHY_STATE_READY {
                return Ok(());
            }
            Self::sleep(1);
        }
        log::warn!("RTL8125B PHY did not reach power-up state 3");
        Ok(())
    }

    fn phy_patch_request(&self, enable: bool) -> Result<()> {
        if enable {
            self.phy_update(regs::PHY_MCU_REQUEST, 0, regs::PHY_MCU_BIT)?;
        } else {
            self.phy_update(regs::PHY_MCU_REQUEST, regs::PHY_MCU_BIT, 0)?;
        }

        let timeout = Timeout::from_millis(100);
        loop {
            let status = self.real_ocp_phy_read(regs::PHY_MCU_STATUS)?;
            let ready = status & regs::PHY_MCU_READY != 0;
            if ready == enable {
                return Ok(());
            }
            timeout.run().map_err(|()| Error::new(EIO))?;
        }
    }

    fn phy_config(&mut self) -> Result<()> {
        self.real_ocp_phy_write(0xA436, 0x801E)?;
        let hardware_version = self.real_ocp_phy_read(0xA438)?;
        if hardware_version != firmware::PHY_PATCH_8125B_VERSION {
            log::info!(
                "RTL8125B PHY patch {hardware_version:#06X} -> {:#06X}",
                firmware::PHY_PATCH_8125B_VERSION
            );
            self.phy_patch_request(true)?;
            for pair in firmware::PHY_PATCH_8125B.chunks_exact(2) {
                if pair == [0xFFFF, 0xFFFF] {
                    break;
                }
                self.real_ocp_phy_write(pair[0], pair[1])?;
            }
            self.phy_patch_request(false)?;

            self.real_ocp_phy_write(0xA436, 0x801E)?;
            self.real_ocp_phy_write(0xA438, firmware::PHY_PATCH_8125B_VERSION)?;
        } else {
            log::info!("RTL8125B PHY patch {hardware_version:#06X} already present");
        }

        self.mdio_write(regs::MII_PAGE_SELECT, 0)?;
        self.apply_phy_config_8125b_2()
    }

    fn apply_phy_config_8125b_2(&self) -> Result<()> {
        self.phy_update(0xA442, 0, 1 << 11)?;
        self.phy_update(0xAC46, 0x00F0, 0x0090)?;
        self.phy_update(0xAD30, 0x0003, 0x0001)?;

        self.real_ocp_phy_write(0xB87C, 0x80F5)?;
        self.real_ocp_phy_write(0xB87E, 0x760E)?;
        self.real_ocp_phy_write(0xB87C, 0x8107)?;
        self.real_ocp_phy_write(0xB87E, 0x360E)?;
        self.real_ocp_phy_write(0xB87C, 0x8551)?;
        self.phy_update(0xB87E, 0xFF00, 0x0800)?;

        self.phy_update(0xBF00, 0xE000, 0xA000)?;
        self.phy_update(0xBF46, 0x0F00, 0x0300)?;

        for address in (0x8044..=0x807A).step_by(6) {
            self.real_ocp_phy_write(0xA436, address)?;
            self.real_ocp_phy_write(0xA438, 0x2417)?;
        }

        self.phy_update(0xA4CA, 0, 1 << 6)?;
        self.phy_update(0xA4CA, (1 << 14) | (1 << 13), 0)?;

        self.phy_update(0xBF84, 0xE000, 0xA000)?;
        self.real_ocp_phy_write(0xA436, 0x8170)?;
        self.phy_update(
            0xA438,
            (1 << 13) | (1 << 10) | (1 << 9) | (1 << 8),
            (1 << 15) | (1 << 14) | (1 << 12) | (1 << 11),
        )?;

        self.phy_update(0xA424, 0, 1 << 3)?;
        // Match the FreeBSD default (`phy_power_saving = 1`).
        self.phy_update(0xA430, 0, 1 << 2)?;
        Ok(())
    }

    fn aspm_clkreq_lock(&self, enable: bool) {
        self.regs
            .update8(regs::CFG5, 0, if enable { regs::CFG5_ASPM_LOCK } else { 0 });
        self.regs
            .update8(regs::CFG2, 0, if enable { regs::CFG2_ASPM_LOCK } else { 0 });
    }

    fn force_clkreq(&self, enable: bool) {
        self.regs.update8(
            regs::CLOCK_REQUEST_F1,
            0,
            if enable { regs::CLOCK_REQUEST_FORCE } else { 0 },
        );
    }

    fn hw_common(&self) {
        self.cfg_unlock();
        self.aspm_clkreq_lock(false);
        self.force_clkreq(false);

        // FreeBSD enables L1 offload only when a PCIe capability bit allows
        // it. The driver cannot read that bit, and leaving L1 disabled is the
        // conservative choice for a first bring-up.
        self.mac_update(0xE094, 0xFF00, 0);
        self.mac_update(0xE092, 0x00FF, 0);

        self.regs.write16(regs::CPLUS_COMMAND, regs::CPLUS_CONFIG);
        self.regs.write32(regs::TXCFG, regs::TXCFG_CONFIG);
        self.regs.update8(regs::CFG2, 0, regs::CFG2_RTL8125_BIT5);

        self.mac_ocp_write(0xE098, 0xC302);
        self.regs.write16(0x382, 0x221B);
        self.regs.write8(0x4500, 0x00);
        self.regs.write16(0x4800, 0x0000);

        self.regs.update8(regs::CFG1, 0x10, 0);
        self.regs.update8(regs::CLOCK_CONTROL_F2, 1 << 3, 0);
        self.regs.write8(regs::TDFNR, 0x10);

        self.mac_ocp_write(0xC140, 0xFFFF);
        self.mac_ocp_write(0xC142, 0xFFFF);

        // Select the legacy 16-byte transmit descriptor format.
        self.mac_update(0xEB58, 1 << 0, 0);

        self.mac_update(0xE614, (1 << 10) | (1 << 9) | (1 << 8), 2 << 8);

        self.mac_update(0xE63E, (1 << 11) | (1 << 10), 0);
        self.mac_update(0xE63E, (1 << 5) | (1 << 4), 2 << 4);

        self.mac_update(0xC0B4, 0, (1 << 3) | (1 << 2));
        self.mac_update(0xEB6A, 0xFF, (1 << 5) | (1 << 4) | (1 << 1) | (1 << 0));
        self.mac_update(
            0xEB50,
            (1 << 9) | (1 << 8) | (1 << 7) | (1 << 6) | (1 << 5),
            1 << 6,
        );
        self.mac_update(0xE056, 0xF0, 0);
        self.mac_update(0xE040, 1 << 12, 0);
        self.mac_update(0xEA1C, 0x3, 1);
        self.mac_ocp_write(0xE0C0, 0x4400);
        self.mac_update(0xE052, (1 << 3) | (1 << 7), (1 << 6) | (1 << 5));
        self.mac_update(0xC0AC, 0, 0x1F80);
        self.mac_update(0xD430, 0xFFF, 0x45F);

        self.regs
            .update8(regs::POWER_CONTROL_D0, 0, (1 << 6) | (1 << 7));
        self.regs.update8(regs::CLOCK_CONTROL_F2, 0, 1 << 6);

        self.mac_update(0xEA1C, 1 << 2, 0);
        self.mac_update(0xEB54, 0, 1 << 0);
        Self::sleep(1);
        self.mac_update(0xEB54, 1 << 0, 0);
        self.regs.update16(0x1880, (1 << 4) | (1 << 5), 0);

        for address in (0xA00..0xA80).step_by(4) {
            self.regs.write32(address, 0);
        }

        self.regs.update8(
            regs::INT_CFG0,
            regs::INT_CFG0_ENABLE
                | regs::INT_CFG0_TIMEOUT0_BYPASS
                | regs::INT_CFG0_MITIGATION_BYPASS,
            regs::INT_CFG0_ENABLE,
        );
        self.regs.write16(regs::INT_CFG1, 0);

        self.aspm_clkreq_lock(true);
        self.force_clkreq(false);

        self.regs.update8(regs::CFG3, regs::CFG3_WOL_LINK, 0);
        self.mac_update(0xC0B6, 1 << 0, 0);
        self.regs.update8(regs::CFG5, regs::CFG5_WOL_MASK, 0);

        self.regs
            .write32(regs::INTR_MITIGATE_8125B, regs::INTR_MITIGATE_VALUE);
        self.cfg_lock();
    }

    /// Enable the transmitter and the receiver.
    fn start(&mut self) {
        self.regs.write32(regs::RXCFG, regs::RXCFG_INITIAL);
        self.regs
            .update32(regs::RXCFG, regs::RXCFG_VLAN_TAG_MASK, regs::RXCFG_FILTER);

        // The driver does not use the receive checksum offload, so the
        // hardware should not report checksum status.
        self.regs.update16(regs::CPLUS_COMMAND, 1 << 5, 0);

        // Accept every multicast group; netstack filters in software.
        self.regs.write32(regs::MAR0, u32::MAX);
        self.regs.write32(regs::MAR4, u32::MAX);

        self.cfg_unlock();
        self.aspm_clkreq_lock(false);
        self.force_clkreq(false);
        self.aspm_clkreq_lock(true);
        self.force_clkreq(false);
        self.cfg_lock();

        self.regs.write8(
            regs::COMMAND,
            regs::COMMAND_TX_ENABLE | regs::COMMAND_RX_ENABLE,
        );
        self.regs.write32(regs::ISR_V2, u32::MAX);
        self.regs
            .write32(regs::IMR_V2_SET, regs::ISR_RXQ0_OK | regs::ISR_TXQ0_OK);

        // The largest frame the controller may hand over, with room for the
        // Ethernet header, a VLAN tag and the CRC.
        self.regs.write16(regs::RX_MAX_SIZE, regs::RX_MAX_SIZE_1500);

        let phy_status = self.regs.read32(regs::PHY_STATUS);
        self.link_up = phy_status & regs::PHY_STATUS_LINK != 0;
        if self.link_up {
            self.link_up_patch(phy_status);
            log::info!(
                "RTL8125B link is up at {} Mb/s",
                regs::link_speed_mbps(phy_status)
            );
        } else {
            log::info!("RTL8125B link is down; waiting for the first interrupt");
        }
    }
}

/// An RTL8125B network adapter.
pub struct Rtl8125 {
    controller: Controller<MmioRegisters>,
    receive_buffer: [Dma<[Mmio<u8>; RX_BUFFER_SIZE]>; RX_RING_LEN],
    receive_ring: Dma<[Descriptor; RX_RING_LEN]>,
    receive_i: usize,
    transmit_buffer: [Dma<[Mmio<u8>; TX_BUFFER_SIZE]>; TX_RING_LEN],
    transmit_ring: Dma<[Descriptor; TX_RING_LEN]>,
    transmit_i: usize,
}

impl NetworkAdapter for Rtl8125 {
    fn mac_address(&mut self) -> [u8; 6] {
        self.controller.mac_address
    }

    fn available_for_read(&mut self) -> usize {
        self.next_read()
    }

    fn read_packet(&mut self, buf: &mut [u8]) -> Result<Option<usize>> {
        if self.receive_i >= RX_RING_LEN {
            self.receive_i = 0;
        }

        let index = self.receive_i;
        let ctrl = self.receive_ring[index].ctrl.read();
        if desc::is_owned(ctrl) {
            return Ok(None);
        }

        let length = received_length(ctrl);
        let mut copied = 0;
        for (destination, source) in buf.iter_mut().zip(self.receive_buffer[index].iter()) {
            if copied == length {
                break;
            }
            *destination = source.read();
            copied += 1;
        }

        self.receive_ring[index]
            .ctrl
            .write(desc::rx_command(RX_BUFFER_SIZE, index + 1 == RX_RING_LEN));
        self.receive_i += 1;

        if copied == 0 {
            // A frame without a payload is dropped rather than reported as an
            // empty read, which the network stack would treat as end of file.
            return Ok(None);
        }

        Ok(Some(copied))
    }

    fn write_packet(&mut self, buf: &[u8]) -> Result<usize> {
        if buf.len() > TX_BUFFER_SIZE {
            return Err(Error::new(EMSGSIZE));
        }

        let timeout = Timeout::from_millis(100);
        loop {
            if self.transmit_i >= TX_RING_LEN {
                self.transmit_i = 0;
            }

            let index = self.transmit_i;
            let td = &mut self.transmit_ring[index];
            if td.ctrl.readf(desc::OWN) {
                // The transmit ring is full. Wait for the controller to release
                // a descriptor instead of spinning forever, which would stall
                // the network scheme if the link never comes up.
                if timeout.run().is_err() {
                    return Err(Error::new(EAGAIN));
                }
                continue;
            }

            for (destination, byte) in self.transmit_buffer[index].iter_mut().zip(buf) {
                destination.write(*byte);
            }

            td.ctrl
                .write(desc::tx_command(buf.len(), index + 1 == TX_RING_LEN));

            // The RTL8125 uses a 16-bit normal-priority queue doorbell at 0x90.
            self.controller.regs.write16(regs::TPPOLL, regs::TPPOLL_NPQ);

            self.transmit_i += 1;
            return Ok(buf.len());
        }
    }
}

impl Rtl8125 {
    /// Initialise the controller and its DMA rings.
    ///
    /// # Errors
    /// Returns `ENODEV` for an unsupported MAC revision or invalid station address,
    /// allocation errors for DMA memory, or `EIO` when controller/PHY handshakes time out.
    ///
    /// # Safety
    ///
    /// `base` must point to the uncacheable memory BAR of an RTL8125 function
    /// that pcid has bound to this driver. The mapping must stay valid for the
    /// lifetime of the returned adapter.
    pub unsafe fn new(base: usize) -> Result<Self> {
        // SAFETY: the caller guarantees the BAR mapping.
        let regs = unsafe { MmioRegisters::new(base) };
        check_mac_version(&regs)?;

        let receive_buffer = (0..RX_RING_LEN)
            // SAFETY: zeroed DMA memory is valid for every byte value and is
            // fully written before the controller is allowed to read it.
            .map(|_| Ok(unsafe { Dma::zeroed()?.assume_init() }))
            .collect::<Result<Vec<_>>>()?
            .try_into()
            .map_err(|_| Error::new(EIO))?;
        let transmit_buffer = (0..TX_RING_LEN)
            // SAFETY: see the receive buffers above.
            .map(|_| Ok(unsafe { Dma::zeroed()?.assume_init() }))
            .collect::<Result<Vec<_>>>()?
            .try_into()
            .map_err(|_| Error::new(EIO))?;

        let mut adapter = Self {
            controller: Controller::new(regs),
            receive_buffer,
            // SAFETY: a zeroed descriptor has owner clear, so the controller
            // will not touch it until `setup_rings` writes valid addresses.
            receive_ring: unsafe { Dma::zeroed()?.assume_init() },
            receive_i: 0,
            transmit_buffer,
            // SAFETY: see the receive ring.
            transmit_ring: unsafe { Dma::zeroed()?.assume_init() },
            transmit_i: 0,
        };

        adapter.controller.init()?;
        adapter.setup_rings();
        adapter.controller.start();

        Ok(adapter)
    }

    /// Read and acknowledge the interrupt status.
    ///
    /// Returns true when the status contains a receive or transmit event that
    /// the network scheme should service.
    pub fn irq(&mut self) -> bool {
        self.controller.irq()
    }

    fn next_read(&self) -> usize {
        let index = if self.receive_i >= RX_RING_LEN {
            0
        } else {
            self.receive_i
        };
        let ctrl = self.receive_ring[index].ctrl.read();
        if desc::is_owned(ctrl) {
            0
        } else {
            // A frame with no payload still has to be reported, or the ring
            // would stall waiting for a read that never arrives.
            received_length(ctrl).max(1)
        }
    }

    fn setup_rings(&mut self) {
        for (index, descriptor) in self.receive_ring.iter_mut().enumerate() {
            let physical = self.receive_buffer[index].physical();
            descriptor.buffer_low.write(physical as u32);
            descriptor.buffer_high.write((physical as u64 >> 32) as u32);
            descriptor
                .ctrl
                .write(desc::rx_command(RX_BUFFER_SIZE, index + 1 == RX_RING_LEN));
        }

        for (index, descriptor) in self.transmit_ring.iter_mut().enumerate() {
            let physical = self.transmit_buffer[index].physical();
            descriptor.buffer_low.write(physical as u32);
            descriptor.buffer_high.write((physical as u64 >> 32) as u32);
            descriptor.ctrl.write(if index + 1 == TX_RING_LEN {
                desc::EOR
            } else {
                0
            });
        }

        let receive = self.receive_ring.physical();
        self.controller.regs.write32(regs::RDSAR, receive as u32);
        self.controller
            .regs
            .write32(regs::RDSAR + 4, (receive as u64 >> 32) as u32);

        let transmit = self.transmit_ring.physical();
        self.controller.regs.write32(regs::TNPDS, transmit as u32);
        self.controller
            .regs
            .write32(regs::TNPDS + 4, (transmit as u64 >> 32) as u32);
    }
}

fn is_valid_mac(mac: &[u8; 6]) -> bool {
    *mac != [0; 6] && *mac != [0xFF; 6] && mac[0] & 1 == 0
}

/// Check that the controller is an RTL8125B.
///
/// The MAC version code sits in the upper bits of the transmit configuration
/// register. The driver refuses to touch a controller it does not recognise,
/// because the MAC and PHY patch tables are specific to the revision.
fn check_mac_version<R: Regs>(regs: &R) -> Result<()> {
    let txcfg = regs.read32(regs::TXCFG);
    let Some(mac_version) = version::detect(txcfg) else {
        log::error!("unknown RTL8125 MAC version (TXCFG {txcfg:#010X})");
        return Err(Error::new(ENODEV));
    };
    if !version::is_supported(mac_version) {
        log::error!("RTL8125 MAC version {mac_version:?} is not supported yet");
        return Err(Error::new(ENODEV));
    }
    log::info!("RTL8125B detected (TXCFG {txcfg:#010X})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    /// Size of the modelled register file.
    const BAR_SIZE: usize = 0x1_0000;
    /// Size of the modelled OCP address space.
    const OCP_SIZE: usize = 0x1_0000;

    /// The state of the modelled controller.
    #[derive(Default)]
    struct FakeState {
        bar: Vec<u8>,
        ocp: Vec<u16>,
        mac_pending: Option<u16>,
        mac_page: u16,
        phy_pending: Option<(u16, bool)>,
        ephy_pending: Option<(u16, bool)>,
        /// The MAC OCP writes, with the MCU RAM page each one was made on.
        mac_writes: Vec<(u16, u16, u16)>,
        phy_writes: Vec<(u16, u16)>,
        ephy_writes: Vec<(u16, u16)>,
    }

    impl FakeState {
        fn put8(&mut self, offset: u16, value: u8) {
            self.bar[usize::from(offset)] = value;
        }

        fn put16(&mut self, offset: u16, value: u16) {
            let bytes = value.to_le_bytes();
            self.bar[usize::from(offset)] = bytes[0];
            self.bar[usize::from(offset) + 1] = bytes[1];
        }

        fn put32(&mut self, offset: u16, value: u32) {
            let bytes = value.to_le_bytes();
            for (index, byte) in bytes.iter().enumerate() {
                self.bar[usize::from(offset) + index] = *byte;
            }
        }

        fn get16(&self, offset: u16) -> u16 {
            u16::from_le_bytes([
                self.bar[usize::from(offset)],
                self.bar[usize::from(offset) + 1],
            ])
        }

        fn get32(&self, offset: u16) -> u32 {
            let mut bytes = [0u8; 4];
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = self.bar[usize::from(offset) + index];
            }
            u32::from_le_bytes(bytes)
        }
    }

    /// A model of the RTL8125B controller.
    ///
    /// The model answers the reads the driver makes, records every OCP write,
    /// and models the two handshakes the driver waits for: the PHY MCU patch
    /// request and the reset.
    #[derive(Clone)]
    struct FakeDevice {
        state: Rc<RefCell<FakeState>>,
    }

    impl FakeDevice {
        fn new() -> Self {
            let mut state = FakeState {
                bar: vec![0; BAR_SIZE],
                ocp: vec![0; OCP_SIZE],
                ..FakeState::default()
            };

            // The RTL8125B reports its MAC version in the transmit
            // configuration register.
            state.put32(regs::TXCFG, 0x6410_0000);
            // A 2.5 Gb/s full-duplex link.
            state.put32(
                regs::PHY_STATUS,
                regs::PHY_STATUS_LINK | regs::PHY_STATUS_FULL_DUP | regs::PHY_STATUS_2500M,
            );
            // The station address in the backup registers.
            state.put32(
                regs::BACKUP_MAC_ADDR,
                u32::from_le_bytes([0x10, 0x7B, 0x44, 0x11]),
            );
            state.put16(regs::BACKUP_MAC_ADDR + 4, u16::from_le_bytes([0x22, 0x33]));
            // The out-of-box handshakes the driver waits for.
            state.put16(0x00D2, 1 << 9);
            state.put8(regs::MCU_COMMAND, regs::MCU_COMMAND_FIFO_EMPTY);
            state.put16(regs::INTR_MITIGATE, regs::INTR_MITIGATE_IDLE);
            // The PHY is powered up, but its patch version does not match the
            // one the driver carries.
            state.ocp[usize::from(regs::PHY_STATE)] = regs::PHY_STATE_READY;

            Self {
                state: Rc::new(RefCell::new(state)),
            }
        }

        fn with_state<T>(&self, f: impl FnOnce(&FakeState) -> T) -> T {
            f(&self.state.borrow())
        }
    }

    impl Regs for FakeDevice {
        fn read8(&self, offset: u16) -> u8 {
            self.state.borrow().bar[usize::from(offset)]
        }

        fn write8(&self, offset: u16, value: u8) {
            let mut state = self.state.borrow_mut();
            let value = if offset == regs::COMMAND {
                // The reset completes immediately.
                value & !regs::COMMAND_RESET
            } else {
                value
            };
            state.bar[usize::from(offset)] = value;
        }

        fn read16(&self, offset: u16) -> u16 {
            self.state.borrow().get16(offset)
        }

        fn write16(&self, offset: u16, value: u16) {
            self.state.borrow_mut().put16(offset, value);
        }

        fn read32(&self, offset: u16) -> u32 {
            let mut state = self.state.borrow_mut();
            match offset {
                regs::MAC_OCP => {
                    let address = state.mac_pending.take().unwrap_or(0);
                    u32::from(state.ocp[usize::from(address)])
                }
                regs::PHY_OCP => match state.phy_pending.take() {
                    Some((address, false)) => {
                        u32::from(state.ocp[usize::from(address)]) | ocp::FLAG
                    }
                    _ => 0,
                },
                regs::EPHY_ACCESS => match state.ephy_pending.take() {
                    Some((address, false)) => {
                        u32::from(state.ocp[usize::from(address)]) | ocp::FLAG
                    }
                    _ => 0,
                },
                _ => state.get32(offset),
            }
        }

        fn write32(&self, offset: u16, value: u32) {
            let mut state = self.state.borrow_mut();
            match offset {
                regs::MAC_OCP => {
                    let address = (((value >> 16) & 0x7FFF) as u16) * 2;
                    if value & ocp::FLAG != 0 {
                        let data = value as u16;
                        state.ocp[usize::from(address)] = data;
                        let page = state.mac_page;
                        state.mac_writes.push((page, address, data));
                        if address == regs::MCU_PAGE_SELECT {
                            state.mac_page = data & 0x3;
                        }
                        state.mac_pending = None;
                    } else {
                        state.mac_pending = Some(address);
                    }
                }
                regs::PHY_OCP => {
                    let address = (((value >> 16) & 0x7FFF) as u16) * 2;
                    let is_write = value & ocp::FLAG != 0;
                    if is_write {
                        let data = value as u16;
                        state.ocp[usize::from(address)] = data;
                        state.phy_writes.push((address, data));
                        if address == regs::PHY_MCU_REQUEST {
                            // The patch request is answered by the status bit.
                            let ready = data & regs::PHY_MCU_BIT != 0;
                            state.ocp[usize::from(regs::PHY_MCU_STATUS)] =
                                if ready { regs::PHY_MCU_READY } else { 0 };
                        }
                    }
                    state.phy_pending = Some((address, is_write));
                }
                regs::EPHY_ACCESS => {
                    let address = ((value >> 16) & 0x7FFF) as u16;
                    let is_write = value & ocp::FLAG != 0;
                    if is_write {
                        let data = value as u16;
                        state.ocp[usize::from(address)] = data;
                        state.ephy_writes.push((address, data));
                    }
                    state.ephy_pending = Some((address, is_write));
                }
                _ => state.put32(offset, value),
            }
        }
    }

    /// A controller driven by the model, initialised the way the driver does.
    fn initialised() -> (FakeDevice, Controller<FakeDevice>) {
        let device = FakeDevice::new();
        let handle = device.clone();
        let mut controller = Controller::new(device);
        controller
            .init()
            .expect("the modelled controller should initialise");
        controller.start();
        (handle, controller)
    }

    #[test]
    fn accepts_unicast_station_addresses() {
        assert!(is_valid_mac(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]));
        assert!(is_valid_mac(&[0x10, 0x7B, 0x44, 0x11, 0x22, 0x33]));
    }

    #[test]
    fn rejects_zero_broadcast_and_multicast_addresses() {
        assert!(!is_valid_mac(&[0; 6]));
        assert!(!is_valid_mac(&[0xFF; 6]));
        assert!(!is_valid_mac(&[0x01, 0x00, 0x5E, 0x00, 0x00, 0x01]));
    }

    #[test]
    fn buffer_sizes_fit_descriptor_length_fields() {
        assert!(RX_BUFFER_SIZE <= desc::RX_LEN_MASK as usize);
        assert!(TX_BUFFER_SIZE <= desc::TX_LEN_MASK as usize);
    }

    #[test]
    fn receive_length_excludes_the_ethernet_crc() {
        assert_eq!(received_length(1518), 1514);
        assert_eq!(received_length(64), 60);
        assert_eq!(received_length(4), 0);
        assert_eq!(received_length(0), 0);
    }

    #[test]
    fn recognises_the_mac_version_in_the_transmit_configuration() {
        let device = FakeDevice::new();
        assert!(check_mac_version(&device).is_ok());
    }

    #[test]
    fn rejects_an_unknown_mac_version() {
        let device = FakeDevice::new();
        device.state.borrow_mut().put32(regs::TXCFG, 0x0000_0000);
        assert!(check_mac_version(&device).is_err());
    }

    #[test]
    fn writes_the_mac_patch_table_and_its_version() {
        let (device, _) = initialised();

        device.with_state(|state| {
            // The patch RAM is written one page at a time: the page register
            // selects the page the window at 0xF800 refers to.
            let expected: Vec<(u16, u16, u16)> = firmware::MAC_PATCH_8125B
                .iter()
                .enumerate()
                .map(|(index, word)| {
                    let page = (index / regs::MCU_PAGE_WORDS) as u16;
                    let offset = (index % regs::MCU_PAGE_WORDS) as u16 * 2;
                    (page, regs::MCU_RAM_BASE + offset, *word)
                })
                .collect();

            let written: Vec<(u16, u16, u16)> = state
                .mac_writes
                .iter()
                .copied()
                .filter(|&(_, address, _)| {
                    (regs::MCU_RAM_BASE..regs::MCU_RAM_BASE + 0x200).contains(&address)
                })
                .collect();

            assert_eq!(&written[..expected.len()], &expected[..]);

            // The patch version follows the table, on page 2 and most
            // significant half first.
            let version: Vec<(u16, u16, u16)> = written[expected.len()..].to_vec();
            assert_eq!(
                version,
                vec![
                    (2, 0xF9FE, 0x1203),
                    (2, 0xF9FC, 0x0604),
                    (2, 0xF9FA, 0x0119),
                    (2, 0xF9F8, 0x6637),
                ]
            );
        });
    }

    #[test]
    fn writes_the_phy_patch_table_and_its_version() {
        let (device, _) = initialised();

        device.with_state(|state| {
            let start = state
                .phy_writes
                .iter()
                .position(|&(address, value)| (address, value) == (0xA436, 0x8024))
                .expect("the PHY patch table should be written");

            let expected: Vec<(u16, u16)> = firmware::PHY_PATCH_8125B
                .chunks_exact(2)
                .take_while(|pair| *pair != [0xFFFF, 0xFFFF])
                .map(|pair| (pair[0], pair[1]))
                .collect();

            let written: Vec<(u16, u16)> = state.phy_writes[start..start + expected.len()].to_vec();
            assert_eq!(written, expected);

            // The driver writes the version it carries after patching.
            assert!(
                state
                    .phy_writes
                    .contains(&(0xA438, firmware::PHY_PATCH_8125B_VERSION))
            );
        });
    }

    #[test]
    fn writes_the_ephy_sequence() {
        let (device, _) = initialised();

        device.with_state(|state| {
            assert_eq!(state.ephy_writes, firmware::EPHY_CONFIG_8125B);
        });
    }

    #[test]
    fn selects_the_legacy_descriptor_format() {
        let (device, _) = initialised();

        device.with_state(|state| {
            let last = state
                .mac_writes
                .iter()
                .rev()
                .find(|&&(_, address, _)| address == 0xEB58)
                .expect("the descriptor format register should be written");
            assert_eq!(last.2 & 1, 0);
        });
    }

    #[test]
    fn leaves_the_controller_running_with_the_queue_interrupts_unmasked() {
        let (device, _) = initialised();

        device.with_state(|state| {
            assert_eq!(
                state.bar[usize::from(regs::COMMAND)],
                regs::COMMAND_TX_ENABLE | regs::COMMAND_RX_ENABLE
            );
            assert_eq!(
                state.get32(regs::IMR_V2_SET),
                regs::ISR_RXQ0_OK | regs::ISR_TXQ0_OK
            );
            assert_eq!(
                state.get32(regs::INTR_MITIGATE_8125B),
                regs::INTR_MITIGATE_VALUE
            );
            assert_eq!(
                state.get32(regs::RXCFG),
                regs::RXCFG_INITIAL & !regs::RXCFG_VLAN_TAG_MASK | regs::RXCFG_FILTER
            );
            assert_eq!(state.get32(regs::TXCFG), regs::TXCFG_CONFIG);
            assert_eq!(state.get16(regs::RX_MAX_SIZE), regs::RX_MAX_SIZE_1500);
        });
    }

    #[test]
    fn programs_the_station_address() {
        let (device, _) = initialised();

        device.with_state(|state| {
            let low = state.get32(regs::MAC_ADDR);
            let high = state.get16(regs::MAC_ADDR + 4);
            let address = [
                low as u8,
                (low >> 8) as u8,
                (low >> 16) as u8,
                (low >> 24) as u8,
                high as u8,
                (high >> 8) as u8,
            ];
            assert_eq!(address, [0x10, 0x7B, 0x44, 0x11, 0x22, 0x33]);
        });
    }
}
