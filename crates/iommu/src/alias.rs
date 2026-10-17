//! The requester IDs a PCI function's DMA can carry when it reaches the IOMMU.
//!
//! On PCI Express every request carries the requester ID of the function that issued it, and
//! root ports and switch ports pass it on unchanged. Conventional PCI and PCI-X have no such
//! field on every transaction, so a bridge between them and PCI Express has to supply one.
//! The PCI Express to PCI/PCI-X Bridge Specification (revision 1.0, section 2.3) makes a
//! bridge forwarding a transaction upstream from its secondary side use its secondary bus
//! number with device and function 0, but bridges also use their own ID, and a conventional
//! PCI-to-PCI bridge below such a bridge can also be named. The IOMMU only sees the ID that
//! arrives, so a domain must hold all of them: [`requester_ids`] lists them so they can be
//! attached together.

use alloc::vec::Vec;

use crate::abi::PciDevice;

/// What kind of bridge sits above a function, from the bridge's PCI Express capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BridgeKind {
    /// A PCI Express root port or switch port (device/port types 4, 5 and 6): requester IDs
    /// pass through unchanged.
    PciExpressPort,
    /// A PCI Express to PCI/PCI-X bridge (device/port type 7): requests from its secondary side
    /// carry its secondary bus with device and function 0, or the bridge's own ID.
    PciExpressToPci,
    /// A bridge without a PCI Express capability, or with another device/port type (such as a
    /// PCI/PCI-X to PCI Express bridge, type 8): requests forwarded through it may carry the
    /// bridge's own ID.
    Other,
}

impl BridgeKind {
    /// The kind for a bridge whose PCI Express capability has device/port type `port_type`
    /// (bits 7:4 of the PCI Express Capabilities register), or that has no such capability
    /// (`None`).
    pub const fn from_port_type(port_type: Option<u8>) -> Self {
        match port_type {
            Some(4..=6) => Self::PciExpressPort,
            Some(7) => Self::PciExpressToPci,
            _ => Self::Other,
        }
    }
}

/// A bridge above a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpstreamBridge {
    /// The bridge's address.
    pub address: PciDevice,
    /// Its secondary bus number.
    pub secondary: u8,
    /// What kind of bridge it is.
    pub kind: BridgeKind,
}

/// The requester IDs, as addresses, that DMA from `function` can carry at the IOMMU: its own
/// first, then the aliases the bridges above it add. `upstream` lists those bridges from the
/// nearest to the root. The list has no duplicates.
///
/// # Examples
///
/// ```
/// use ferric_iommu::abi::PciDevice;
/// use ferric_iommu::alias::{requester_ids, BridgeKind, UpstreamBridge};
///
/// let device = PciDevice::new(0, 5, 2, 0).unwrap();
/// let bridge = UpstreamBridge {
///     address: PciDevice::new(0, 4, 0, 0).unwrap(),
///     secondary: 5,
///     kind: BridgeKind::PciExpressToPci,
/// };
/// let ids = requester_ids(device, &[bridge]);
/// assert_eq!(ids, [device, PciDevice::new(0, 5, 0, 0).unwrap(), bridge.address]);
/// ```
pub fn requester_ids(function: PciDevice, upstream: &[UpstreamBridge]) -> Vec<PciDevice> {
    let mut ids = Vec::from([function]);
    let mut push = |id: PciDevice| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };
    for bridge in upstream {
        match bridge.kind {
            BridgeKind::PciExpressPort => {}
            BridgeKind::PciExpressToPci => {
                if let Some(id) = PciDevice::new(bridge.address.segment(), bridge.secondary, 0, 0) {
                    push(id);
                }
                push(bridge.address);
            }
            BridgeKind::Other => push(bridge.address),
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(bus: u8, device: u8, function: u8) -> PciDevice {
        PciDevice::new(0, bus, device, function).unwrap()
    }

    fn bridge(address: PciDevice, secondary: u8, kind: BridgeKind) -> UpstreamBridge {
        UpstreamBridge {
            address,
            secondary,
            kind,
        }
    }

    #[test]
    fn root_complex_device_has_no_aliases() {
        assert_eq!(requester_ids(dev(0, 2, 0), &[]), [dev(0, 2, 0)]);
    }

    #[test]
    fn pcie_ports_pass_ids_through() {
        let up = [
            bridge(dev(2, 0, 0), 3, BridgeKind::PciExpressPort),
            bridge(dev(1, 0, 0), 2, BridgeKind::PciExpressPort),
            bridge(dev(0, 0x1c, 0), 1, BridgeKind::PciExpressPort),
        ];
        assert_eq!(requester_ids(dev(3, 0, 0), &up), [dev(3, 0, 0)]);
    }

    #[test]
    fn conventional_pci_behind_pcie_bridge() {
        // A conventional PCI bridge below a PCI Express to PCI bridge below a root port.
        let up = [
            bridge(dev(5, 1, 0), 6, BridgeKind::Other),
            bridge(dev(4, 0, 0), 5, BridgeKind::PciExpressToPci),
            bridge(dev(0, 0x1c, 0), 4, BridgeKind::PciExpressPort),
        ];
        assert_eq!(
            requester_ids(dev(6, 3, 0), &up),
            [dev(6, 3, 0), dev(5, 1, 0), dev(5, 0, 0), dev(4, 0, 0)]
        );
    }

    #[test]
    fn secondary_bus_alias_can_be_the_device_itself() {
        let up = [bridge(dev(1, 0, 0), 2, BridgeKind::PciExpressToPci)];
        assert_eq!(
            requester_ids(dev(2, 0, 0), &up),
            [dev(2, 0, 0), dev(1, 0, 0)]
        );
    }

    #[test]
    fn port_types() {
        assert_eq!(
            BridgeKind::from_port_type(Some(4)),
            BridgeKind::PciExpressPort
        );
        assert_eq!(
            BridgeKind::from_port_type(Some(6)),
            BridgeKind::PciExpressPort
        );
        assert_eq!(
            BridgeKind::from_port_type(Some(7)),
            BridgeKind::PciExpressToPci
        );
        assert_eq!(BridgeKind::from_port_type(Some(8)), BridgeKind::Other);
        assert_eq!(BridgeKind::from_port_type(None), BridgeKind::Other);
    }
}
