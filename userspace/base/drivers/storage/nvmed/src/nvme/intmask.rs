//! Ferric Oxide: how nvmed masks a completion interrupt while it drains completion queues.
//!
//! The executor masks the queue pair's vector, polls its completion queues dry and unmasks it
//! again (`executor::LocalExecutor::react`). Which register that touches depends on how the
//! controller interrupts, and is decided once, when the driver starts ([`InterruptMask::new`]):
//!
//! - NVM Express Base Specification 1.4, sections 3.1.3 (INTMS, Interrupt Mask Set, offset 0Ch)
//!   and 3.1.4 (INTMC, Interrupt Mask Clear, offset 10h): the registers mask and unmask
//!   pin-based and MSI interrupts, one bit per vector, and host software shall not access them
//!   when the controller is configured for MSI-X.
//! - Section 7.5.2 (MSI-X based behavior): an MSI-X vector is masked through the Mask bit of
//!   its table entry's Vector Control word (the PCI MSI-X capability), which
//!   `pcid_interface::irq_helpers::InterruptVector::set_masked_if_fast` writes.

use pcid_interface::irq_helpers::InterruptMechanism;

use super::Iv;

/// The registers that mask nvmed's completion interrupts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptMask {
    /// Pin-based or MSI interrupts: INTMS sets a vector's mask bit and INTMC clears it.
    IntmsIntmc,
    /// MSI-X: the Mask bit of the vector's table entry. INTMS and INTMC are never accessed.
    MsixVectorControl,
}

/// One of the two interrupt mask registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskRegister {
    /// INTMS: each bit written as one masks that vector.
    Set,
    /// INTMC: each bit written as one unmasks that vector.
    Clear,
}

/// A write to INTMS or INTMC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaskWrite {
    /// The register to write.
    pub register: MaskRegister,
    /// The value to write: one bit per vector to mask or unmask.
    pub value: u32,
}

impl InterruptMask {
    /// The mask registers for a controller that interrupts through `mechanism`.
    pub fn new(mechanism: InterruptMechanism) -> Self {
        match mechanism {
            InterruptMechanism::Pin | InterruptMechanism::Msi => Self::IntmsIntmc,
            InterruptMechanism::MsiX => Self::MsixVectorControl,
        }
    }

    /// The INTMS or INTMC write that masks (`masked`) or unmasks `vector`.
    ///
    /// `None` under MSI-X, where the specification forbids accessing either register, and for
    /// a vector above 31, which has no bit (pin-based interrupts use vector 0 and MSI at most
    /// 32 vectors).
    pub fn register_write(self, vector: Iv, masked: bool) -> Option<MaskWrite> {
        match self {
            Self::MsixVectorControl => None,
            Self::IntmsIntmc => {
                let value = 1_u32.checked_shl(u32::from(vector))?;
                let register = if masked {
                    MaskRegister::Set
                } else {
                    MaskRegister::Clear
                };
                Some(MaskWrite { register, value })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_and_msi_use_the_mask_registers() {
        assert_eq!(
            InterruptMask::new(InterruptMechanism::Pin),
            InterruptMask::IntmsIntmc
        );
        assert_eq!(
            InterruptMask::new(InterruptMechanism::Msi),
            InterruptMask::IntmsIntmc
        );
    }

    #[test]
    fn msix_uses_the_vector_control_word() {
        assert_eq!(
            InterruptMask::new(InterruptMechanism::MsiX),
            InterruptMask::MsixVectorControl
        );
    }

    #[test]
    fn masking_sets_and_unmasking_clears_the_vector_bit() {
        let mask = InterruptMask::new(InterruptMechanism::Pin);
        assert_eq!(
            mask.register_write(0, true),
            Some(MaskWrite {
                register: MaskRegister::Set,
                value: 1
            })
        );
        assert_eq!(
            mask.register_write(0, false),
            Some(MaskWrite {
                register: MaskRegister::Clear,
                value: 1
            })
        );
        let mask = InterruptMask::new(InterruptMechanism::Msi);
        assert_eq!(
            mask.register_write(31, true),
            Some(MaskWrite {
                register: MaskRegister::Set,
                value: 1 << 31
            })
        );
    }

    #[test]
    fn a_vector_without_a_bit_writes_nothing() {
        let mask = InterruptMask::new(InterruptMechanism::Msi);
        assert_eq!(mask.register_write(32, true), None);
        assert_eq!(mask.register_write(Iv::MAX, false), None);
    }

    #[test]
    fn msix_never_writes_the_mask_registers() {
        let mask = InterruptMask::new(InterruptMechanism::MsiX);
        for vector in [0, 1, 31, 32, Iv::MAX] {
            assert_eq!(mask.register_write(vector, true), None);
            assert_eq!(mask.register_write(vector, false), None);
        }
    }
}
