//! Hardware facts for the Realtek RTL8125 family, shared by the driver and its
//! host tests.
//!
//! The driver lives in `userspace/base/drivers/net/rtl8125d`, which only builds
//! for Redox. This crate contains the parts that can be checked on the host:
//! MAC-version identification, register offsets, OCP address encoding, the
//! descriptor command words and the MAC/PHY firmware patch tables.
//!
//! The RTL8125 is not a register-compatible extension of the RTL8168. It has a
//! different interrupt map, a 32-bit interrupt status register, a different
//! transmit doorbell and a MAC MCU that must be patched through OCP before the
//! controller behaves reliably. The register facts here come from the
//! permissively licensed FreeBSD `if_re` driver; the tables in [`firmware`] are
//! hardware data rather than copied code. Linux and macOS drivers are GPL and
//! are not used as a source.
//!
//! [`version::detect`] reads the MAC version from the transmit configuration
//! register. [`ocp`] contains the two OCP access paths the RTL8125 uses: MAC
//! OCP through `0x00B0` and direct PHY OCP through `0x00B8`. [`desc`] defines
//! the 16-byte descriptor command words, and [`regs`] names the registers the
//! driver touches.
//!
//! # Examples
//!
//! ```
//! use ferric_rtl8125::desc::{EOR, OWN, RX_LEN_MASK, tx_command};
//! use ferric_rtl8125::ocp::mapped_phy_address;
//! use ferric_rtl8125::version::{MacVersion, detect};
//!
//! assert_eq!(detect(0x6410_0000), Some(MacVersion::Rtl8125B));
//! assert_eq!(mapped_phy_address(0, 0), 0xA400);
//! assert_eq!(tx_command(60, true) & EOR, EOR);
//! assert_eq!(tx_command(60, false) & OWN, OWN);
//! assert_eq!(RX_LEN_MASK, 0x3FFF);
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod desc;
pub mod firmware;
pub mod ocp;
pub mod regs;
pub mod version;
