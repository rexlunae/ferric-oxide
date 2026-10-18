//! Ferric Oxide: DMA mappings for `memory:translation` in a bound domain (io/iommu-design.md
//! in the project files, section 2 and step 4).
//!
//! A [`DmaSpace`] is a domain plus the mappings a driver's translations made in it. When the
//! driver's address space has bound the domain, translating a virtual page:
//!
//! 1. finds the page's frame, and for a physically contiguous allocation
//!    (`memory:zeroed?phys_contiguous`, which is what `common::dma` hands out) the whole
//!    allocation, so a buffer translated through its first page is contiguous in IOVA space
//!    too, as drivers expect;
//! 2. returns the IOVA it already has if it is mapped, adding the virtual range to the
//!    mapping's users (the per-domain reference count);
//! 3. otherwise *pins* the frames (takes a reference on each frame the frame allocator owns, so
//!    they are not freed or reused while the device can reach them, even if the process
//!    unmaps them), allocates a fresh IOVA block and maps the frames there with the page's
//!    protection: read-only pages are read-only for the device too.
//!
//! The answer, like the physical address it replaces, is the address of the page's start.
//!
//! When the process takes write access away from translated memory (`mprotect`), the mappings
//! it reaches become read-only for the device as well, with the IOTLB invalidated before the
//! call returns ([`DmaSpace::narrow`]); a later translation of a writable page widens them
//! again.
//!
//! Mappings are taken down strictly (clear the entries, invalidate the IOTLB through the
//! invalidation queue and wait, then unpin) when the process unmaps the memory
//! ([`DmaSpace::release`], called from the address-space code) or when the domain closes. On
//! close the devices are blocked first, which already invalidates everything the domain
//! translated, so the tables are freed whole before the frames are unpinned. Whenever the
//! hardware does not confirm an invalidation, the frames stay pinned for good.
//!
//! The bookkeeping (which frames have which IOVA, and who still reaches them) is the
//! host-tested `ferric_iommu::dma` module.
//!
//! With the `regbuf` feature a domain also holds *grant mappings* (`MapGrant`,
//! io/registered-buffers-design.md, PR 3): the frames of another process's registered buffer,
//! pinned and mapped at consecutive IOVAs of one fresh block, with the grant's access. They
//! are kept apart from the translation mappings, by IOVA, and each carries a ticket so a
//! revocation that arrives late (after the driver unmapped it and the IOVA was reused) cannot
//! take down a newer mapping. They are taken down strictly too: [`DmaSpace::unmap_grant`]
//! returns only after the IOTLB invalidation completed.
//!
//! A revocation (the grant revoked or closed under the driver) unpins the frames at once but
//! *retires* the IOVA block instead of freeing it: the driver may still hold the IOVA in a
//! queued request, so the block is not handed out again until the driver's own `UnmapGrant`
//! of it ([`DmaSpace::release_retired`]) or the domain's close. Until then DMA to it faults,
//! and can never reach memory that a later mapping put there (io/registered-buffers-design.md,
//! PR 4 decisions).

use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "regbuf")]
use ferric_iommu::dma::{
    complete_unmap,
    grant::{map_runs, record_installation, MapAttempt, MapRunsError, MapStep},
    UnmapFailure, UnmapState,
};
use ferric_iommu::dma::{DmaMappings, Mapping, Overlap, Span};
use ferric_vtd::{iova::IovaRange, table::Access};

use super::domain::{Domain, DomainError};
use crate::{
    context::{
        memory::{AddrSpace, GenericFlusher, NopFlusher, Provider, TlbShootdownActions},
        reshare::{self, ReshareError},
    },
    memory::{get_page_info, Frame, Page, PhysicalAddress, RefCount, RefKind, VirtualAddress},
};

/// The page size, as the 64-bit quantity IOVA arithmetic uses.
const PAGE: u64 = crate::memory::PAGE_SIZE as u64;

/// Why a translation into the domain failed.
#[derive(Debug)]
pub enum DmaError {
    /// The page is not mapped (or not yet populated) in the address space.
    NotMapped {
        /// The virtual address asked for.
        virt: usize,
    },
    /// A frame's reference count is full, so it cannot be pinned.
    PinOverflow {
        /// The frame's physical address.
        phys: u64,
    },
    /// The frame is not in use, which a mapped page's frame always is.
    FreeFrame {
        /// The frame's physical address.
        phys: u64,
    },
    /// The physical range is already mapped; the bookkeeping refused it.
    Overlap(Overlap),
    /// Allocating the IOVA or changing the domain's tables failed.
    Domain(DomainError),
    /// Ferric Oxide: the page is borrowed from another process (context/reshare.rs).
    Borrowed(ReshareError),
    /// No grant mapping (with this ticket) starts at the IOVA.
    #[cfg(feature = "regbuf")]
    NoGrantMapping {
        /// The IOVA asked for.
        iova: u64,
    },
    /// A grant mapping has no pages, or too many to address.
    #[cfg(feature = "regbuf")]
    GrantSize,
    /// A previous strict grant unmap failed; the domain must be blocked before release.
    #[cfg(feature = "regbuf")]
    FailedUnmap {
        /// The IOVA whose teardown failed.
        iova: u64,
    },
    /// Grant installation or rollback could not confirm that DMA is unreachable.
    #[cfg(feature = "regbuf")]
    GrantMap(MapRunsError<DomainError>),
}

impl fmt::Display for DmaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMapped { virt } => write!(f, "{virt:#x} is not mapped"),
            Self::PinOverflow { phys } => write!(f, "cannot pin frame {phys:#x}"),
            Self::FreeFrame { phys } => write!(f, "frame {phys:#x} is free"),
            Self::Overlap(err) => write!(f, "{err}"),
            Self::Domain(err) => write!(f, "{err}"),
            Self::Borrowed(err) => write!(f, "{err}"),
            #[cfg(feature = "regbuf")]
            Self::NoGrantMapping { iova } => write!(f, "no grant is mapped at IOVA {iova:#x}"),
            #[cfg(feature = "regbuf")]
            Self::GrantSize => f.write_str("a grant mapping must have between 1 and 2^52 pages"),
            #[cfg(feature = "regbuf")]
            Self::FailedUnmap { iova } => {
                write!(f, "unmap of grant IOVA {iova:#x} previously failed")
            }
            #[cfg(feature = "regbuf")]
            Self::GrantMap(error) => write!(f, "{error}"),
        }
    }
}

impl From<DomainError> for DmaError {
    fn from(err: DomainError) -> Self {
        Self::Domain(err)
    }
}

/// What the kernel keeps to undo one mapping: its IOVA block and the frames it pinned.
#[derive(Debug)]
pub struct Held {
    block: IovaRange,
    pins: Pins,
}

/// Frames with a reference held for the device. Dropping it leaks the references, which is
/// the safe outcome when an invalidation failed; [`Pins::release`] gives them back.
#[derive(Debug, Default)]
struct Pins(Vec<Frame>);

impl Pins {
    /// Takes a reference on every allocator-owned frame in `len` bytes from `phys`. Frames the
    /// allocator does not own (borrowed physical memory such as another device's BAR) are never
    /// freed, so they need none.
    fn take(phys: u64, len: u64) -> Result<Self, DmaError> {
        let mut pins = Self::default();
        let mut offset = 0;
        while offset < len {
            let addr = phys.saturating_add(offset);
            offset = offset.saturating_add(PAGE);
            let Some(frame) = usize::try_from(addr)
                .ok()
                .filter(|&a| a != 0)
                .map(|a| Frame::containing(PhysicalAddress::new(a)))
            else {
                continue;
            };
            if let Err(err) = pins.pin(frame) {
                pins.release();
                return Err(err);
            }
        }
        Ok(pins)
    }

    /// Takes a reference on each of `frames`, as [`Pins::take`] does.
    #[cfg(feature = "regbuf")]
    fn take_frames(frames: &[Frame]) -> Result<Self, DmaError> {
        let mut pins = Self::default();
        for &frame in frames {
            if let Err(err) = pins.pin(frame) {
                pins.release();
                return Err(err);
            }
        }
        Ok(pins)
    }

    /// Takes a reference on `frame` if the allocator owns it, and records it.
    fn pin(&mut self, frame: Frame) -> Result<(), DmaError> {
        let Some(info) = get_page_info(frame) else {
            return Ok(());
        };
        let phys = frame.base().data() as u64;
        // A private page (one reference) or a shared one gets a shared reference, so the
        // owner keeps writing the same frame. A copy-on-write page gets a copy-on-write
        // reference: it is mapped read-only for the device as well.
        let kind = match info.refcount() {
            Some(RefCount::Cow(_)) => RefKind::Cow,
            Some(RefCount::One | RefCount::Shared(_)) => RefKind::Shared,
            None => return Err(DmaError::FreeFrame { phys }),
        };
        if info.add_ref(kind).is_err() {
            return Err(DmaError::PinOverflow { phys });
        }
        self.0.push(frame);
        Ok(())
    }

    /// Drops the references, freeing frames nothing else uses.
    fn release(self) {
        for frame in self.0 {
            NopFlusher.queue(frame, None, TlbShootdownActions::FREE);
        }
    }
}

/// The frames a translation of one page reaches, and the virtual range that reaches them.
struct Target {
    /// The page's frame.
    phys: u64,
    /// The whole range to map: the page, or its physically contiguous allocation.
    range: Span,
    /// The virtual range that reaches `range`.
    user: Span,
    /// Whether the page is writable, and so the device may write it.
    writable: bool,
}

/// What translating `virt` in `space` would map.
fn target(space: &AddrSpace, virt: VirtualAddress) -> Result<Target, DmaError> {
    let not_mapped = DmaError::NotMapped { virt: virt.data() };
    let page = Page::containing_address(virt);
    let (grant_base, info) = space.grants.contains(page).ok_or(not_mapped)?;
    reshare::check_grant(grant_base, info).map_err(DmaError::Borrowed)?;
    let (phys, flags) = space
        .table
        .utable
        .translate(page.start_address())
        .ok_or(DmaError::NotMapped { virt: virt.data() })?;
    let phys = phys.data() as u64;
    let page_start = page.start_address().data() as u64;
    let contiguous = matches!(
        info.provider,
        Provider::Allocated {
            phys_contiguous: true,
            ..
        }
    );
    let whole = contiguous.then(|| {
        let index = page.offset_from(grant_base) as u64;
        let before = index.saturating_mul(PAGE);
        let len = (info.page_count() as u64).saturating_mul(PAGE);
        (
            Span::new(phys.saturating_sub(before), len),
            Span::new(page_start.saturating_sub(before), len),
        )
    });
    let (range, user) = whole.unwrap_or((Span::new(phys, PAGE), Span::new(page_start, PAGE)));
    Ok(Target {
        phys,
        range,
        user,
        writable: flags.has_write(),
    })
}

/// The device's access for a page that is writable or not.
fn access(writable: bool) -> Access {
    if writable {
        Access::ReadWrite
    } else {
        Access::Read
    }
}

/// A grant mapping: a registered buffer's frames at one IOVA block.
#[cfg(feature = "regbuf")]
struct GrantHeld {
    block: IovaRange,
    /// The mapped length, from the block's start; the block may be longer (alignment).
    len: u64,
    pins: Pins,
    ticket: u64,
    unmap: UnmapState,
}

/// The runs of physically consecutive frames in `frames`: each run's first frame's physical
/// address and its length in bytes.
#[cfg(feature = "regbuf")]
fn runs(frames: &[Frame]) -> Vec<(u64, u64)> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for frame in frames {
        let phys = frame.base().data() as u64;
        match runs.last_mut() {
            Some((start, len)) if start.checked_add(*len) == Some(phys) => {
                *len = len.saturating_add(PAGE);
            }
            _ => runs.push((phys, PAGE)),
        }
    }
    runs
}

/// A domain and the mappings that translations made in it.
pub struct DmaSpace {
    domain: Domain,
    mappings: DmaMappings<Held>,
    /// Grant mappings by IOVA.
    #[cfg(feature = "regbuf")]
    grants: alloc::collections::BTreeMap<u64, GrantHeld>,
    /// IOVA blocks of revoked grant mappings, by IOVA, reserved until the driver unmaps them.
    #[cfg(feature = "regbuf")]
    retired: alloc::collections::BTreeMap<u64, IovaRange>,
}

impl DmaSpace {
    /// `domain`, with nothing mapped by translation yet.
    pub fn new(domain: Domain) -> Self {
        Self {
            domain,
            mappings: DmaMappings::new(),
            #[cfg(feature = "regbuf")]
            grants: alloc::collections::BTreeMap::new(),
            #[cfg(feature = "regbuf")]
            retired: alloc::collections::BTreeMap::new(),
        }
    }

    /// The domain, to attach and detach devices.
    pub fn domain(&mut self) -> &mut Domain {
        &mut self.domain
    }

    /// The domain ID.
    pub fn id(&self) -> u16 {
        self.domain.id()
    }

    /// How many devices are attached.
    pub fn device_count(&self) -> usize {
        self.domain.device_count()
    }

    /// How many mappings translations made.
    pub fn mapping_count(&self) -> usize {
        self.mappings.len()
    }

    /// The IOVA of the page at `virt` in `space`, which has bound the domain: the IOVA it
    /// already has, or a fresh one after pinning and mapping its frames (see the module
    /// documentation).
    ///
    /// # Errors
    ///
    /// [`DmaError::NotMapped`] when the page is not present, [`DmaError::PinOverflow`] and
    /// [`DmaError::FreeFrame`] when a frame cannot be pinned, and [`DmaError::Domain`] when
    /// the IOVA space is full or the tables cannot be changed. Nothing stays pinned or mapped
    /// on error.
    pub fn translate(&mut self, space: &AddrSpace, virt: VirtualAddress) -> Result<u64, DmaError> {
        let mut target = target(space, virt)?;
        if self.mappings.get(target.phys).is_some() {
            self.widen(target.phys, target.writable)?;
            self.mappings.add_user(target.phys, target.user);
        } else {
            // Frames mapped through another virtual mapping keep their IOVA; map only this
            // page then.
            if self.mappings.overlapping(target.range).is_some() {
                target.range = Span::new(target.phys, PAGE);
                target.user = Span::new(target.user.start, PAGE);
            }
            self.map(&target)?;
        }
        self.mappings
            .iova_of(target.phys)
            .ok_or(DmaError::NotMapped { virt: virt.data() })
    }

    /// Pins and maps `target.range` at a fresh IOVA block.
    fn map(&mut self, target: &Target) -> Result<(), DmaError> {
        let Span { start: phys, len } = target.range;
        let pins = Pins::take(phys, len)?;
        let block = match self.domain.alloc_iova(len) {
            Ok(block) => block,
            Err(err) => {
                pins.release();
                return Err(err.into());
            }
        };
        if let Err(err) = self
            .domain
            .map(block.start, phys, len, access(target.writable))
        {
            // A table error leaves nothing mapped (`map` undoes its part, with the
            // invalidation that needs). A failed invalidation may leave entries a unit uses,
            // so then the block and the frames stay taken.
            if matches!(err, DomainError::Map(_)) {
                self.give_back(block, pins);
            }
            return Err(err.into());
        }
        let mapping = Mapping::new(
            phys,
            len,
            block.start,
            target.writable,
            target.user,
            Held { block, pins },
        );
        self.mappings.insert(mapping).map_err(|err| {
            // `translate` checked for overlaps, so this is not reached. Take the entries down
            // again; the frames stay pinned, since nothing records them any more.
            if let Err(unmap) = self.domain.unmap(block.start, len) {
                error!("IOMMU: cannot unmap IOVA {:#x}: {unmap}", block.start);
            }
            DmaError::Overlap(err)
        })
    }

    /// Makes the mapping that holds `phys` writable for the device when `writable` asks for
    /// it and it is read-only (the page was made writable since its first translation).
    fn widen(&mut self, phys: u64, writable: bool) -> Result<(), DmaError> {
        let Some(mapping) = self.mappings.get(phys) else {
            return Ok(());
        };
        if !writable || mapping.writable() {
            return Ok(());
        }
        let (iova, len) = (mapping.iova(), mapping.phys().len);
        if let Err(err) = self.domain.protect(iova, len, Access::ReadWrite) {
            // Part of it may be writable now. Make it read-only again, as the bookkeeping
            // says, or take it down (its users must translate again).
            let gone = match self.domain.protect(iova, len, Access::Read) {
                Ok(()) => None,
                Err(_) => self.mappings.remove(phys),
            };
            if let Some(gone) = gone {
                self.tear_down(gone);
            }
            return Err(err.into());
        }
        if let Some(mapping) = self.mappings.get_mut(phys) {
            mapping.set_writable();
        }
        Ok(())
    }

    /// The process took write access away from `len` bytes of its memory from virtual
    /// address `start` (`mprotect`): makes every writable mapping that memory reaches
    /// read-only for the device too, with the IOTLB invalidated before this returns.
    ///
    /// A mapping is narrowed whole, even when only part of it lost write access or another
    /// virtual range that reaches it is still writable; translating a page that is still
    /// writable widens it again. A mapping whose access cannot be changed is taken down
    /// instead, so its users must translate again.
    pub fn narrow(&mut self, start: usize, len: usize) {
        let reached = self
            .mappings
            .writable_reached_by(Span::new(start as u64, len as u64));
        if reached.is_empty() {
            return;
        }
        info!(
            "IOMMU: domain {}: {} DMA mapping(s) made read-only after write access was removed",
            self.domain.id(),
            reached.len()
        );
        for phys in reached {
            let Some(mapping) = self.mappings.get(phys) else {
                continue;
            };
            let (iova, len) = (mapping.iova(), mapping.phys().len);
            match self.domain.protect(iova, len, Access::Read) {
                Ok(()) => {
                    if let Some(mapping) = self.mappings.get_mut(phys) {
                        mapping.set_read_only();
                    }
                }
                Err(err) => {
                    warn!(
                        "IOMMU: domain {}: cannot make IOVA {iova:#x}+{len:#x} read-only: {err}; unmapping it",
                        self.domain.id()
                    );
                    if let Some(gone) = self.mappings.remove(phys) {
                        self.tear_down(gone);
                    }
                }
            }
        }
    }

    /// The process unmapped `len` bytes of its memory from virtual address `start`: tears down
    /// the mappings no virtual memory of the process reaches any more.
    pub fn release(&mut self, start: usize, len: usize) {
        let released = self.mappings.release(Span::new(start as u64, len as u64));
        if released.is_empty() {
            return;
        }
        info!(
            "IOMMU: domain {}: {} DMA mapping(s) released after an unmap",
            self.domain.id(),
            released.len()
        );
        for mapping in released {
            self.tear_down(mapping);
        }
    }

    /// Unmaps one mapping strictly, then frees its IOVA block and unpins its frames. If the
    /// IOTLB invalidation fails, the frames stay pinned and the block allocated, since the
    /// device may still reach them.
    fn tear_down(&mut self, mapping: Mapping<Held>) {
        let (iova, len) = (mapping.iova(), mapping.phys().len);
        match self.domain.unmap(iova, len) {
            Ok(()) => self.give_back(mapping.data.block, mapping.data.pins),
            Err(err) => {
                error!(
                    "IOMMU: cannot unmap IOVA {iova:#x}+{len:#x} from domain {}: {err}; its frames stay pinned",
                    self.domain.id()
                );
            }
        }
    }

    /// Pins `frames` (the pages of a grant, in order) and maps them at consecutive IOVAs of a
    /// fresh block, writable for the device when `writable` is set; returns the IOVA of the
    /// first. `ticket` names the mapping for [`DmaSpace::unmap_grant`].
    ///
    /// # Errors
    ///
    /// [`DmaError::GrantSize`] for no frames or too many, [`DmaError::PinOverflow`] and
    /// [`DmaError::FreeFrame`] when a frame cannot be pinned, and [`DmaError::Domain`] when the
    /// IOVA space is full or the tables cannot be changed. A rejected attempt is fully
    /// rolled back. An uncertain installation returns a retained attempt: its pins, IOVA
    /// and failed teardown state stay owned until successful domain blocking. The caller
    /// must publish that attempt to the grant before returning its error to user space.
    #[cfg(feature = "regbuf")]
    pub fn map_grant(
        &mut self,
        frames: &[Frame],
        writable: bool,
        ticket: u64,
    ) -> Result<MapAttempt<u64, DmaError>, DmaError> {
        let len = u64::try_from(frames.len())
            .ok()
            .and_then(|n| n.checked_mul(PAGE))
            .filter(|&len| len != 0)
            .ok_or(DmaError::GrantSize)?;
        let pins = Pins::take_frames(frames)?;
        let block = match self.domain.alloc_iova(len) {
            Ok(block) => block,
            Err(err) => {
                pins.release();
                return Err(err.into());
            }
        };
        let mapped = map_runs(
            &runs(frames),
            |step| match step {
                MapStep::Map { offset, phys, len } => self.domain.map(
                    block.start.saturating_add(offset),
                    phys,
                    len,
                    access(writable),
                ),
                MapStep::Rollback { len } => self.domain.unmap(block.start, len),
            },
            |err| matches!(err, DomainError::Map(_)),
        );
        let held = GrantHeld {
            block,
            len,
            pins,
            ticket,
            unmap: UnmapState::default(),
        };
        record_installation(&mut self.grants, block.start, held, mapped, |held| {
            held.unmap = UnmapState::failed();
        })
        .map(|attempt| attempt.map_error(DmaError::GrantMap))
        .map_err(|(error, held)| {
            // Only a confirmed rollback returns ownership instead of publishing a record.
            self.give_back(held.block, held.pins);
            match error {
                MapRunsError::Clean(error) => error.into(),
                MapRunsError::InvalidLength => DmaError::GrantSize,
                error => DmaError::GrantMap(error),
            }
        })
    }

    /// Takes the grant mapping at `iova` down strictly (entries cleared, IOTLB invalidated and
    /// waited on), then unpins its frames. Without `ticket` (the driver's own `UnmapGrant`) the
    /// block is freed. With `ticket` (a revocation) only the mapping that ticket names is taken
    /// down, and its block is retired rather than freed, until [`DmaSpace::release_retired`].
    /// Returns the ticket of the mapping taken down.
    ///
    /// # Errors
    ///
    /// [`DmaError::NoGrantMapping`] when no such mapping is there, and [`DmaError::Domain`]
    /// when the unmap or its invalidation failed. The mapping, pins and block remain owned,
    /// and [`DmaError::FailedUnmap`] is returned on later attempts until domain closure.
    #[cfg(feature = "regbuf")]
    pub fn unmap_grant(&mut self, iova: u64, ticket: Option<u64>) -> Result<u64, DmaError> {
        let domain = &mut self.domain;
        let held = complete_unmap(&mut self.grants, iova, |held| {
            if ticket.is_some_and(|t| t != held.ticket) {
                return Err(DmaError::NoGrantMapping { iova });
            }
            held.unmap
                .attempt(|| domain.unmap(iova, held.len))
                .map_err(|err| {
                    error!(
                        "IOMMU: cannot unmap grant IOVA {iova:#x}+{:#x} from domain {}: {err}; its frames stay pinned",
                        held.len,
                        domain.id()
                    );
                    match err {
                        UnmapFailure::Backend(error) => error.into(),
                        _ => DmaError::FailedUnmap { iova },
                    }
                })
        })?
        .ok_or(DmaError::NoGrantMapping { iova })?;
        if ticket.is_some() {
            held.pins.release();
            self.retired.insert(iova, held.block);
        } else {
            self.give_back(held.block, held.pins);
        }
        Ok(held.ticket)
    }

    /// Frees the block of the revoked grant mapping that was at `iova`, now that the driver
    /// has unmapped it. Whether there was one.
    #[cfg(feature = "regbuf")]
    pub fn release_retired(&mut self, iova: u64) -> bool {
        let Some(block) = self.retired.remove(&iova) else {
            return false;
        };
        if let Err(err) = self.domain.free_iova(block) {
            warn!(
                "IOMMU: domain {}: cannot free IOVA block {:#x}+{:#x}: {err}",
                self.domain.id(),
                block.start,
                block.len
            );
        }
        true
    }

    /// How many grant mappings the domain holds.
    #[cfg(feature = "regbuf")]
    pub fn grant_count(&self) -> usize {
        self.grants.len()
    }

    /// Frees an unmapped block and unpins its frames.
    fn give_back(&mut self, block: IovaRange, pins: Pins) {
        if let Err(err) = self.domain.free_iova(block) {
            warn!(
                "IOMMU: domain {}: cannot free IOVA block {:#x}+{:#x}: {err}",
                self.domain.id(),
                block.start,
                block.len
            );
        }
        pins.release();
    }
}

impl Drop for DmaSpace {
    fn drop(&mut self) {
        // Blocking the devices invalidates everything the domain translated, so the frames
        // can be unpinned without unmapping them one by one.
        let clean = self.domain.close();
        for mapping in self.mappings.drain() {
            if clean {
                mapping.data.pins.release();
            }
        }
        #[cfg(feature = "regbuf")]
        for (_, held) in core::mem::take(&mut self.grants) {
            if clean {
                held.pins.release();
            }
        }
    }
}
