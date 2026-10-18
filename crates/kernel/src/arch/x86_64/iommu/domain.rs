//! Ferric Oxide: DMA domains (io/iommu-design.md, step 2).
//!
//! A [`Domain`] is a set of I/O page tables with its own domain ID and IOVA space, for
//! whichever IOMMU the machine has: VT-d (second-stage tables, context entries) or AMD-Vi
//! (host page tables, device table entries, `amdvi.rs`). The tables are built by the same
//! walker, `ferric_iommu::table`, with each vendor's entry encoding; callers do not see which
//! one is in use. Devices are moved into a domain with [`Domain::attach`] and out of it with
//! [`Domain::detach`], which leaves them *blocked*: every DMA they issue faults (on VT-d their
//! context entry is not present; on AMD-Vi their device table entry translates through an
//! empty table with reads and writes refused). Devices that were never attached stay in the
//! identity domain.
//!
//! On VT-d, every context-entry change follows the VT-d specification's invalidation rules (section
//! 6.5.3, "Guidance to Software for Invalidations"): a present entry is first made not
//! present, then the context cache is invalidated for that device (device-selective, with
//! the old domain ID) and the IOTLB for the old domain (domain-selective). Only then is the
//! new entry written, high word first so the unit never sees a present entry that mixes two
//! domains. Unmapping is strict: the entries are cleared, the IOTLB is invalidated
//! (page-selective where possible) and waited on, and only then are emptied table pages
//! freed.
//!
//! AMD-Vi device table entries change in the same order (`amdvi::attach`, `amdvi::detach`).
//!
//! A device that an RMRR (VT-d) or a unity or exclusion IVMD (AMD-Vi) names gets that
//! reserved memory mapped one-to-one in the domain before it is attached, so the firmware's
//! DMA to it keeps working. Device scopes that name
//! bridges (in DRHDs and RMRRs) are resolved with the bus ranges pcid reports through
//! [`report_bridge`].
//!
//! User space creates domains and attaches devices through the `iommu` scheme
//! (`scheme/iommu.rs`).

use alloc::{collections::BTreeMap, vec::Vec};
use core::{fmt, ops::Range};

use ferric_vtd::{
    dmar::{BusRange, Structure},
    invalidation::{iotlb_flush_for, Request},
    iova::{IovaAllocator, IovaError, IovaRange},
    plan::{select_unit, unit_coverage_with, SelectError, IDENTITY_DOMAIN},
    table::{
        self, Access, ContextEntry, MapError, RangeError, RootEntry, TableMemory, TranslationType,
        UnmapError, ENTRIES,
    },
    Bdf,
};

use spin::Mutex;

use super::{
    amdvi, Error, KernelTables, NoMemory, Unit, UnitState, DMAR, DOMAIN_IDS, LAYOUT, UNITS,
};

/// The bus ranges of the PCI-to-PCI bridges pcid has reported, by segment and requester ID.
static BRIDGES: Mutex<BTreeMap<(u16, u16), BusRange>> = Mutex::new(BTreeMap::new());

/// Why a domain operation failed.
#[derive(Debug)]
pub enum DomainError {
    /// No IOMMU is enabled, or the units share no table layout.
    NotEnabled,
    /// Every domain ID is in use.
    NoDomainIds,
    /// No memory for a table page.
    NoMemory,
    /// An IOVA allocation failed.
    Iova(IovaError),
    /// Mapping failed part-way; the part that was mapped has been unmapped again.
    Map(RangeError<MapError<NoMemory>>),
    /// Unmapping failed part-way.
    Unmap(RangeError<UnmapError>),
    /// Changing the access of a mapped range failed part-way.
    Protect(RangeError<UnmapError>),
    /// No unit, or no single unit, serves the device.
    Select(SelectError),
    /// The IOMMU that serves the device was skipped at boot.
    UnitNotEnabled {
        /// The unit's register base.
        base: u64,
    },
    /// The device belongs to another domain.
    Busy {
        /// That domain's ID.
        did: u16,
    },
    /// The device is already attached to this domain.
    AlreadyAttached,
    /// The device is not attached to this domain.
    NotAttached,
    /// The unit did not complete an invalidation.
    Hardware(Error),
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotEnabled => f.write_str("no usable IOMMU"),
            Self::NoDomainIds => f.write_str("out of domain IDs"),
            Self::NoMemory => f.write_str("out of memory for translation tables"),
            Self::Iova(err) => write!(f, "{err}"),
            Self::Map(err) => write!(f, "map: {err}"),
            Self::Unmap(err) => write!(f, "unmap: {err}"),
            Self::Protect(err) => write!(f, "protect: {err}"),
            Self::Select(err) => write!(f, "{err}"),
            Self::UnitNotEnabled { base } => write!(f, "IOMMU unit {base:#x} is not enabled"),
            Self::Busy { did } => write!(f, "the device is in domain {did}"),
            Self::AlreadyAttached => f.write_str("the device is already attached"),
            Self::NotAttached => f.write_str("the device is not attached"),
            Self::Hardware(err) => write!(f, "{err}"),
        }
    }
}

impl From<Error> for DomainError {
    fn from(err: Error) -> Self {
        Self::Hardware(err)
    }
}

/// A device's PCI address: segment and requester ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    /// The PCI segment.
    pub segment: u16,
    /// Bus, device and function.
    pub bdf: Bdf,
}

/// Which IOMMU a domain's tables are for, and their layout.
#[derive(Clone, Copy, Debug)]
enum Vendor {
    /// VT-d second-stage tables.
    Vtd(ferric_vtd::plan::DomainLayout),
    /// AMD-Vi host page tables.
    AmdVi(ferric_amdvi::plan::DomainLayout),
}

impl Vendor {
    /// The vendor of the enabled IOMMUs, when DMA domains are available.
    fn current() -> Option<Self> {
        LAYOUT
            .get()
            .map(|&layout| Self::Vtd(layout))
            .or_else(|| amdvi::layout().map(Self::AmdVi))
    }

    /// One past the largest IOVA.
    fn iova_limit(self) -> u64 {
        match self {
            Self::Vtd(layout) => layout.iova_limit,
            Self::AmdVi(layout) => layout.iova_limit,
        }
    }
}

/// How a device is attached, read back from its context or device table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Attachment {
    /// In the identity domain: DMA is not translated.
    Identity,
    /// Blocked: every DMA faults.
    Blocked,
    /// Translated through the tables at `root`, tagged with domain ID `id`.
    Domain {
        /// The domain ID.
        id: u16,
        /// The root table.
        root: u64,
    },
    /// Something this kernel does not write.
    Other,
}

/// A DMA domain: I/O page tables, a domain ID and an IOVA allocator.
pub struct Domain {
    did: u16,
    vendor: Vendor,
    root: u64,
    tables: KernelTables,
    iova: IovaAllocator,
    /// The attached devices and the index of the unit that serves each, in `UNITS` or in
    /// `amdvi`'s units.
    attached: Vec<(Device, usize)>,
    /// The RMRRs or IVMDs mapped one-to-one for attached devices.
    reserved: Vec<Range<u64>>,
    /// Set by [`Domain::close`]: whether the domain was torn down cleanly.
    closed: Option<bool>,
}

impl Domain {
    /// Creates an empty domain: until something is mapped, every DMA from its devices faults.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotEnabled`] when no IOMMU is on, [`DomainError::NoDomainIds`] and
    /// [`DomainError::NoMemory`] when out of resources.
    pub fn new() -> Result<Self, DomainError> {
        let vendor = Vendor::current().ok_or(DomainError::NotEnabled)?;
        let did = DOMAIN_IDS
            .lock()
            .as_mut()
            .and_then(|ids| ids.alloc())
            .ok_or(DomainError::NoDomainIds)?;
        let mut tables = KernelTables::default();
        let root = match tables.alloc_table() {
            Ok(root) => root,
            Err(NoMemory) => {
                release_did(did);
                return Err(DomainError::NoMemory);
            }
        };
        // RMRRs and IVMDs are left out of every domain's IOVA space, so they can later be
        // mapped one-to-one for the devices that use them.
        let reserved = match vendor {
            Vendor::Vtd(_) => DMAR
                .get()
                .into_iter()
                .flat_map(super::rmrrs)
                .filter_map(|rmrr| Some(rmrr.base..rmrr.end()?))
                .collect::<Vec<_>>(),
            Vendor::AmdVi(_) => amdvi::reserved_iova(),
        };
        Ok(Self {
            did,
            vendor,
            root,
            tables,
            iova: IovaAllocator::new(vendor.iova_limit(), reserved),
            attached: Vec::new(),
            reserved: Vec::new(),
            closed: None,
        })
    }

    /// The domain ID.
    pub fn id(&self) -> u16 {
        self.did
    }

    /// The physical address of the root table.
    pub fn root_table(&self) -> u64 {
        self.root
    }

    /// How many devices are attached.
    pub fn device_count(&self) -> usize {
        self.attached.len()
    }

    /// How many table pages the domain holds, the root included.
    pub fn table_pages(&self) -> usize {
        self.tables.pages.len()
    }

    /// Allocates a naturally aligned IOVA range of at least `len` bytes.
    ///
    /// # Errors
    ///
    /// [`DomainError::Iova`] when `len` is invalid or the space is full.
    pub fn alloc_iova(&mut self, len: u64) -> Result<IovaRange, DomainError> {
        self.iova.alloc(len).map_err(DomainError::Iova)
    }

    /// Gives back a range from [`Domain::alloc_iova`]. Unmap it first.
    ///
    /// # Errors
    ///
    /// [`DomainError::Iova`] when the range was not allocated.
    pub fn free_iova(&mut self, range: IovaRange) -> Result<(), DomainError> {
        self.iova.free(range).map_err(DomainError::Iova)
    }

    /// Maps `len` bytes at `iova` to `phys` with `access`, using large pages where the
    /// addresses allow and every unit supports them.
    ///
    /// # Errors
    ///
    /// [`DomainError::Map`] when an address is misaligned, out of range or already mapped, or
    /// a table cannot be allocated; whatever was mapped is unmapped again.
    pub fn map(
        &mut self,
        iova: u64,
        phys: u64,
        len: u64,
        access: Access,
    ) -> Result<(), DomainError> {
        let mapped = match self.vendor {
            Vendor::Vtd(layout) => table::map_range(
                &mut self.tables,
                self.root,
                layout.agaw,
                iova,
                phys,
                len,
                access,
                layout.large_pages,
            ),
            Vendor::AmdVi(layout) => ferric_amdvi::table::map_range(
                &mut self.tables,
                self.root,
                layout.levels,
                iova,
                phys,
                len,
                access,
                layout.large_pages,
            ),
        };
        if let Err(err) = mapped {
            if err.done > 0 {
                self.unmap(iova, err.done)?;
            }
            return Err(DomainError::Map(err));
        }
        self.flush_tables();
        // VT-d units in caching mode may cache not-present entries, so making entries present
        // needs an invalidation there (CAP.CM, section 6.1). AMD-Vi gives no such guarantee
        // either way, so its pages are always invalidated.
        self.flush_iotlb(iova, len, false, |unit| unit.regs.cap.caching_mode())
    }

    /// Unmaps `len` bytes at `iova`, invalidates the IOTLB of every unit the domain is
    /// attached to, waits for that, and then frees the table pages left empty.
    ///
    /// # Errors
    ///
    /// [`DomainError::Unmap`] when part of the range is not mapped or the range ends inside
    /// a large page (the part before it is still unmapped and invalidated), and
    /// [`DomainError::Hardware`] when an invalidation fails; the emptied pages are then kept,
    /// since a unit may still use them.
    pub fn unmap(&mut self, iova: u64, len: u64) -> Result<(), DomainError> {
        let mut emptied = Vec::new();
        let collect = |page| emptied.push(page);
        let result = match self.vendor {
            Vendor::Vtd(layout) => {
                table::unmap_range(&mut self.tables, self.root, layout.agaw, iova, len, collect)
            }
            Vendor::AmdVi(layout) => ferric_amdvi::table::unmap_range(
                &mut self.tables,
                self.root,
                layout.levels,
                iova,
                len,
                collect,
            ),
        };
        let done = match &result {
            Ok(()) => len,
            Err(err) => err.done,
        };
        self.flush_tables();
        if done > 0 {
            self.flush_iotlb(iova, done, !emptied.is_empty(), |_| true)?;
        }
        for page in emptied {
            self.tables.free(page);
        }
        result.map_err(DomainError::Unmap)
    }

    /// Changes the device's access to the `len` bytes mapped at `iova` to `access`, then
    /// invalidates the IOTLB of every unit the domain is attached to and waits for that, so
    /// when it returns no device can use the old access any more.
    ///
    /// # Errors
    ///
    /// [`DomainError::Protect`] when part of the range is not mapped or it starts or ends
    /// inside a large page (the part before that is changed and invalidated), and
    /// [`DomainError::Hardware`] when an invalidation fails.
    pub fn protect(&mut self, iova: u64, len: u64, access: Access) -> Result<(), DomainError> {
        let result = match self.vendor {
            Vendor::Vtd(layout) => {
                table::protect_range(&mut self.tables, self.root, layout.agaw, iova, len, access)
            }
            Vendor::AmdVi(layout) => ferric_amdvi::table::protect_range(
                &mut self.tables,
                self.root,
                layout.levels,
                iova,
                len,
                access,
            ),
        };
        let done = match &result {
            Ok(()) => len,
            Err(err) => err.done,
        };
        self.flush_tables();
        if done > 0 {
            self.flush_iotlb(iova, done, false, |_| true)?;
        }
        result.map_err(DomainError::Protect)
    }

    /// The attached devices.
    pub fn devices(&self) -> impl Iterator<Item = Device> + '_ {
        self.attached.iter().map(|&(device, _)| device)
    }

    /// The physical address and access a device in this domain gets for `iova`, read back
    /// from the tables.
    pub fn translate(&mut self, iova: u64) -> Option<(u64, Access)> {
        match self.vendor {
            Vendor::Vtd(layout) => table::translate(&mut self.tables, self.root, layout.agaw, iova),
            Vendor::AmdVi(layout) => {
                ferric_amdvi::table::translate(&mut self.tables, self.root, layout.levels, iova)
            }
        }
    }

    /// Moves `device` from the identity domain (or the blocked state) into this domain,
    /// after mapping the reserved memory of every RMRR or IVMD that names it one-to-one.
    ///
    /// # Errors
    ///
    /// [`DomainError::Select`] or [`DomainError::UnitNotEnabled`] when no enabled unit
    /// serves the device, [`DomainError::Map`] when reserved memory cannot be mapped,
    /// [`DomainError::Busy`] when it is in another domain, [`DomainError::AlreadyAttached`],
    /// and [`DomainError::Hardware`] when an invalidation fails. On error the reserved memory
    /// mapped for this call is unmapped again.
    pub fn attach(&mut self, device: Device) -> Result<(), DomainError> {
        if self.attached.iter().any(|&(d, _)| d == device) {
            return Err(DomainError::AlreadyAttached);
        }
        let index = unit_for(device)?;
        let mapped = self.map_reserved(device)?;
        let result = match self.vendor {
            Vendor::Vtd(layout) => {
                let entry = ContextEntry::new(
                    self.root,
                    layout.agaw,
                    self.did,
                    TranslationType::Untranslated,
                );
                set_context(index, device.bdf, Target::Entry(entry), |old| {
                    match old.is_present().then(|| old.domain_id()) {
                        Some(did) if did != IDENTITY_DOMAIN => Err(DomainError::Busy { did }),
                        _ => Ok(()),
                    }
                })
                .map(drop)
            }
            Vendor::AmdVi(_) => amdvi::attach(index, device, self.root, self.did),
        };
        if let Err(err) = result {
            self.unmap_reserved(mapped);
            return Err(err);
        }
        self.attached.push((device, index));
        Ok(())
    }

    /// Maps the reserved memory that names `device` and is not mapped yet one-to-one, and
    /// returns it.
    fn map_reserved(&mut self, device: Device) -> Result<Vec<Range<u64>>, DomainError> {
        let mut mapped = Vec::new();
        for (range, access) in reserved_ranges(device) {
            if self.reserved.contains(&range) {
                continue;
            }
            let len = range.end.saturating_sub(range.start);
            if let Err(err) = self.map(range.start, range.start, len, access) {
                self.unmap_reserved(mapped);
                return Err(err);
            }
            self.reserved.push(range.clone());
            mapped.push(range);
        }
        Ok(mapped)
    }

    /// Unmaps reserved memory that [`Domain::map_reserved`] mapped, after a failed attach.
    fn unmap_reserved(&mut self, ranges: Vec<Range<u64>>) {
        for range in ranges {
            self.reserved.retain(|r| *r != range);
            let len = range.end.saturating_sub(range.start);
            if let Err(err) = self.unmap(range.start, len) {
                warn!(
                    "IOMMU: cannot unmap reserved memory {:#x}-{:#x} from domain {}: {err}",
                    range.start, range.end, self.did
                );
            }
        }
    }

    /// Takes `device` out of this domain and blocks it: its DMA faults until it is attached
    /// again.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotAttached`], and [`DomainError::Hardware`] when an invalidation
    /// fails (the device then stays attached).
    pub fn detach(&mut self, device: Device) -> Result<(), DomainError> {
        let position = self
            .attached
            .iter()
            .position(|&(d, _)| d == device)
            .ok_or(DomainError::NotAttached)?;
        let index = self
            .attached
            .get(position)
            .map(|&(_, index)| index)
            .ok_or(DomainError::NotAttached)?;
        block(self.vendor, index, device)?;
        self.attached.swap_remove(position);
        Ok(())
    }

    /// Writes the domain's changed tables out of the CPU caches, for VT-d units that need it.
    /// AMD-Vi page-table reads are coherent.
    fn flush_tables(&self) {
        if matches!(self.vendor, Vendor::Vtd(layout) if layout.flush_cpu_caches) {
            self.tables.flush_cpu_caches();
        }
    }

    /// Invalidates the IOTLB for `[iova, iova + len)` on each attached unit; on VT-d only on
    /// those `filter` accepts.
    fn flush_iotlb(
        &self,
        iova: u64,
        len: u64,
        freed_tables: bool,
        filter: impl Fn(&Unit) -> bool,
    ) -> Result<(), DomainError> {
        let mut done: Vec<usize> = Vec::new();
        if let Vendor::AmdVi(_) = self.vendor {
            for &(_, index) in &self.attached {
                if !done.contains(&index) {
                    amdvi::flush_pages(index, self.did, iova, len)?;
                    done.push(index);
                }
            }
            return Ok(());
        }
        let units = units();
        for &(_, index) in &self.attached {
            let Some(unit) = units.get(index).filter(|u| filter(u)) else {
                continue;
            };
            if done.contains(&index) {
                continue;
            }
            let request = iotlb_flush_for(unit.regs.cap, self.did, iova, len, freed_tables);
            let mut state = unit.state.lock();
            unit.invalidate(&mut state, &[request])?;
            done.push(index);
        }
        Ok(())
    }
}

impl Domain {
    /// Blocks every attached device (with the context-cache and IOTLB invalidations that
    /// takes, waited for), then frees the tables and the domain ID. Returns `false` when a
    /// device could not be blocked: a unit may then still translate through the domain, so its
    /// tables are leaked, and so must be any memory mapped in it. Later calls do nothing and
    /// return the first call's answer.
    pub fn close(&mut self) -> bool {
        if let Some(clean) = self.closed {
            return clean;
        }
        let mut clean = true;
        for (device, index) in core::mem::take(&mut self.attached) {
            if let Err(err) = block(self.vendor, index, device) {
                error!(
                    "IOMMU: cannot detach {:04x}:{} from domain {}: {err}; leaking its tables",
                    device.segment, device.bdf, self.did
                );
                clean = false;
                break;
            }
        }
        if clean {
            self.tables.free_all();
            release_did(self.did);
        }
        self.closed = Some(clean);
        clean
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        self.close();
    }
}

/// Gives a domain ID back once nothing can hold cached entries for it.
fn release_did(did: u16) {
    if let Some(ids) = DOMAIN_IDS.lock().as_mut() {
        ids.free(did);
    }
}

/// Blocks `device` on unit `index` of `vendor` (on AMD-Vi, an alias entry another attached
/// function still uses stays in its domain).
fn block(vendor: Vendor, index: usize, device: Device) -> Result<(), DomainError> {
    match vendor {
        Vendor::Vtd(_) => set_context(index, device.bdf, Target::Blocked, |_| Ok(())).map(drop),
        Vendor::AmdVi(_) => amdvi::detach(index, device),
    }
}

/// The enabled VT-d units.
fn units() -> &'static [Unit] {
    UNITS.get().map_or(&[], Vec::as_slice)
}

/// The index of the enabled unit that serves `device`, in `UNITS` or in `amdvi`'s units.
fn unit_for(device: Device) -> Result<usize, DomainError> {
    match Vendor::current() {
        Some(Vendor::AmdVi(_)) => amdvi::unit_for(device),
        _ => vtd_unit_for(device),
    }
}

/// The index in `UNITS` of the enabled VT-d unit that serves `device`.
fn vtd_unit_for(device: Device) -> Result<usize, DomainError> {
    let dmar = DMAR.get().ok_or(DomainError::NotEnabled)?;
    let drhds = dmar
        .structures()
        .filter_map(|s| match s {
            Ok(Structure::Drhd(drhd)) => Some(drhd),
            _ => None,
        })
        .collect::<Vec<_>>();
    let chosen = select_unit(drhds.iter().map(|d| {
        unit_coverage_with(d, device.segment, device.bdf, |bridge| {
            bridge_buses(device.segment, bridge)
        })
    }))
    .map_err(DomainError::Select)?;
    let base = drhds
        .get(chosen)
        .map(|d| d.register_base)
        .ok_or(DomainError::Select(SelectError::NotCovered))?;
    units()
        .iter()
        .position(|u| u.regs.base == base)
        .ok_or(DomainError::UnitNotEnabled { base })
}

/// Records the bus range behind the bridge at `bridge` in `segment`, as pcid read it from the
/// bridge's configuration space, so that device scopes naming the bridge can be resolved.
pub fn report_bridge(segment: u16, bridge: Bdf, buses: BusRange) {
    BRIDGES.lock().insert((segment, bridge.source_id()), buses);
}

/// The bus range behind `bridge` in `segment`, if pcid reported it.
fn bridge_buses(segment: u16, bridge: Bdf) -> Option<BusRange> {
    BRIDGES.lock().get(&(segment, bridge.source_id())).copied()
}

/// The reserved memory `device` must reach one-to-one, and how: every RMRR (VT-d) or unity or
/// exclusion IVMD (AMD-Vi) that names it.
fn reserved_ranges(device: Device) -> Vec<(Range<u64>, Access)> {
    match Vendor::current() {
        Some(Vendor::AmdVi(_)) => amdvi::reserved_ranges(device),
        _ => rmrr_ranges(device)
            .into_iter()
            .map(|range| (range, Access::ReadWrite))
            .collect(),
    }
}

/// The memory of every RMRR that names `device`. Scopes that pass through bridges pcid has
/// not reported do not match.
fn rmrr_ranges(device: Device) -> Vec<Range<u64>> {
    let Some(dmar) = DMAR.get() else {
        return Vec::new();
    };
    super::rmrrs(dmar)
        .filter(|rmrr| rmrr.segment == device.segment)
        .filter(|rmrr| {
            rmrr.scopes.iter().flatten().any(|scope| {
                scope.covers(device.bdf, |bridge| bridge_buses(device.segment, bridge))
                    == Some(true)
            })
        })
        .filter_map(|rmrr| Some(rmrr.base..rmrr.end()?))
        .collect()
}

/// Whether reserved memory (an RMRR or an IVMD) names `device`.
pub(super) fn has_reserved_memory(device: Device) -> bool {
    !reserved_ranges(device).is_empty()
}

/// What a device's context entry becomes.
#[derive(Clone, Copy)]
enum Target {
    /// This entry (present).
    Entry(ContextEntry),
    /// The unit's identity-domain entry.
    Identity,
    /// Not present.
    Blocked,
}

/// Changes `bdf`'s context entry on unit `index` to `target`, after `check` accepts the
/// current entry, with the invalidations the change needs. Returns the old entry.
fn set_context(
    index: usize,
    bdf: Bdf,
    target: Target,
    check: impl FnOnce(ContextEntry) -> Result<(), DomainError>,
) -> Result<ContextEntry, DomainError> {
    let unit = units().get(index).ok_or(DomainError::NotEnabled)?;
    let mut state = unit.state.lock();
    let new = match target {
        Target::Entry(entry) => entry,
        Target::Identity => state.identity,
        Target::Blocked => ContextEntry::BLOCKED,
    };
    let table = context_table(unit, &mut state, bdf.bus)?;
    let old = read_entry(&mut state.tables, table, bdf);
    check(old)?;
    let sid = bdf.source_id();
    if old.is_present() {
        write_entry(unit, &mut state.tables, table, bdf, ContextEntry::BLOCKED);
        unit.invalidate(
            &mut state,
            &[
                Request::ContextDevice {
                    did: old.domain_id(),
                    source_id: sid,
                    function_mask: 0,
                },
                Request::IotlbDomain {
                    did: old.domain_id(),
                },
            ],
        )?;
    }
    if new.is_present() {
        write_entry(unit, &mut state.tables, table, bdf, new);
        // Needed after making an entry present only in caching mode, but cheap.
        unit.invalidate(
            &mut state,
            &[Request::ContextDevice {
                did: new.domain_id(),
                source_id: sid,
                function_mask: 0,
            }],
        )?;
    }
    Ok(old)
}

/// How `device` is attached, read back from its context or device table entry.
pub(super) fn attachment(device: Device) -> Result<Attachment, DomainError> {
    let index = unit_for(device)?;
    if let Some(Vendor::AmdVi(_)) = Vendor::current() {
        return amdvi::attachment(index, device);
    }
    let entry = context_entry(index, device.bdf)?;
    Ok(if !entry.is_present() {
        Attachment::Blocked
    } else if entry.domain_id() == IDENTITY_DOMAIN {
        Attachment::Identity
    } else if entry.translation_type() == Some(TranslationType::Untranslated) {
        Attachment::Domain {
            id: entry.domain_id(),
            root: entry.table(),
        }
    } else {
        Attachment::Other
    })
}

/// `bdf`'s context entry on VT-d unit `index`, read back from the tables.
fn context_entry(index: usize, bdf: Bdf) -> Result<ContextEntry, DomainError> {
    let unit = units().get(index).ok_or(DomainError::NotEnabled)?;
    let mut state = unit.state.lock();
    let table = state
        .split_buses
        .get(&bdf.bus)
        .copied()
        .unwrap_or(state.shared_context);
    Ok(read_entry(&mut state.tables, table, bdf))
}

/// Puts `device`, attached to no domain, back in the identity domain. For the boot self-test,
/// which attaches a device nothing uses and must leave it as it was.
pub(super) fn restore_identity(device: Device) -> Result<(), DomainError> {
    let index = unit_for(device)?;
    if let Some(Vendor::AmdVi(_)) = Vendor::current() {
        return amdvi::restore_identity(index, device);
    }
    set_context(index, device.bdf, Target::Identity, |old| {
        if old.is_present() && old.domain_id() != IDENTITY_DOMAIN {
            Err(DomainError::Busy {
                did: old.domain_id(),
            })
        } else {
            Ok(())
        }
    })?;
    Ok(())
}

/// The index of the unit that serves `device`, for the self-test.
pub(super) fn unit_index(device: Device) -> Result<usize, DomainError> {
    unit_for(device)
}

/// The index in `UNITS` of the VT-d unit that serves `device`, for interrupt remapping.
pub(super) fn vtd_unit_index(device: Device) -> Result<usize, DomainError> {
    vtd_unit_for(device)
}

/// The context table for `bus` on `unit`, giving the bus a table of its own first if it
/// still uses the shared identity table. The copy holds the same entries, so splitting does
/// not change any device's translation.
fn context_table(unit: &Unit, state: &mut UnitState, bus: u8) -> Result<u64, DomainError> {
    if let Some(&table) = state.split_buses.get(&bus) {
        return Ok(table);
    }
    let table = state
        .tables
        .alloc_table()
        .map_err(|NoMemory| DomainError::NoMemory)?;
    for i in 0..ENTRIES {
        let value = state
            .tables
            .table(state.shared_context)
            .get(i)
            .copied()
            .unwrap_or(0);
        if let Some(slot) = state.tables.table(table).get_mut(i) {
            *slot = value;
        }
    }
    unit.flush_lines(table, crate::memory::PAGE_SIZE);
    let root = state.root_table;
    if let Some(entry) = state
        .tables
        .table(root)
        .chunks_exact_mut(2)
        .nth(usize::from(bus))
    {
        // One aligned 64-bit store: the unit sees either the old or the new table, both of
        // which hold the same entries.
        entry.copy_from_slice(&RootEntry::new(table).0);
    }
    unit.flush_lines(root.wrapping_add(u64::from(bus).wrapping_mul(16)), 16);
    state.split_buses.insert(bus, table);
    unit.invalidate(state, &[Request::ContextGlobal])?;
    Ok(table)
}

/// The context entry for `bdf` in the context table at `table`.
fn read_entry(tables: &mut KernelTables, table: u64, bdf: Bdf) -> ContextEntry {
    let words = tables
        .table(table)
        .chunks_exact(2)
        .nth(usize::from(bdf.devfn()))
        .map_or([0, 0], |pair| {
            [
                pair.first().copied().unwrap_or(0),
                pair.last().copied().unwrap_or(0),
            ]
        });
    ContextEntry(words)
}

/// Writes the context entry for `bdf`: the low word (with the present bit) is cleared first
/// and set last, so the unit never reads a present entry with half of it changed.
fn write_entry(unit: &Unit, tables: &mut KernelTables, table: u64, bdf: Bdf, entry: ContextEntry) {
    let devfn = usize::from(bdf.devfn());
    let [lo, hi] = entry.0;
    let mut store = |word: usize, value: u64| {
        if let Some(slot) = tables
            .table(table)
            .chunks_exact_mut(2)
            .nth(devfn)
            .and_then(|pair| pair.get_mut(word))
        {
            *slot = value;
        }
        // Keep the stores in this order.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    };
    store(0, 0);
    store(1, hi);
    store(0, lo);
    unit.flush_lines(
        table.wrapping_add(u64::from(bdf.devfn()).wrapping_mul(16)),
        16,
    );
}
