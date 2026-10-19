use std::fs::File;
use std::io::prelude::*;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::path::Path;
use std::ptr::{self, NonNull};
use std::{env, fmt, io, process};

use common::MemoryType;
use daemon::Daemon;
use libredox::call::MmapArgs;
use libredox::flag::{MAP_SHARED, PROT_READ, PROT_WRITE};
pub use pci_types::PciAddress;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use self::bar::PciBar;
pub use self::cap::VendorSpecificCapability;
pub use self::id::FullDeviceId;

mod bar;
pub mod cap;
pub mod config;
mod id;
pub mod irq_helpers;
pub mod msi;
// Ferric Oxide: vectors on a chosen CPU.
pub mod percpu;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct LegacyInterruptLine {
    #[doc(hidden)]
    pub irq: u8,
    pub phandled: Option<(u32, [u32; 3], usize)>,
}

impl LegacyInterruptLine {
    /// Get an IRQ handle for this interrupt line.
    pub fn irq_handle(self, driver: &str) -> File {
        if let Some((phandle, addr, cells)) = self.phandled {
            let path = match cells {
                1 => format!("/scheme/irq/phandle-{}/{}", phandle, addr[0]),
                2 => format!("/scheme/irq/phandle-{}/{},{}", phandle, addr[0], addr[1]),
                3 => format!(
                    "/scheme/irq/phandle-{}/{},{},{}",
                    phandle, addr[0], addr[1], addr[2]
                ),
                _ => panic!(
                    "unexpected number of IRQ description cells for phandle {phandle}: {cells}"
                ),
            };
            File::create(path)
                .unwrap_or_else(|err| panic!("{driver}: failed to open IRQ file: {err}"))
        } else {
            File::open(format!("/scheme/irq/{}", self.irq))
                .unwrap_or_else(|err| panic!("{driver}: failed to open IRQ file: {err}"))
        }
    }
}

impl fmt::Display for LegacyInterruptLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some((phandle, addr, cells)) = self.phandled {
            match cells {
                1 => write!(f, "(phandle {}, {:?})", phandle, addr[0]),
                2 => write!(f, "(phandle {}, {:?},{:?})", phandle, addr[0], addr[1]),
                3 => write!(f, "(phandle {}, {:?})", phandle, addr),
                _ => panic!(
                    "unexpected number of IRQ description cells for phandle {phandle}: {cells}"
                ),
            }
        } else {
            write!(f, "{}", self.irq)
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PciAddress")]
struct PciAddressDef {
    #[serde(getter = "PciAddress::segment")]
    segment: u16,
    #[serde(getter = "PciAddress::bus")]
    bus: u8,
    #[serde(getter = "PciAddress::device")]
    device: u8,
    #[serde(getter = "PciAddress::function")]
    function: u8,
}

impl From<PciAddressDef> for PciAddress {
    fn from(value: PciAddressDef) -> Self {
        PciAddress::new(value.segment, value.bus, value.device, value.function)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PciRom {
    pub addr: u32,
    pub size: u32,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PciFunction {
    /// Address of the PCI function.
    #[serde(with = "PciAddressDef")]
    pub addr: PciAddress,

    /// PCI Base Address Registers
    pub bars: [PciBar; 6],

    /// PCI Option ROM
    pub rom: Option<PciRom>,

    /// Legacy IRQ line: It's the responsibility of pcid to make sure that it be mapped in either
    /// the I/O APIC or the 8259 PIC, so that the subdriver can map the interrupt vector directly.
    /// The vector to map is always this field, plus 32.
    /// If INTx# interrupts aren't supported at all this is `None`.
    pub legacy_interrupt_line: Option<LegacyInterruptLine>,

    /// All identifying information of the PCI function.
    pub full_device_id: FullDeviceId,
}
impl PciFunction {
    pub fn name(&self) -> String {
        // FIXME stop replacing : with - once it is a valid character in scheme names
        format!("pci-{}", self.addr).replace(':', "-")
    }

    pub fn display(&self) -> String {
        let mut string = self.name();
        let mut first = true;
        for (i, bar) in self.bars.iter().enumerate() {
            if !bar.is_none() {
                if first {
                    first = false;
                    string.push_str(" on:");
                }
                string.push_str(&format!(" {i}={}", bar.display()));
            }
        }
        if let Some(irq) = self.legacy_interrupt_line {
            string.push_str(&format!(" IRQ: {irq}"));
        }
        string
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubdriverArguments {
    pub func: PciFunction,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum FeatureStatus {
    Enabled,
    Disabled,
}

impl FeatureStatus {
    pub fn enabled(enabled: bool) -> Self {
        if enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
    pub fn is_enabled(&self) -> bool {
        if let &Self::Enabled = self {
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum PciFeature {
    Msi,
    MsiX,
}
impl PciFeature {
    pub fn is_msi(self) -> bool {
        if let Self::Msi = self {
            true
        } else {
            false
        }
    }
    pub fn is_msix(self) -> bool {
        if let Self::MsiX = self {
            true
        } else {
            false
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub enum PciFeatureInfo {
    Msi(msi::MsiInfo),
    MsiX(msi::MsixInfo),
}

// TODO: Remove these "features" and just go strait to the actual thing.

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MsiSetFeatureInfo {
    /// The Multi Message Enable field of the Message Control in the MSI Capability Structure,
    /// is the log2 of the interrupt vectors, minus one. Can only be 0b000..=0b101.
    pub multi_message_enable: Option<u8>,

    /// The system-specific message address and data.
    ///
    /// The message address contains things like the CPU that will be targeted, at least on
    /// x86_64. The message data contains the actual interrupt vector (lower 8 bits) and
    /// the kind of interrupt, at least on x86_64.
    pub message_address_and_data: Option<msi::MsiAddrAndData>,

    /// A bitmap of the vectors that are masked. This field is not guaranteed (and not likely,
    /// at least according to the feature flags I got from QEMU), to exist.
    pub mask_bits: Option<u32>,
}

/// Some flags that might be set simultaneously, but separately.
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SetFeatureInfo {
    Msi(MsiSetFeatureInfo),

    MsiX {
        /// Masks the entire function, and all of its vectors.
        function_mask: Option<bool>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PcidClientRequest {
    EnableDevice,
    RequestConfig,
    RequestFeatures,
    RequestVendorCapabilities,
    EnableFeature(PciFeature),
    FeatureInfo(PciFeature),
    SetFeatureInfo(SetFeatureInfo),
    ReadConfig(u16),
    WriteConfig(u16, u32),
    /// Ferric Oxide: ask for a DMA domain holding this function (kernel iommu scheme). On
    /// `DmaDomain` the domain's descriptor is then fetched with a descriptor-passing read of
    /// the channel; see [`PciFunctionHandle::get_dma_domain`].
    GetDmaDomain,
    /// Ferric Oxide: the MSI address and data that deliver `vector` to logical CPU `cpu` (the
    /// `irq:cpu-<n>` numbering). With interrupt remapping, pcid has the kernel make an entry
    /// for this function and answers with its remappable-format message; without, it answers
    /// with the compatibility-format message upstream drivers computed themselves. See
    /// [`PciFunctionHandle::msi_message`].
    MsiMessage {
        cpu: u32,
        vector: u8,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PcidServerResponseError {
    NonexistentFeature(PciFeature),
    InvalidBitPattern,
    /// Ferric Oxide: no DMA domain (no kernel iommu scheme, or attaching failed).
    NoDmaDomain,
    /// Ferric Oxide: no MSI message for that CPU and vector (the kernel refused an interrupt
    /// remapping entry, or the CPU needs interrupt remapping and there is none).
    NoMsiMessage,
}

#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PcidClientResponse {
    EnabledDevice,
    Config(SubdriverArguments),
    AllFeatures(Vec<PciFeature>),
    VendorCapabilities(Vec<VendorSpecificCapability>),
    FeatureEnabled(PciFeature),
    FeatureStatus(PciFeature, FeatureStatus),
    Error(PcidServerResponseError),
    FeatureInfo(PciFeature, PciFeatureInfo),
    SetFeatureInfo(PciFeature),
    ReadConfig(u32),
    WriteConfig,
    /// Ferric Oxide: a DMA domain is ready to be fetched from the channel.
    DmaDomain,
    /// Ferric Oxide: the answer to `MsiMessage`.
    MsiMessage(msi::MsiAddrAndData),
}

pub struct MappedBar {
    pub ptr: NonNull<u8>,
    pub bar_size: usize,
    pub memory_type: MemoryType,
}

/// Ferric Oxide: a function's pcid channel, shared so that [`irq_helpers::Msix`] can ask pcid for
/// MSI-X messages after the driver has handed its [`PciFunctionHandle`] elsewhere. Each request
/// and its response happen under the lock.
#[derive(Clone)]
pub struct PcidChannel(std::sync::Arc<std::sync::Mutex<File>>);

impl PcidChannel {
    fn lock(&self) -> std::sync::MutexGuard<'_, File> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Sends `req` and returns pcid's response.
    fn request(&self, req: &PcidClientRequest) -> PcidClientResponse {
        let mut channel = self.lock();
        send(&mut channel, req);
        recv(&mut channel)
    }

    /// The MSI message for `vector` on logical CPU `cpu`; see
    /// [`PciFunctionHandle::msi_message`].
    ///
    /// # Panics
    ///
    /// When `cpu` does not fit in 32 bits.
    pub fn msi_message(&self, cpu: usize, vector: u8) -> Option<msi::MsiAddrAndData> {
        let cpu = u32::try_from(cpu).expect("pcid_interface: CPU number above 32 bits");
        match self.request(&PcidClientRequest::MsiMessage { cpu, vector }) {
            PcidClientResponse::MsiMessage(message) => Some(message),
            PcidClientResponse::Error(PcidServerResponseError::NoMsiMessage) => None,
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }
}

/// A handle from a `pcid` client (e.g. `ahcid`) to `pcid`.
pub struct PciFunctionHandle {
    // Ferric Oxide: shared with MSI-X allocators (was a `File`).
    channel: PcidChannel,
    config: SubdriverArguments,
    mapped_bars: [Option<MappedBar>; 6],
}

fn send<T: Serialize>(w: &mut File, message: &T) {
    let mut data = Vec::new();
    bincode::serialize_into(&mut data, message).expect("couldn't serialize pcid message");
    match w.write(&data) {
        Ok(len) => assert_eq!(len, data.len()),
        Err(err) => {
            log::error!("writing pcid request failed: {err}");
            process::exit(1);
        }
    }
}
fn recv<T: DeserializeOwned>(r: &mut File) -> T {
    let mut length_bytes = [0u8; 8];
    if let Err(err) = r.read_exact(&mut length_bytes) {
        log::error!("reading pcid response length failed: {err}");
        process::exit(1);
    }
    let length = u64::from_le_bytes(length_bytes);
    if length > 0x100_000 {
        panic!("pcid_interface: buffer too large");
    }
    let mut data = vec![0u8; length as usize];
    if let Err(err) = r.read_exact(&mut data) {
        log::error!("reading pcid response failed: {err}");
        process::exit(1);
    }

    bincode::deserialize_from(&data[..]).expect("couldn't deserialize pcid message")
}

impl PciFunctionHandle {
    fn connect_default() -> Self {
        let channel_fd = match env::var("PCID_CLIENT_CHANNEL") {
            Ok(channel_fd) => channel_fd,
            Err(err) => {
                log::error!("PCID_CLIENT_CHANNEL invalid: {err}");
                process::exit(1);
            }
        };
        let channel_fd = match channel_fd.parse::<RawFd>() {
            Ok(channel_fd) => channel_fd,
            Err(err) => {
                log::error!("PCID_CLIENT_CHANNEL invalid: {err}");
                process::exit(1);
            }
        };
        Self::connect_common(channel_fd)
    }

    pub fn connect_by_path(device_path: &Path) -> io::Result<Self> {
        let channel_fd = libredox::call::open(
            device_path.join("channel").to_str().unwrap(),
            libredox::flag::O_RDWR,
            0,
        )?;
        Ok(Self::connect_common(channel_fd as RawFd))
    }

    fn connect_common(channel_fd: i32) -> PciFunctionHandle {
        let mut channel = unsafe { File::from_raw_fd(channel_fd) };

        send(&mut channel, &PcidClientRequest::RequestConfig);
        let config = match recv(&mut channel) {
            PcidClientResponse::Config(a) => a,
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        };

        Self {
            channel: PcidChannel(std::sync::Arc::new(std::sync::Mutex::new(channel))),
            config,
            mapped_bars: [const { None }; 6],
        }
    }

    /// # Panics
    ///
    /// Ferric Oxide: when an [`irq_helpers::Msix`] still shares the channel.
    pub fn into_inner_fd(self) -> RawFd {
        match std::sync::Arc::try_unwrap(self.channel.0) {
            Ok(channel) => channel
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .into_raw_fd(),
            Err(_) => panic!("pcid_interface: the channel is still shared with an MSI-X table"),
        }
    }

    /// Ferric Oxide: sends `req` and returns pcid's response, under the channel's lock.
    fn request(&mut self, req: &PcidClientRequest) -> PcidClientResponse {
        self.channel.request(req)
    }

    /// Ferric Oxide: the channel, for asking pcid for MSI messages later.
    pub fn channel(&self) -> PcidChannel {
        self.channel.clone()
    }

    /// Ferric Oxide: the MSI address and data that deliver `vector` to logical CPU `cpu`, from
    /// pcid. With interrupt remapping (`kernel.iommu = "auto"` or `"enforce"` on hardware that
    /// has it) the message names an interrupt remapping entry the kernel made for this
    /// function only, and the unit blocks compatibility-format messages; otherwise it is the
    /// compatibility-format message for `cpu` as an APIC ID, as upstream. `None` when pcid has
    /// no message to give.
    pub fn msi_message(&mut self, cpu: usize, vector: u8) -> Option<msi::MsiAddrAndData> {
        self.channel.msi_message(cpu, vector)
    }

    pub fn config(&self) -> SubdriverArguments {
        self.config.clone()
    }

    pub fn enable_device(&mut self) {
        match self.request(&PcidClientRequest::EnableDevice) {
            PcidClientResponse::EnabledDevice => {}
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }

    pub fn get_vendor_capabilities(&mut self) -> Vec<VendorSpecificCapability> {
        match self.request(&PcidClientRequest::RequestVendorCapabilities) {
            PcidClientResponse::VendorCapabilities(a) => a,
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }

    // FIXME turn into struct with bool fields
    pub fn fetch_all_features(&mut self) -> Vec<PciFeature> {
        match self.request(&PcidClientRequest::RequestFeatures) {
            PcidClientResponse::AllFeatures(a) => a,
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }
    pub fn enable_feature(&mut self, feature: PciFeature) {
        match self.request(&PcidClientRequest::EnableFeature(feature)) {
            PcidClientResponse::FeatureEnabled(feat) if feat == feature => {}
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }
    pub fn feature_info(&mut self, feature: PciFeature) -> PciFeatureInfo {
        match self.request(&PcidClientRequest::FeatureInfo(feature)) {
            PcidClientResponse::FeatureInfo(feat, info) if feat == feature => info,
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }
    pub fn set_feature_info(&mut self, info: SetFeatureInfo) {
        match self.request(&PcidClientRequest::SetFeatureInfo(info)) {
            PcidClientResponse::SetFeatureInfo(_) => {}
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }
    pub unsafe fn read_config(&mut self, offset: u16) -> u32 {
        match self.request(&PcidClientRequest::ReadConfig(offset)) {
            PcidClientResponse::ReadConfig(value) => value,
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }
    pub unsafe fn write_config(&mut self, offset: u16, value: u32) {
        match self.request(&PcidClientRequest::WriteConfig(offset, value)) {
            PcidClientResponse::WriteConfig => {}
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
    }
    /// Ferric Oxide: asks pcid for a DMA domain that holds this function and the requester
    /// IDs its DMA can carry, and returns the domain's descriptor (close-on-exec), or `None`
    /// when pcid has none to give (the kernel has no iommu scheme, or attaching failed).
    /// Drivers started by pcid-spawner do not need this: [`pci_daemon`] calls
    /// [`PciFunctionHandle::use_dma_domain`], which does it and binds the domain. pcid gives
    /// each channel one domain.
    ///
    /// # Errors
    ///
    /// [`DmaDomainError::Receive`] when pcid created a domain but passing its descriptor
    /// failed.
    pub fn get_dma_domain(&mut self) -> Result<Option<libredox::Fd>, DmaDomainError> {
        match self.request(&PcidClientRequest::GetDmaDomain) {
            PcidClientResponse::DmaDomain => {}
            PcidClientResponse::Error(PcidServerResponseError::NoDmaDomain) => return Ok(None),
            other => {
                log::error!("received wrong pcid response: {other:?}");
                process::exit(1);
            }
        }
        // usize::MAX asks the kernel to pick the descriptor number.
        let mut fd = usize::MAX.to_ne_bytes();
        libredox::call::call_ro(
            self.channel.lock().as_raw_fd() as usize,
            &mut fd,
            syscall::CallFlags::FD,
            &[],
        )
        .map_err(DmaDomainError::Receive)?;
        let domain = libredox::Fd::new(usize::from_ne_bytes(fd));
        // Children the driver spawns (xhcid's class drivers, say) must not keep its domain
        // open, and with it the device's mappings, after the driver exits.
        let cloexec = libredox::flag::O_CLOEXEC as usize;
        if let Err(err) = domain.fcntl(libredox::flag::F_SETFD as usize, cloexec) {
            log::warn!(
                "pcid_interface: cannot make the DMA domain descriptor close-on-exec: {err}"
            );
        }
        Ok(Some(domain))
    }

    /// Ferric Oxide: makes the calling process do its DMA for this function through a DMA
    /// domain (kernel iommu scheme), before it translates any DMA address. The first function a
    /// process uses gets its domain from pcid and binds it, so from then on
    /// `memory:translation` (and so [`common::dma::Dma`]) answers with IOVAs in it. A later
    /// function's domain is joined into that one, so a driver of several functions has one
    /// domain and one IOVA space. When pcid has no domain to give (no IOMMU, or a kernel built
    /// with `kernel.iommu = "never"`), DMA addresses stay physical, as upstream.
    ///
    /// [`pci_daemon`] calls this for the function it connects to; a driver that opens more
    /// functions with [`PciFunctionHandle::connect_by_path`] calls it for each of them.
    ///
    /// A forked child does not inherit the binding: it has an address space of its own, so
    /// under `kernel.iommu = "enforce"` its translations fail with `EACCES`, and under
    /// `"auto"` it gets physical addresses, which its parent's device cannot reach.
    ///
    /// # Errors
    ///
    /// [`DmaDomainError::Receive`], [`DmaDomainError::Bind`] or [`DmaDomainError::Join`] when
    /// that step fails. The function's device then cannot DMA: pcid has already moved it out
    /// of the identity mapping.
    pub fn use_dma_domain(&mut self) -> Result<DmaDomainUse, DmaDomainError> {
        let Some(domain) = self.get_dma_domain()? else {
            return Ok(DmaDomainUse::Physical);
        };
        let mut bound = PROCESS_DMA_DOMAIN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if bound.is_some() {
            call_iommu(&domain, ferric_iommu::abi::Request::Join).map_err(DmaDomainError::Join)?;
            // The domain is empty now; dropping it closes it.
            return Ok(DmaDomainUse::Joined);
        }
        bind_dma_domain(&domain).map_err(DmaDomainError::Bind)?;
        *bound = Some(domain);
        Ok(DmaDomainUse::Bound)
    }

    pub unsafe fn map_bar(&mut self, bir: u8, memory_type: MemoryType) -> &MappedBar {
        let mapped_bar = &mut self.mapped_bars[bir as usize];
        if let Some(mapped_bar) = mapped_bar {
            assert_eq!(mapped_bar.memory_type, memory_type);
            mapped_bar
        } else {
            let (bar, bar_size) = self.config.func.bars[bir as usize].expect_mem();

            let ptr = match unsafe {
                libredox::call::mmap(MmapArgs {
                    addr: ptr::null_mut(),
                    length: bar_size,
                    prot: PROT_READ | PROT_WRITE,
                    flags: MAP_SHARED,
                    fd: self.channel.lock().as_raw_fd() as usize,
                    offset: u64::from(bir) << (64 - 3) | (memory_type as u64) << (64 - 3 - 2) | 0,
                })
            } {
                Ok(ptr) => ptr,
                Err(err) => {
                    log::error!("failed to map BAR at {bar:016X}: {err}");
                    process::exit(1);
                }
            };

            mapped_bar.insert(MappedBar {
                ptr: NonNull::new(ptr.cast::<u8>()).expect("Mapping a BAR resulted in a nullptr"),
                bar_size,
                memory_type,
            })
        }
    }
}

/// Ferric Oxide: the DMA domain this process has bound, kept open for as long as the process
/// lives: closing it would block the devices in it.
static PROCESS_DMA_DOMAIN: std::sync::Mutex<Option<libredox::Fd>> = std::sync::Mutex::new(None);

/// Ferric Oxide: what [`PciFunctionHandle::use_dma_domain`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DmaDomainUse {
    /// pcid had no domain to give, so DMA addresses are physical, as upstream.
    Physical,
    /// The function's domain is now the process's domain.
    Bound,
    /// The process had a domain already; the function was moved into it.
    Joined,
}

/// Ferric Oxide: why [`PciFunctionHandle::use_dma_domain`] could not put a function in the
/// process's DMA domain.
#[derive(Debug)]
#[non_exhaustive]
pub enum DmaDomainError {
    /// pcid created a domain, but receiving its descriptor over the channel failed.
    Receive(libredox::error::Error),
    /// The kernel refused to bind the domain.
    Bind(libredox::error::Error),
    /// The kernel refused to move the function into the process's domain.
    Join(libredox::error::Error),
}

impl fmt::Display for DmaDomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Receive(_) => "cannot receive the DMA domain from pcid",
            Self::Bind(_) => "cannot bind the DMA domain",
            Self::Join(_) => "cannot move the function into the process's DMA domain",
        })
    }
}

impl std::error::Error for DmaDomainError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Receive(err) | Self::Bind(err) | Self::Join(err) => Some(err),
        }
    }
}

/// Makes a call to the kernel's iommu scheme on `domain`.
fn call_iommu(
    domain: &libredox::Fd,
    request: ferric_iommu::abi::Request,
) -> libredox::error::Result<usize> {
    let (words, len) = request.encode();
    domain.call_ro(&mut [], syscall::CallFlags::empty(), &words[..len])
}

/// Ferric Oxide: makes `domain` (from [`PciFunctionHandle::get_dma_domain`]) the calling
/// process's DMA domain. A process binds at most one domain. Drivers normally leave this to
/// [`PciFunctionHandle::use_dma_domain`], which also keeps the domain open.
///
/// # Errors
///
/// The kernel's errno: `EBUSY` when the process or another one has bound a domain already,
/// `EINVAL` under `kernel.iommu = "enforce"` when no device is attached to the domain.
pub fn bind_dma_domain(domain: &libredox::Fd) -> libredox::error::Result<()> {
    call_iommu(domain, ferric_iommu::abi::Request::Bind)?;
    Ok(())
}

/// Ferric Oxide: maps the registered-buffer grant behind `grant` (a `kernel.regbuf` grant
/// descriptor an application passed to the driver) into the process's bound DMA domain
/// (`MapGrant`), and returns the IOVA of its first byte. The device then reaches the grant's
/// pages at one contiguous IOVA range. Give it back with [`unmap_dma_grant`], also after the
/// application revoked the grant: until then the IOVA stays reserved.
///
/// # Errors
///
/// [`DmaGrantError::NoDomain`] when the process has bound no domain (no IOMMU, or a kernel
/// built with `kernel.iommu = "never"`), and [`DmaGrantError::Kernel`] with the kernel's errno
/// otherwise (`ferric_iommu::abi` lists them).
pub fn map_dma_grant(grant: &libredox::Fd) -> Result<u64, DmaGrantError> {
    // Descriptor numbers and IOVAs are 64-bit on every target with an IOMMU scheme.
    let grant_fd = grant.raw() as u64;
    let iova = with_process_dma_domain(|domain| {
        call_iommu(domain, ferric_iommu::abi::Request::MapGrant { grant_fd })
    })?;
    Ok(iova as u64)
}

/// Ferric Oxide: takes the grant mapping at `iova` (from [`map_dma_grant`]) out of the
/// process's DMA domain, strictly: when this returns the device can no longer reach it. After
/// the application revoked the grant, this gives the reserved IOVA back.
///
/// # Errors
///
/// [`DmaGrantError::NoDomain`] when the process has bound no domain, and
/// [`DmaGrantError::Kernel`] with the kernel's errno otherwise.
pub fn unmap_dma_grant(iova: u64) -> Result<(), DmaGrantError> {
    with_process_dma_domain(|domain| {
        call_iommu(domain, ferric_iommu::abi::Request::UnmapGrant { iova })
    })?;
    Ok(())
}

/// Runs `call` on the process's bound DMA domain.
fn with_process_dma_domain(
    call: impl FnOnce(&libredox::Fd) -> libredox::error::Result<usize>,
) -> Result<usize, DmaGrantError> {
    let bound = PROCESS_DMA_DOMAIN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let domain = bound.as_ref().ok_or(DmaGrantError::NoDomain)?;
    call(domain).map_err(DmaGrantError::Kernel)
}

/// Ferric Oxide: why [`map_dma_grant`] or [`unmap_dma_grant`] failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum DmaGrantError {
    /// The process has bound no DMA domain.
    NoDomain,
    /// The kernel refused the call.
    Kernel(libredox::error::Error),
}

impl fmt::Display for DmaGrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoDomain => "the process has bound no DMA domain",
            Self::Kernel(_) => "the kernel refused the grant mapping call",
        })
    }
}

impl std::error::Error for DmaGrantError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoDomain => None,
            Self::Kernel(err) => Some(err),
        }
    }
}

/// Ferric Oxide: [`PciFunctionHandle::use_dma_domain`], reporting a failure. A failure is not
/// fatal here: the driver's first DMA allocation then fails with the kernel's reason. The
/// kernel logs every bind and join, so success needs no message.
fn use_dma_domain_logged(handle: &mut PciFunctionHandle) {
    let addr = handle.config.func.addr;
    let Err(err) = handle.use_dma_domain() else {
        return;
    };
    let source = std::error::Error::source(&err).map(ToString::to_string);
    let source = source.unwrap_or_default();
    if log::max_level() == log::LevelFilter::Off {
        // Drivers set up their logger only after connecting, so nothing would record it.
        eprintln!("pcid_interface: {addr}: {err}: {source}");
    } else {
        log::error!("pcid_interface: {addr}: {err}: {source}");
    }
}

pub fn pci_daemon<F: FnOnce(Daemon, PciFunctionHandle) -> !>(f: F) -> ! {
    Daemon::new(|daemon| {
        common::init();
        let mut pcid_handle = PciFunctionHandle::connect_default();
        // Ferric Oxide: before the driver translates any DMA address.
        use_dma_domain_logged(&mut pcid_handle);
        f(daemon, pcid_handle)
    })
}
