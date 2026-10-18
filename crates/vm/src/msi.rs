//! Decoding the MSI a VMM or device asks the kernel to deliver to a guest (`Msi` on a VM fd).
//!
//! The format is the one x86 devices write (Intel SDM Vol. 3A, "Message Signalled Interrupts"):
//! the address is FEEx_xxxxh with the destination APIC ID in bits 19:12, the redirection hint in
//! bit 3 and the destination mode in bit 2; the data holds the vector in bits 7:0, the delivery
//! mode in bits 10:8, the level in bit 14 and the trigger mode in bit 15. The kernel's virtual
//! local APIC then delivers the decoded message to the vCPUs it names.

use thiserror::Error;

/// How a message is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeliveryMode {
    /// To every destination, with the given vector.
    Fixed,
    /// To the destination with the lowest priority.
    LowestPriority,
    /// A system-management interrupt.
    Smi,
    /// An NMI (the vector is ignored).
    Nmi,
    /// An INIT.
    Init,
    /// An external interrupt from a legacy controller.
    ExtInt,
}

/// A decoded MSI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Msi {
    /// The destination APIC ID, or the logical destination mask.
    pub destination: u8,
    /// The destination is logical rather than physical.
    pub logical: bool,
    /// The redirection hint: deliver to only one of the destinations.
    pub redirection_hint: bool,
    /// The vector.
    pub vector: u8,
    /// The delivery mode.
    pub delivery: DeliveryMode,
    /// Level-triggered rather than edge-triggered.
    pub level_triggered: bool,
}

impl Msi {
    /// Decodes an MSI address and data pair.
    ///
    /// # Errors
    ///
    /// [`MsiError::Address`] when the address is not in the FEEx_xxxxh window or sets reserved
    /// bits 11:4, [`MsiError::Data`] when the data sets reserved bits, and
    /// [`MsiError::DeliveryMode`] for a reserved delivery mode.
    pub const fn decode(address: u64, data: u32) -> Result<Self, MsiError> {
        // Bits 1:0 are don't-care; bits 11:4 are reserved (remappable-format messages set bit 4,
        // and a guest's devices never send those to the kernel).
        if address >> 20 != 0xfee || address & 0xff0 != 0 {
            return Err(MsiError::Address { address });
        }
        if data >> 16 != 0 || data & 0x3800 != 0 {
            return Err(MsiError::Data { data });
        }
        let delivery = match (data >> 8) & 7 {
            0 => DeliveryMode::Fixed,
            1 => DeliveryMode::LowestPriority,
            2 => DeliveryMode::Smi,
            4 => DeliveryMode::Nmi,
            5 => DeliveryMode::Init,
            7 => DeliveryMode::ExtInt,
            mode => return Err(MsiError::DeliveryMode { mode: mode as u8 }),
        };
        Ok(Self {
            destination: (address >> 12) as u8,
            logical: address & (1 << 2) != 0,
            redirection_hint: address & (1 << 3) != 0,
            vector: data as u8,
            delivery,
            level_triggered: data & (1 << 15) != 0,
        })
    }
}

/// Why an MSI was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum MsiError {
    /// The address is not a valid MSI address.
    #[error("{address:#x} is not an MSI address")]
    Address {
        /// The address.
        address: u64,
    },
    /// The data sets reserved bits.
    #[error("MSI data {data:#x} sets reserved bits")]
    Data {
        /// The data.
        data: u32,
    },
    /// The delivery mode is reserved.
    #[error("MSI delivery mode {mode} is reserved")]
    DeliveryMode {
        /// The mode.
        mode: u8,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_decode() {
        let msi = Msi::decode(0xfee0_3000, 0x0031).expect("valid");
        assert_eq!(
            msi,
            Msi {
                destination: 3,
                logical: false,
                redirection_hint: false,
                vector: 0x31,
                delivery: DeliveryMode::Fixed,
                level_triggered: false,
            }
        );
        let nmi = Msi::decode(0xfee0_100c, 0x4400).expect("valid");
        assert!(nmi.logical && nmi.redirection_hint);
        assert_eq!(nmi.delivery, DeliveryMode::Nmi);
        assert!(
            Msi::decode(0xfee0_0000, 0x8000)
                .expect("valid")
                .level_triggered
        );
    }

    #[test]
    fn bad_messages_are_refused() {
        assert_eq!(
            Msi::decode(0xfed0_0000, 0),
            Err(MsiError::Address {
                address: 0xfed0_0000
            })
        );
        assert_eq!(
            Msi::decode(0x1_fee0_0000, 0),
            Err(MsiError::Address {
                address: 0x1_fee0_0000
            })
        );
        assert!(matches!(
            Msi::decode(0xfee0_0010, 0),
            Err(MsiError::Address { .. })
        ));
        assert!(Msi::decode(0xfee0_0003, 0).is_ok());
        assert_eq!(
            Msi::decode(0xfee0_0000, 0x1_0000),
            Err(MsiError::Data { data: 0x1_0000 })
        );
        assert_eq!(
            Msi::decode(0xfee0_0000, 0x0300),
            Err(MsiError::DeliveryMode { mode: 3 })
        );
        assert_eq!(
            Msi::decode(0xfee0_0000, 0x0600),
            Err(MsiError::DeliveryMode { mode: 6 })
        );
    }
}
