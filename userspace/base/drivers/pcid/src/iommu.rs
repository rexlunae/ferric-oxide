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
//! A driver's `MsiMessage` request is answered here too. When the kernel remaps interrupts,
//! pcid asks it for an interrupt remapping entry (`AllocIrte`) that delivers the vector to the
//! CPU and accepts requests only from the function (or, behind a PCI Express to PCI bridge, from
//! the buses its aliases are on), and answers with the remappable-format message naming it. The
//! entries are freed when the channel closes. A driver therefore never writes a
//! compatibility-format message, which the kernel has the remapping unit block.
//!
//! Without the scheme (`kernel.iommu = "never"`, upstream's behaviour) none of this runs:
//! `GetDmaDomain` answers `NoDmaDomain`, and `MsiMessage` answers the compatibility-format
//! message upstream drivers computed themselves.
//!
//! A compatibility-format message names an APIC id, while drivers ask for a logical CPU (the
//! number `irq:cpu-XX` takes). When the kernel has the root-only `irq:cpus` map
//! (`kernel.irq_percpu`), pcid looks the APIC id up there, and refuses a CPU whose APIC id the
//! format cannot carry. Without the map it takes them to be the same number, as upstream did.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::rc::Rc;

use ferric_iommu::abi::{
    Bridge, MsiMessage, MsiRoute, PciDevice, Request, SourceCheck, DOMAIN_PATH, SCHEME_NAME,
};
use ferric_iommu::alias::{requester_ids, BridgeKind, UpstreamBridge};
use log::{debug, info, warn};
use pci_types::{ConfigRegionAccess, PciAddress, PciPciBridgeHeader};
use pcid_interface::msi::MsiAddrAndData;
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
    /// The interrupt remapping entries made for each channel, by channel handle.
    irtes: BTreeMap<usize, Vec<u32>>,
    /// The scheme socket, which moves descriptors to a client.
    pub socket: Option<Rc<Socket>>,
    /// The kernel's logical CPU to APIC id map, for compatibility-format messages.
    cpus: CpuMap,
}

/// Where the APIC id of a logical CPU comes from.
#[derive(Debug, Default)]
enum CpuMap {
    /// `irq:cpus` has not been read yet.
    #[default]
    Unread,
    /// The kernel has no `irq:cpus` (built without `irq-percpu`): logical and APIC ids are taken
    /// to be the same, as upstream drivers took them.
    Same,
    /// The text of `irq:cpus`.
    Listed(String),
}

/// The path of the kernel's CPU map.
const CPUS_PATH: &str = "/scheme/irq/cpus";

/// Why there is no compatibility-format destination for a logical CPU.
#[derive(Debug)]
enum DestinationError {
    /// Reading `irq:cpus` failed.
    Read(std::io::Error),
    /// `irq:cpus` has a malformed line.
    Parse(ferric_irq::ParseError),
    /// `irq:cpus` does not list the CPU.
    UnknownCpu(u32),
    /// The CPU's APIC id cannot be named by a compatibility-format MSI address.
    Unrepresentable { cpu: u32, apic: ferric_irq::ApicId },
}

impl fmt::Display for DestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(_) => write!(f, "cannot read {CPUS_PATH}"),
            Self::Parse(_) => write!(f, "{CPUS_PATH} is malformed"),
            Self::UnknownCpu(cpu) => write!(f, "{CPUS_PATH} does not list CPU {cpu}"),
            Self::Unrepresentable { cpu, apic } => write!(
                f,
                "CPU {cpu} has APIC id {}, which a compatibility-format MSI address cannot name",
                apic.get()
            ),
        }
    }
}

impl std::error::Error for DestinationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(err) => Some(err),
            Self::Parse(err) => Some(err),
            Self::UnknownCpu(_) | Self::Unrepresentable { .. } => None,
        }
    }
}

/// `err` and its sources, for a log line.
fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

impl CpuMap {
    /// Reads the kernel's map.
    fn read() -> std::result::Result<Self, DestinationError> {
        match std::fs::read_to_string(CPUS_PATH) {
            Ok(text) => Ok(Self::Listed(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::Same),
            Err(err) => Err(DestinationError::Read(err)),
        }
    }

    /// The APIC id of logical CPU `cpu`, reading the map first when it has not been read or
    /// does not list `cpu` yet (an AP that came up after the last read).
    fn apic_id(&mut self, cpu: u32) -> std::result::Result<ferric_irq::ApicId, DestinationError> {
        if matches!(self, Self::Unread) || self.lookup(cpu)?.is_none() {
            *self = Self::read()?;
        }
        self.lookup(cpu)?.ok_or(DestinationError::UnknownCpu(cpu))
    }

    /// The APIC id of `cpu` in the map as read, `None` when it is unread or does not list it.
    fn lookup(
        &self,
        cpu: u32,
    ) -> std::result::Result<Option<ferric_irq::ApicId>, DestinationError> {
        match self {
            Self::Unread => Ok(None),
            Self::Same => Ok(Some(ferric_irq::ApicId::new(cpu))),
            Self::Listed(text) => ferric_irq::apic_of(text, ferric_irq::CpuId::new(cpu))
                .map_err(DestinationError::Parse),
        }
    }

    /// The compatibility-format destination of logical CPU `cpu`.
    fn destination(&mut self, cpu: u32) -> std::result::Result<u8, DestinationError> {
        let apic = self.apic_id(cpu)?;
        apic.compatibility_destination()
            .ok_or(DestinationError::Unrepresentable { cpu, apic })
    }
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

/// The compatibility-format message for `vector` on the CPU whose APIC id is `apic` (a
/// [`CpuMap::destination`]), as upstream drivers computed it: fixed delivery, edge-triggered,
/// physical destination.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn compatibility_message(apic: u8, vector: u8) -> Option<MsiAddrAndData> {
    use pcid_interface::msi::x86::{message_address, message_data_edge_triggered, DeliveryMode};
    Some(MsiAddrAndData {
        addr: message_address(apic, false, false),
        data: message_data_edge_triggered(DeliveryMode::Fixed, vector),
    })
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn compatibility_message(_apic: u8, _vector: u8) -> Option<MsiAddrAndData> {
    None
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

    /// Which requester IDs an interrupt remapping entry for `function` accepts: its own, or
    /// the buses of all its aliases when a bridge adds some.
    fn source_check(&self, function: PciDevice) -> SourceCheck {
        let ids = requester_ids(function, &self.upstream(function.bus()));
        if ids.iter().all(|&id| id == function) {
            return SourceCheck::Exact;
        }
        let buses = ids.iter().map(|id| id.bus());
        SourceCheck::Bus {
            first: buses.clone().min().unwrap_or(function.bus()),
            last: buses.max().unwrap_or(function.bus()),
        }
    }

    /// Has the kernel make an interrupt remapping entry for `vector` on `cpu` for `function`.
    fn alloc_irte(
        &self,
        function: PciDevice,
        cpu: u32,
        vector: u8,
    ) -> libredox::error::Result<(u32, MsiMessage)> {
        let cap = self
            .cap
            .as_ref()
            .ok_or(libredox::error::Error::new(libredox::errno::ENODEV))?;
        let route = MsiRoute {
            device: function,
            source: self.source_check(function),
            cpu,
            vector,
        };
        let (words, len) = Request::AllocIrte(route).encode();
        let mut payload = [0_u8; MsiMessage::BYTES];
        let id = libredox::call::call_ro(
            &[cap.raw()][..],
            &mut payload,
            CallFlags::empty(),
            &words[..len],
        )?;
        let message = MsiMessage::from_bytes(&payload)
            .ok_or(libredox::error::Error::new(libredox::errno::EIO))?;
        // The kernel's entry IDs are 32-bit.
        let id =
            u32::try_from(id).map_err(|_| libredox::error::Error::new(libredox::errno::EIO))?;
        Ok((id, message))
    }

    /// Answers `MsiMessage` on channel `id` for function `addr`: a remappable-format message
    /// when the kernel remaps the function's interrupts, otherwise the compatibility-format one.
    pub fn msi_message(
        &mut self,
        id: usize,
        addr: PciAddress,
        cpu: u32,
        vector: u8,
    ) -> PcidClientResponse {
        let no_message = PcidClientResponse::Error(PcidServerResponseError::NoMsiMessage);
        let Some(function) = device(addr) else {
            return no_message;
        };
        match self.alloc_irte(function, cpu, vector) {
            Ok((entry, message)) => {
                self.irtes.entry(id).or_default().push(entry);
                debug!("pcid: {function} vector {vector} on CPU {cpu} through interrupt remapping entry {entry:#x}");
                PcidClientResponse::MsiMessage(MsiAddrAndData {
                    addr: message.address,
                    data: message.data,
                })
            }
            // No remapping unit remaps this function's interrupts, so the compatibility format
            // reaches the CPU as upstream.
            Err(err) if err.errno() == libredox::errno::ENODEV => {
                match self.cpus.destination(cpu) {
                    Ok(apic) => compatibility_message(apic, vector)
                        .map_or(no_message, PcidClientResponse::MsiMessage),
                    Err(err) => {
                        warn!(
                            "pcid: no MSI message for {function} vector {vector} on CPU {cpu}: {}",
                            chain(&err)
                        );
                        no_message
                    }
                }
            }
            Err(err) => {
                warn!("pcid: no interrupt remapping entry for {function} vector {vector} on CPU {cpu}: {err}");
                no_message
            }
        }
    }

    /// Forgets channel `id`, closing a domain the driver never fetched and freeing its
    /// interrupt remapping entries.
    pub fn on_close(&mut self, id: usize) {
        self.pending.remove(&id);
        self.given.remove(&id);
        let Some(entries) = self.irtes.remove(&id) else {
            return;
        };
        let Some(cap) = &self.cap else {
            return;
        };
        for entry in entries {
            if let Err(err) = call(&[cap.raw()], Request::FreeIrte(entry)) {
                warn!("pcid: cannot free interrupt remapping entry {entry:#x}: {err}");
            }
        }
    }
}
