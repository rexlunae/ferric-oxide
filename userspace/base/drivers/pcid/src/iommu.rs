//! Ferric Oxide: DMA domains for drivers, through the kernel's `iommu` scheme
//! (`kernel.iommu = "auto"` or `"enforce"`; see `crates/iommu`).
//!
//! pcid opens the scheme's attach capability at start-up and reports every PCI-to-PCI bridge
//! it found, so the kernel can resolve DMAR device scopes that name bridges. When a driver
//! sends `GetDmaDomain` on its channel, pcid creates a domain, attaches the function and the
//! requester IDs its DMA can arrive with (aliases added by the bridges above it), and keeps the
//! domain's descriptor until the driver fetches it over the channel (`on_recvfd`, which moves
//! it to the driver). The domain then lives as long as the driver keeps the descriptor; when it
//! is closed (the driver exits or crashes) the kernel blocks the devices.
//!
//! Without the scheme (`kernel.iommu = "never"`, upstream's behaviour) none of this runs and
//! `GetDmaDomain` answers `NoDmaDomain`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::rc::Rc;

use ferric_iommu::abi::{Bridge, PciDevice, Request, DOMAIN_PATH, SCHEME_NAME};
use ferric_iommu::alias::{requester_ids, BridgeKind, UpstreamBridge};
use log::{debug, info, warn};
use pci_types::{ConfigRegionAccess, PciAddress, PciPciBridgeHeader};
use pcid_interface::{PcidClientResponse, PcidServerResponseError};
use redox_scheme::{OpenResult, RecvFdRequest, Socket};
use syscall::error::{Error, Result, EINVAL, ENOENT};
use syscall::{CallFlags, FmoveFdFlags};

use crate::cfg_access::Pcie;

/// The PCI Express capability's ID.
const CAP_PCI_EXPRESS: u32 = 0x10;

/// The iommu state of pcid.
#[derive(Default)]
pub struct Iommu {
    /// The attach capability, when the kernel has the scheme.
    cap: Option<libredox::Fd>,
    /// The bridge whose secondary bus each bus is, as found while scanning.
    parents: BTreeMap<u8, (UpstreamBridge, u8)>,
    /// Domains created for a channel that the driver has not fetched yet, by channel handle.
    pending: BTreeMap<usize, libredox::Fd>,
    /// Channels that already got a domain.
    given: BTreeSet<usize>,
    /// The scheme socket, which moves descriptors to a client.
    pub socket: Option<Rc<Socket>>,
}

/// Why a domain could not be set up for a function.
#[derive(Debug)]
enum DomainError {
    /// The kernel has no iommu scheme.
    NoScheme,
    /// The channel already got a domain.
    AlreadyGiven,
    /// Creating the domain failed.
    Create(libredox::error::Error),
    /// Attaching one of the function's requester IDs failed.
    Attach {
        id: PciDevice,
        source: libredox::error::Error,
    },
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoScheme => f.write_str("the kernel has no iommu scheme"),
            Self::AlreadyGiven => f.write_str("the channel already has a DMA domain"),
            Self::Create(err) => write!(f, "cannot create a domain: {err}"),
            Self::Attach { id, source } => write!(f, "cannot attach {id}: {source}"),
        }
    }
}

/// The ferric-iommu form of a PCI address.
fn device(addr: PciAddress) -> Option<PciDevice> {
    PciDevice::new(addr.segment(), addr.bus(), addr.device(), addr.function())
}

/// Makes a call to the iommu scheme with no payload.
fn call(fds: &[usize], request: Request) -> libredox::error::Result<usize> {
    let (words, len) = request.encode();
    libredox::call::call_ro(fds, &mut [], CallFlags::empty(), &words[..len])
}

/// The device/port type in the PCI Express capability of `addr`, if it has one.
fn pcie_port_type(pcie: &Pcie, addr: PciAddress) -> Option<u8> {
    // SAFETY: the header and capability registers are standard configuration space of a
    // function that exists, and reading them has no side effects.
    let read = |offset: u16| unsafe { pcie.read(addr, offset) };
    // Status register bit 4: there is a capability list.
    if read(0x04) & (1 << 20) == 0 {
        return None;
    }
    let mut offset = (read(0x34) & 0xfc) as u16;
    // Each capability takes at least 4 bytes of the 192 after the header, which bounds a
    // corrupt (looping) list.
    for _ in 0..48 {
        if offset < 0x40 {
            return None;
        }
        let header = read(offset);
        if header & 0xff == CAP_PCI_EXPRESS {
            return Some(((header >> 20) & 0xf) as u8);
        }
        offset = ((header >> 8) & 0xfc) as u16;
    }
    None
}

impl Iommu {
    /// Opens the attach capability, when the kernel has the iommu scheme.
    pub fn open() -> Self {
        let cap = match libredox::Fd::open(
            &format!("/scheme/{SCHEME_NAME}"),
            libredox::flag::O_CLOEXEC,
            0,
        ) {
            Ok(cap) => {
                info!("pcid: kernel iommu scheme found; drivers can get DMA domains");
                Some(cap)
            }
            Err(err) => {
                debug!("pcid: no kernel iommu scheme ({err}); DMA domains unavailable");
                None
            }
        };
        Self {
            cap,
            ..Self::default()
        }
    }

    /// Records a bridge found while scanning: its kind, and the buses behind it.
    pub fn record_bridge(&mut self, pcie: &Pcie, bridge: &PciPciBridgeHeader) {
        let addr = bridge.header().address();
        let Some(address) = device(addr) else {
            return;
        };
        let secondary = bridge.secondary_bus_number(pcie);
        let subordinate = bridge.subordinate_bus_number(pcie);
        let kind = BridgeKind::from_port_type(pcie_port_type(pcie, addr));
        let upstream = UpstreamBridge {
            address,
            secondary,
            kind,
        };
        self.parents.insert(secondary, (upstream, subordinate));
    }

    /// Reports the bridges found to the kernel.
    pub fn report_bridges(&self) {
        let Some(cap) = &self.cap else {
            return;
        };
        for &(bridge, subordinate) in self.parents.values() {
            let request = Request::Bridge(Bridge {
                address: bridge.address,
                secondary: bridge.secondary,
                subordinate: subordinate.max(bridge.secondary),
            });
            if let Err(err) = call(&[cap.raw()], request) {
                warn!("pcid: cannot report bridge {}: {err}", bridge.address);
            }
        }
    }

    /// The bridges above `bus`, nearest first.
    fn upstream(&self, mut bus: u8) -> Vec<UpstreamBridge> {
        let mut chain = Vec::new();
        while let Some(&(bridge, _)) = self.parents.get(&bus) {
            // A bridge's secondary bus is above its own, so a loop means bad bus numbers.
            if chain.len() > 255 || bridge.address.bus() >= bus {
                break;
            }
            chain.push(bridge);
            bus = bridge.address.bus();
        }
        chain
    }

    /// Creates a domain with `addr` and its aliases in it.
    fn create_domain(&self, addr: PciAddress) -> std::result::Result<libredox::Fd, DomainError> {
        let cap = self.cap.as_ref().ok_or(DomainError::NoScheme)?;
        let function = device(addr).ok_or(DomainError::NoScheme)?;
        let domain = cap
            .openat(DOMAIN_PATH, libredox::flag::O_CLOEXEC, 0)
            .map_err(DomainError::Create)?;
        let ids = requester_ids(function, &self.upstream(function.bus()));
        for &id in &ids {
            match call(&[cap.raw(), domain.raw()], Request::Attach(id)) {
                Ok(_) => {}
                // An alias that no remapping unit serves cannot carry remapped DMA anyway.
                Err(err) if id != function && err.errno() == libredox::errno::ENODEV => {
                    debug!("pcid: alias {id} of {function} is not remapped");
                }
                Err(source) => return Err(DomainError::Attach { id, source }),
            }
        }
        let aliases = ids
            .iter()
            .skip(1)
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        info!(
            "pcid: {function} attached to its own DMA domain (aliases: [{}])",
            aliases.join(", ")
        );
        Ok(domain)
    }

    /// Answers `GetDmaDomain` on channel `id` for function `addr`.
    pub fn get_dma_domain(&mut self, id: usize, addr: PciAddress) -> PcidClientResponse {
        let result = if self.given.contains(&id) {
            Err(DomainError::AlreadyGiven)
        } else {
            self.create_domain(addr)
        };
        match result {
            Ok(domain) => {
                self.given.insert(id);
                self.pending.insert(id, domain);
                PcidClientResponse::DmaDomain
            }
            Err(err) => {
                if self.cap.is_some() {
                    warn!("pcid: no DMA domain for {addr}: {err}");
                }
                PcidClientResponse::Error(PcidServerResponseError::NoDmaDomain)
            }
        }
    }

    /// Moves the domain created for channel `id` to the client.
    pub fn on_recvfd(&mut self, request: &RecvFdRequest) -> Result<OpenResult> {
        let socket = self.socket.as_ref().ok_or(Error::new(EINVAL))?;
        if request.num_fds() != 1 {
            return Err(Error::new(EINVAL));
        }
        let domain = self
            .pending
            .remove(&request.id())
            .ok_or(Error::new(ENOENT))?;
        request.move_fd(socket, FmoveFdFlags::empty(), &[domain.raw()])?;
        // The kernel moved the descriptor out of pcid's table.
        let _ = domain.into_raw();
        Ok(OpenResult::OtherSchemeMultiple { num_fds: 1 })
    }

    /// Forgets channel `id`, closing a domain the driver never fetched.
    pub fn on_close(&mut self, id: usize) {
        self.pending.remove(&id);
        self.given.remove(&id);
    }
}
