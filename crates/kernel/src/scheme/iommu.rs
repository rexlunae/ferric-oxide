//! Ferric Oxide: the `kernel.iommu` scheme, which gives user space DMA domains
//! (io/iommu-design.md in the project files, section 1 and step 3).
//!
//! - Opening the scheme root (`/scheme/kernel.iommu`) as root gives the *attach capability*.
//!   pcid opens it at boot. A later capability system replaces the root check.
//! - Opening [`DOMAIN_PATH`] from the attach capability creates a domain with empty tables, so
//!   DMA from its devices faults until something is mapped.
//! - [`Request::Attach`] and [`Request::Detach`], called on the attach capability and a domain,
//!   move a device into the domain (mapping its RMRRs one-to-one first) or block it.
//!   [`Request::Bridge`] on the attach capability reports a bridge's bus range, which resolves
//!   device scopes that name bridges.
//! - [`Request::Bind`] on a domain makes it the calling process's own: a process (an address
//!   space) binds at most one domain, and a domain is bound by at most one process. Under
//!   `kernel.iommu = "enforce"` only a domain with a device attached can be bound.
//! - [`Request::Join`] on a domain moves its devices into the domain the calling process has
//!   bound, so a driver of several functions keeps one domain: pcid gives it a domain for each
//!   function, it binds the first and joins the others into it. A forked child has an address
//!   space of its own, so it starts with no bound domain (`EACCES` from translation under
//!   `enforce`), and it cannot bind its parent's domain while the parent has it bound.
//! - Once a process has bound a domain, `memory:translation` ([`translate`]) answers with IOVAs
//!   in it: the frames are pinned and mapped with the page's protection, and repeat
//!   translations return the same IOVA (`arch/x86_64/iommu/dma.rs`). When the process unmaps
//!   that memory, the address-space code calls [`release`], which takes the mappings down
//!   strictly. When it takes write access away (`mprotect`), the address-space code calls
//!   [`narrow`], which makes the mappings that memory reaches read-only for the device. A
//!   process without a bound domain gets physical addresses, or `EACCES` under `enforce`.
//! - [`Request::AllocIrte`] on the attach capability makes an interrupt remapping entry that
//!   delivers a vector to a CPU for one function's MSI or MSI-X requests, and writes the
//!   remappable-format message that names it into the payload; [`Request::FreeIrte`] frees it
//!   (`arch/x86_64/iommu/interrupt.rs`). pcid makes these calls when a driver asks it for an MSI
//!   message, and frees the entries when the driver's channel closes. `ENODEV` means the unit
//!   does not remap interrupts, so compatibility-format messages still work.
//! - With the `regbuf` feature, [`Request::MapGrant`] on a domain the caller has bound maps a
//!   page-aligned `kernel.regbuf` grant (named by the caller's descriptor number) at a fresh
//!   contiguous IOVA range, with the grant's access, and returns the IOVA; the grant records
//!   the mapping (`scheme/regbuf.rs`). [`Request::UnmapGrant`], or revoking or closing the
//!   grant (or one it was derived from, or its region), takes it down strictly: the entries are
//!   cleared and the IOTLB invalidated and waited on before the call returns, so the device's
//!   DMA to the old IOVA faults. Without a bound domain `MapGrant` fails with `ENODEV`: the
//!   driver falls back to its own bounce buffers, and a device never gets physical addresses
//!   of another process's memory. Each grant mapping has a ticket, so a revocation cannot take
//!   down a newer mapping that reuses the IOVA after the driver unmapped the old one.
//! - Closing the last descriptor of a domain blocks its devices (DMA faults), frees its
//!   tables and unpins every frame mapped in it. Domain descriptors cannot be duplicated into
//!   new descriptions, so the scheme's close is the last close.
//!
//! The call encoding is in the shared `ferric-iommu` crate (`crates/iommu`), which pcid uses
//! too.

use alloc::{collections::BTreeMap, sync::Weak, vec::Vec};
#[cfg(feature = "regbuf")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicUsize, Ordering};

use ferric_iommu::abi::{MsiMessage, MsiRoute, PciDevice, Request, SourceCheck, DOMAIN_PATH};
use ferric_vtd::{dmar::BusRange, interrupt::SourceValidation, plan::SelectError, Bdf};
use spin::Mutex;
use syscall::{
    error::{
        Error, Result, EACCES, EBADF, EBUSY, EEXIST, EINVAL, EIO, ENODEV, ENOENT, ENOMEM, ENOSPC,
        ENXIO, EPERM,
    },
    flag::CallFlags,
};

use super::{CallerCtx, KernelScheme, OpenResult, StrOrBytes};
use crate::{
    arch::iommu::{
        dma::{DmaError, DmaSpace},
        domain::{self, Device, Domain, DomainError},
        interrupt::{self, IrqError},
    },
    context::{
        self,
        file::InternalFlags,
        memory::{AddrSpace, AddrSpaceWrapper, PageSpan},
    },
    memory::{VirtualAddress, PAGE_SIZE},
    sync::CleanLockToken,
    syscall::usercopy::UserSliceRw,
};

/// The number of the scheme root, which carries no rights of its own.
const ROOT: usize = 0;

/// The scheme.
pub struct IommuScheme;

/// An open handle.
// There are a handful of handles (one per driver), so boxing the domain is not worth it.
#[allow(clippy::large_enum_variant)]
enum Handle {
    /// The attach capability.
    AttachCap,
    /// A domain with its translation mappings, and the address space that bound it.
    Domain {
        space: DmaSpace,
        bound: Option<Weak<AddrSpaceWrapper>>,
    },
}

/// Every open handle but the root, by number.
static HANDLES: Mutex<BTreeMap<usize, Handle>> = Mutex::new(BTreeMap::new());

/// The next handle number.
static NEXT_ID: AtomicUsize = AtomicUsize::new(ROOT + 1);

/// How many domains have been bound and not closed. While it is zero, translation and unmapping
/// skip the handle table.
static BOUND: AtomicUsize = AtomicUsize::new(0);

/// Stores `handle` and returns its number.
fn insert(handle: Handle) -> usize {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    HANDLES.lock().insert(id, handle);
    id
}

/// The errno a failed domain operation returns to user space.
fn errno(err: &DomainError) -> Error {
    Error::new(match err {
        DomainError::NotEnabled | DomainError::UnitNotEnabled { .. } => ENODEV,
        DomainError::Select(SelectError::NotCovered) => ENODEV,
        DomainError::Select(_) => ENXIO,
        DomainError::NoDomainIds => ENOSPC,
        DomainError::NoMemory | DomainError::Iova(_) => ENOMEM,
        DomainError::Busy { .. } => EBUSY,
        DomainError::AlreadyAttached => EEXIST,
        DomainError::NotAttached => ENOENT,
        DomainError::Map(_)
        | DomainError::Unmap(_)
        | DomainError::Protect(_)
        | DomainError::Hardware(_) => EIO,
    })
}

/// The errno a failed translation into a domain returns to user space.
fn dma_errno(err: &DmaError) -> Error {
    match err {
        // As without a domain: the page is not there.
        DmaError::NotMapped { .. } => Error::new(ENOENT),
        DmaError::PinOverflow { .. } => Error::new(ENOMEM),
        DmaError::FreeFrame { .. } | DmaError::Overlap(_) => Error::new(EIO),
        DmaError::Domain(err) => errno(err),
        DmaError::Borrowed(err) => Error::from(*err),
        #[cfg(feature = "regbuf")]
        DmaError::NoGrantMapping { .. } => Error::new(ENOENT),
        #[cfg(feature = "regbuf")]
        DmaError::GrantSize => Error::new(EINVAL),
    }
}

/// The errno a failed interrupt remapping call returns to user space. `ENODEV` tells pcid
/// that the device's interrupts are not remapped, so a compatibility-format message works.
fn irq_errno(err: &IrqError) -> Error {
    Error::new(match err {
        IrqError::Skip(_) | IrqError::NotEnabled => ENODEV,
        IrqError::Unit(err) => return errno(err),
        IrqError::NoMemory => ENOMEM,
        IrqError::Full => ENOSPC,
        IrqError::NoCpu { .. } | IrqError::Destination { .. } => EINVAL,
        IrqError::Vector { .. } => EPERM,
        IrqError::Hardware(_) => EIO,
        IrqError::NotAllocated(_) | IrqError::BadId { .. } => ENOENT,
    })
}

/// The kernel's form of a device address.
fn device(device: PciDevice) -> Device {
    Device {
        segment: device.segment(),
        bdf: Bdf::new(device.bus(), device.device(), device.function()),
    }
}

impl KernelScheme for IommuScheme {
    fn scheme_root(&self, _token: &mut CleanLockToken) -> Result<usize> {
        Ok(ROOT)
    }

    fn kopenat(
        &self,
        file: usize,
        path: StrOrBytes,
        _flags: usize,
        _fcntl_flags: u32,
        caller: CallerCtx,
        _token: &mut CleanLockToken,
    ) -> Result<OpenResult> {
        let path = path.as_bytes();
        let path = path.strip_prefix(b"/").unwrap_or(path);
        let handle = if file == ROOT {
            if !path.is_empty() {
                return Err(Error::new(ENOENT));
            }
            if caller.uid != 0 {
                return Err(Error::new(EACCES));
            }
            Handle::AttachCap
        } else {
            if !matches!(HANDLES.lock().get(&file), Some(Handle::AttachCap)) {
                return Err(Error::new(EACCES));
            }
            if path != DOMAIN_PATH.as_bytes() {
                return Err(Error::new(ENOENT));
            }
            let domain = Domain::new().map_err(|err| {
                warn!("IOMMU: cannot create a domain: {err}");
                errno(&err)
            })?;
            Handle::Domain {
                space: DmaSpace::new(domain),
                bound: None,
            }
        };
        Ok(OpenResult::SchemeLocal(
            insert(handle),
            InternalFlags::empty(),
        ))
    }

    fn close(&self, id: usize, _token: &mut CleanLockToken) -> Result<()> {
        // Dropping a domain blocks its devices and frees its tables; do that outside the lock.
        let handle = HANDLES.lock().remove(&id);
        if let Some(Handle::Domain { space, bound }) = &handle {
            if bound.is_some() {
                BOUND.fetch_sub(1, Ordering::Relaxed);
            }
            info!(
                "IOMMU: domain {} closed, {} device(s) blocked, {} DMA mapping(s) unpinned",
                space.id(),
                space.device_count(),
                space.mapping_count()
            );
        }
        drop(handle);
        Ok(())
    }

    fn kcall(
        &self,
        fds: &[usize],
        payload: UserSliceRw,
        _flags: CallFlags,
        metadata: &[u64],
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let request = Request::decode(metadata).map_err(|_| Error::new(EINVAL))?;
        match (request, fds) {
            (Request::Attach(dev), &[cap, domain]) => attach(cap, domain, device(dev)).map(|()| 0),
            (Request::Detach(dev), &[cap, domain]) => detach(cap, domain, device(dev)).map(|()| 0),
            (Request::Bridge(bridge), &[cap]) => {
                require_cap(cap)?;
                let address = device(bridge.address);
                domain::report_bridge(
                    address.segment,
                    address.bdf,
                    BusRange {
                        secondary: bridge.secondary,
                        subordinate: bridge.subordinate,
                    },
                );
                Ok(0)
            }
            (Request::Bind, &[domain]) => bind(domain, token).map(|()| 0),
            (Request::Join, &[domain]) => join(domain, token),
            (Request::AllocIrte(route), &[cap]) => {
                require_cap(cap)?;
                alloc_irte(route, payload)
            }
            (Request::FreeIrte(id), &[cap]) => {
                require_cap(cap)?;
                interrupt::free(id).map_err(|err| irq_errno(&err))?;
                Ok(0)
            }
            (Request::MapGrant { grant_fd }, &[domain]) => map_grant(domain, grant_fd, token),
            (Request::UnmapGrant { iova }, &[domain]) => unmap_grant(domain, iova).map(|()| 0),
            _ => Err(Error::new(EINVAL)),
        }
    }
}

/// Makes an interrupt remapping entry for `route`, writes its message into `payload` and
/// returns its ID.
///
/// # Errors
///
/// `EINVAL` when the payload is not [`MsiMessage::BYTES`] long or a bus range leaves out the
/// device's bus, the errno of a failed allocation ([`irq_errno`]), and that of a failed copy
/// (the entry is freed again).
fn alloc_irte(route: MsiRoute, payload: UserSliceRw) -> Result<usize> {
    if payload.len() != MsiMessage::BYTES {
        return Err(Error::new(EINVAL));
    }
    let dev = device(route.device);
    let source = match route.source {
        SourceCheck::Exact => SourceValidation::RequesterId {
            source_id: dev.bdf.source_id(),
            ignore_function_bits: 0,
        },
        SourceCheck::Bus { first, last } if (first..=last).contains(&dev.bdf.bus) => {
            SourceValidation::Bus { first, last }
        }
        _ => return Err(Error::new(EINVAL)),
    };
    let entry = interrupt::alloc_msi(dev, source, route.cpu, route.vector).map_err(|err| {
        if !matches!(err, IrqError::NotEnabled | IrqError::Skip(_)) {
            warn!(
                "IOMMU: cannot make an interrupt remapping entry for {:04x}:{}: {err}",
                dev.segment, dev.bdf
            );
        }
        irq_errno(&err)
    })?;
    let message = MsiMessage {
        address: entry.message.address,
        data: entry.message.data,
    };
    if let Err(err) = payload.copy_exactly(&message.to_bytes()) {
        let _ = interrupt::free(entry.id);
        return Err(err);
    }
    Ok(entry.id as usize)
}

/// Fails unless `cap` is the attach capability.
fn require_cap(cap: usize) -> Result<()> {
    match HANDLES.lock().get(&cap) {
        Some(Handle::AttachCap) => Ok(()),
        Some(_) => Err(Error::new(EACCES)),
        None => Err(Error::new(EBADF)),
    }
}

/// Runs `f` on the domain with handle number `id`.
fn with_domain<T>(
    handles: &mut BTreeMap<usize, Handle>,
    id: usize,
    f: impl FnOnce(&mut DmaSpace, &mut Option<Weak<AddrSpaceWrapper>>) -> Result<T>,
) -> Result<T> {
    match handles.get_mut(&id) {
        Some(Handle::Domain { space, bound }) => f(space, bound),
        Some(Handle::AttachCap) => Err(Error::new(EINVAL)),
        None => Err(Error::new(EBADF)),
    }
}

/// Attaches `device` to domain `id`, with the attach capability `cap`.
fn attach(cap: usize, id: usize, device: Device) -> Result<()> {
    require_cap(cap)?;
    with_domain(&mut HANDLES.lock(), id, |space, _| {
        let domain = space.domain();
        domain.attach(device).map_err(|err| {
            warn!(
                "IOMMU: cannot attach {:04x}:{} to domain {}: {err}",
                device.segment,
                device.bdf,
                domain.id()
            );
            errno(&err)
        })?;
        info!(
            "IOMMU: {:04x}:{} attached to domain {}",
            device.segment,
            device.bdf,
            domain.id()
        );
        Ok(())
    })
}

/// Detaches `device` from domain `id`, with the attach capability `cap`.
fn detach(cap: usize, id: usize, device: Device) -> Result<()> {
    require_cap(cap)?;
    with_domain(&mut HANDLES.lock(), id, |space, _| {
        space.domain().detach(device).map_err(|err| errno(&err))
    })
}

/// Binds domain `id` to the calling process's address space.
fn bind(id: usize, token: &mut CleanLockToken) -> Result<()> {
    let addr_space = AddrSpace::current()?;
    let name = context::current().read(token.token()).name;
    let mut handles = HANDLES.lock();
    let mine = |w: &Weak<AddrSpaceWrapper>| {
        w.strong_count() > 0 && core::ptr::eq(w.as_ptr(), alloc::sync::Arc::as_ptr(&addr_space))
    };
    let already = handles.values().any(|h| match h {
        Handle::Domain { bound: Some(w), .. } => mine(w),
        _ => false,
    });
    with_domain(&mut handles, id, |space, bound| {
        let domain = space.domain();
        if bound.as_ref().is_some_and(|w| w.strong_count() > 0) || already {
            return Err(Error::new(EBUSY));
        }
        if cfg!(iommu = "enforce") && domain.device_count() == 0 {
            warn!(
                "IOMMU: {name} tried to bind domain {} with no device attached",
                domain.id()
            );
            return Err(Error::new(EINVAL));
        }
        if bound.is_none() {
            BOUND.fetch_add(1, Ordering::Relaxed);
        }
        *bound = Some(alloc::sync::Arc::downgrade(&addr_space));
        info!(
            "IOMMU: domain {} ({} device(s)) bound by {name}",
            domain.id(),
            domain.device_count()
        );
        Ok(())
    })
}

/// Moves every device of domain `id` into the domain the calling process has bound, and
/// returns how many there were.
///
/// # Errors
///
/// `EINVAL` when the process has bound no domain or `id` is that domain, `EBUSY` when another
/// live process has bound `id`, `EBADF` when `id` is not open, and the errno of a failed
/// detach or attach; the devices are then put back into `id` (see [`move_devices`]).
fn join(id: usize, token: &mut CleanLockToken) -> Result<usize> {
    let addr_space = AddrSpace::current()?;
    let name = context::current().read(token.token()).name;
    let owner = alloc::sync::Arc::as_ptr(&addr_space);
    let mut handles = HANDLES.lock();
    let target = handles
        .iter()
        .find_map(|(&handle, h)| match h {
            Handle::Domain { bound: Some(w), .. }
                if w.strong_count() > 0 && core::ptr::eq(w.as_ptr(), owner) =>
            {
                Some(handle)
            }
            _ => None,
        })
        .ok_or(Error::new(EINVAL))?;
    if target == id {
        return Err(Error::new(EINVAL));
    }
    let devices = with_domain(&mut handles, id, |space, bound| {
        if bound.as_ref().is_some_and(|w| w.strong_count() > 0) {
            return Err(Error::new(EBUSY));
        }
        Ok(space.domain().devices().collect::<alloc::vec::Vec<_>>())
    })?;
    let (from, into) = move_devices(&mut handles, &devices, id, target)?;
    for device in &devices {
        info!(
            "IOMMU: {:04x}:{} moved from domain {from} into domain {into} (bound by {name})",
            device.segment, device.bdf
        );
    }
    Ok(devices.len())
}

/// Moves `devices`, all of domain handle `from`'s devices, into domain handle `into` as one
/// set, and returns the two domain IDs. Every device leaves `from` before any is attached to
/// `into`: a function and its alias are attached separately, and on AMD-Vi the alias's entry
/// stays with `from` while any of its users is still there, so moving them one at a time
/// would find it busy. When an attach fails, the devices already in `into` are detached and
/// go back into `from` with the rest. A device that cannot be detached from `into` stays
/// there, and that is logged; the attach error is returned either way.
fn move_devices(
    handles: &mut BTreeMap<usize, Handle>,
    devices: &[Device],
    from: usize,
    into: usize,
) -> Result<(u16, u16)> {
    let from_did = with_domain(handles, from, |space, _| {
        let domain = space.domain();
        for (i, &device) in devices.iter().enumerate() {
            if let Err(err) = domain.detach(device) {
                reattach(domain, devices.get(..i).unwrap_or_default());
                return Err(errno(&err));
            }
        }
        Ok(space.id())
    })?;
    let mut moved: usize = 0;
    let attached = with_domain(handles, into, |space, _| {
        let domain = space.domain();
        for &device in devices {
            domain.attach(device).map_err(|err| {
                warn!(
                    "IOMMU: cannot attach {:04x}:{} to domain {}: {err}",
                    device.segment,
                    device.bdf,
                    domain.id()
                );
                errno(&err)
            })?;
            moved = moved.saturating_add(1);
        }
        Ok(domain.id())
    });
    let err = match attached {
        Ok(into_did) => return Ok((from_did, into_did)),
        Err(err) => err,
    };
    let moved = devices.get(..moved).unwrap_or_default();
    // The devices that cannot leave `into` again stay there; only the rest go back.
    let stuck = with_domain(handles, into, |space, _| {
        let mut stuck = Vec::new();
        for &device in moved {
            if let Err(e) = space.domain().detach(device) {
                warn!(
                    "IOMMU: {:04x}:{} cannot leave domain {}: {e}",
                    device.segment,
                    device.bdf,
                    space.id()
                );
                stuck.push(device);
            }
        }
        Ok(stuck)
    })
    .unwrap_or_else(|_| moved.to_vec());
    let back: Vec<Device> = devices
        .iter()
        .copied()
        .filter(|device| !stuck.contains(device))
        .collect();
    let _ = with_domain(handles, from, |space, _| {
        reattach(space.domain(), &back);
        Ok(())
    });
    if !stuck.is_empty() {
        warn!(
            "IOMMU: join into domain handle {into} failed and was not fully undone: {} device(s) stay in it",
            stuck.len()
        );
    }
    Err(err)
}

/// Attaches `devices` back to `domain` after a failed move, warning about any that cannot go
/// back (they stay blocked).
fn reattach(domain: &mut Domain, devices: &[Device]) {
    for &device in devices {
        if let Err(err) = domain.attach(device) {
            warn!(
                "IOMMU: {:04x}:{} stays blocked: it cannot go back into domain {}: {err}",
                device.segment,
                device.bdf,
                domain.id()
            );
        }
    }
}

/// The domain bound by the address space at `owner`, if any.
fn bound_to(
    handles: &mut BTreeMap<usize, Handle>,
    owner: *const AddrSpaceWrapper,
) -> Option<&mut DmaSpace> {
    handles.values_mut().find_map(|h| match h {
        Handle::Domain {
            space,
            bound: Some(w),
        } if w.strong_count() > 0 && core::ptr::eq(w.as_ptr(), owner) => Some(space),
        _ => None,
    })
}

/// What `memory:translation` answers without a bound domain: the physical address (`None`
/// here), or `EACCES` under `kernel.iommu = "enforce"`.
fn unbound() -> Result<Option<usize>> {
    if cfg!(iommu = "enforce") {
        Err(Error::new(EACCES))
    } else {
        Ok(None)
    }
}

/// `memory:translation` for the calling process: the IOVA of the page at `virt` in the domain
/// the process has bound, mapping it first if needed; `None` when the process has bound no
/// domain and gets the physical address instead.
///
/// # Errors
///
/// `EACCES` without a bound domain under `kernel.iommu = "enforce"`; `ENOENT` when the page is
/// not mapped; `ENOMEM` when the IOVA space or memory for tables runs out or a frame cannot be
/// pinned; `EIO` when the unit fails an invalidation.
pub fn translate(virt: VirtualAddress, token: &mut CleanLockToken) -> Result<Option<usize>> {
    if BOUND.load(Ordering::Relaxed) == 0 {
        return unbound();
    }
    let addr_space = AddrSpace::current()?;
    let mut lock_token = token.token();
    // Held throughout, so the page cannot be unmapped (and released) between being found and
    // being mapped. The order, address space before handle table, is the one the release hook
    // takes too.
    let guard = addr_space.acquire_write(lock_token.downgrade());
    let mut handles = HANDLES.lock();
    let Some(space) = bound_to(&mut handles, alloc::sync::Arc::as_ptr(&addr_space)) else {
        return unbound();
    };
    let iova = space.translate(&guard, virt).map_err(|err| {
        if !matches!(err, DmaError::NotMapped { .. }) {
            warn!(
                "IOMMU: cannot map {:#x} in domain {}: {err}",
                virt.data(),
                space.id()
            );
        }
        dma_errno(&err)
    })?;
    usize::try_from(iova).map(Some).map_err(|_| Error::new(EIO))
}

/// The write-removal hook: the address space at `owner` took write access away from `span`
/// (`mprotect`), so the device loses it too for every DMA mapping that memory reaches. Called
/// with the address space locked, so no translation can map the memory again in between.
/// Cheap when no domain is bound.
pub fn narrow(owner: &AddrSpaceWrapper, span: PageSpan) {
    if BOUND.load(Ordering::Relaxed) == 0 || span.is_empty() {
        return;
    }
    let mut handles = HANDLES.lock();
    if let Some(space) = bound_to(&mut handles, owner) {
        space.narrow(
            span.base.start_address().data(),
            span.count.saturating_mul(PAGE_SIZE),
        );
    }
}

/// The release hook: the address space at `owner` unmapped (or moved away) `span`, so the DMA
/// mappings only that memory reached are taken down. Cheap when no domain is bound.
pub fn release(owner: &AddrSpaceWrapper, span: PageSpan) {
    if BOUND.load(Ordering::Relaxed) == 0 || span.is_empty() {
        return;
    }
    let mut handles = HANDLES.lock();
    if let Some(space) = bound_to(&mut handles, owner) {
        space.release(
            span.base.start_address().data(),
            span.count.saturating_mul(PAGE_SIZE),
        );
    }
}

/// The next grant mapping's ticket.
#[cfg(feature = "regbuf")]
static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);

/// The grant behind each grant mapping, by ticket, so [`Request::UnmapGrant`] can drop the
/// grant's record. Taken after the region lock and the handle table, and never held while
/// taking either.
#[cfg(feature = "regbuf")]
static GRANT_LINKS: Mutex<BTreeMap<u64, (super::regbuf::DmaLink, super::regbuf::DmaRecord)>> =
    Mutex::new(BTreeMap::new());

/// The domain with handle number `id`, if the address space at `owner` has bound it.
///
/// # Errors
///
/// `ENODEV` when `owner` has not bound it, `EINVAL` when `id` is the attach capability and
/// `EBADF` when it is not open.
#[cfg(feature = "regbuf")]
fn bound_domain(
    handles: &mut BTreeMap<usize, Handle>,
    id: usize,
    owner: *const AddrSpaceWrapper,
) -> Result<&mut DmaSpace> {
    match handles.get_mut(&id) {
        Some(Handle::Domain {
            space,
            bound: Some(w),
        }) if w.strong_count() > 0 && core::ptr::eq(w.as_ptr(), owner) => Ok(space),
        Some(Handle::Domain { .. }) => Err(Error::new(ENODEV)),
        Some(Handle::AttachCap) => Err(Error::new(EINVAL)),
        None => Err(Error::new(EBADF)),
    }
}

/// [`Request::MapGrant`]: maps the grant behind the caller's descriptor `grant_fd` into domain
/// `id`, which the caller must have bound, and returns its IOVA.
///
/// # Errors
///
/// `ENODEV` when the caller has not bound the domain, `EBADF` and `EINVAL` for a descriptor
/// that is not an open grant, `ECANCELED` once the grant is revoked, `EINVAL` for a grant that
/// is not page-aligned, and the errno of a failed mapping ([`dma_errno`]).
#[cfg(feature = "regbuf")]
fn map_grant(id: usize, grant_fd: u64, token: &mut CleanLockToken) -> Result<usize> {
    let addr_space = AddrSpace::current()?;
    let owner = alloc::sync::Arc::as_ptr(&addr_space);
    let fd = usize::try_from(grant_fd).map_err(|_| Error::new(EBADF))?;
    let grant = super::regbuf::DmaGrant::from_fd(fd, token)?;
    let link = grant.link();
    let grant_id = grant.id();
    let record = grant.map(|frames, writable| {
        let mut handles = HANDLES.lock();
        let space = bound_domain(&mut handles, id, owner)?;
        let ticket = NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
        let iova = space.map_grant(frames, writable, ticket).map_err(|err| {
            warn!(
                "IOMMU: cannot map {grant_id} in domain {}: {err}",
                space.id()
            );
            dma_errno(&err)
        })?;
        info!(
            "IOMMU: domain {}: {grant_id} mapped at IOVA {iova:#x} ({} page(s), {})",
            space.id(),
            frames.len(),
            if writable { "read-write" } else { "read-only" }
        );
        let record = super::regbuf::DmaRecord {
            domain: id,
            iova,
            ticket,
        };
        GRANT_LINKS.lock().insert(ticket, (link, record));
        Ok(record)
    })?;
    usize::try_from(record.iova).map_err(|_| Error::new(EIO))
}

/// Without registered buffers there are no grants to map.
#[cfg(not(feature = "regbuf"))]
fn map_grant(_id: usize, _grant_fd: u64, _token: &mut CleanLockToken) -> Result<usize> {
    Err(Error::new(EINVAL))
}

/// [`Request::UnmapGrant`]: takes the grant mapping at `iova` of domain `id` down strictly, and
/// drops the grant's record of it.
///
/// # Errors
///
/// `ENOENT` when no grant is mapped at `iova`, `EBADF` or `EINVAL` when `id` is not a domain,
/// and `EIO` when the unmap or its invalidation failed (the frames then stay pinned).
#[cfg(feature = "regbuf")]
fn unmap_grant(id: usize, iova: u64) -> Result<()> {
    let ticket = with_domain(&mut HANDLES.lock(), id, |space, _| {
        let ticket = space
            .unmap_grant(iova, None)
            .map_err(|err| dma_errno(&err))?;
        info!(
            "IOMMU: domain {}: grant mapping at IOVA {iova:#x} unmapped by the driver",
            space.id()
        );
        Ok(ticket)
    })?;
    // The region's lock comes before the table's, so take the link out and drop the guard
    // before forgetting the record.
    let link = GRANT_LINKS.lock().remove(&ticket);
    if let Some((link, record)) = link {
        link.forget(record);
    }
    Ok(())
}

/// Without registered buffers no grant is mapped anywhere.
#[cfg(not(feature = "regbuf"))]
fn unmap_grant(_id: usize, _iova: u64) -> Result<()> {
    Err(Error::new(ENOENT))
}

/// Takes down a grant mapping because its grant was revoked or closed: strictly, so when this
/// returns the device's DMA to the IOVA faults. Nothing when the domain has closed (its
/// mappings went with it) or the driver unmapped it already. Called without the region lock.
#[cfg(feature = "regbuf")]
pub fn revoke_grant_mapping(record: super::regbuf::DmaRecord) {
    // Dropped at once: the guard must not be held with the handle table.
    let _ = GRANT_LINKS.lock().remove(&record.ticket);
    let mut handles = HANDLES.lock();
    let Some(Handle::Domain { space, .. }) = handles.get_mut(&record.domain) else {
        return;
    };
    match space.unmap_grant(record.iova, Some(record.ticket)) {
        Ok(_) => {
            info!(
                "IOMMU: domain {}: grant mapping at IOVA {:#x} taken down with its grant, IOTLB invalidated",
                space.id(),
                record.iova
            );
        }
        Err(DmaError::NoGrantMapping { .. }) => {}
        // unmap_grant logged it; the frames stay pinned.
        Err(_) => {}
    }
}
