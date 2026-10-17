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
//! - Closing the last descriptor of a domain blocks its devices (DMA faults), frees its
//!   tables and unpins every frame mapped in it. Domain descriptors cannot be duplicated into
//!   new descriptions, so the scheme's close is the last close.
//!
//! The call encoding is in the shared `ferric-iommu` crate (`crates/iommu`), which pcid uses
//! too.

use alloc::{collections::BTreeMap, sync::Weak};
use core::sync::atomic::{AtomicUsize, Ordering};

use ferric_iommu::abi::{PciDevice, Request, DOMAIN_PATH};
use ferric_vtd::{dmar::BusRange, plan::SelectError, Bdf};
use spin::Mutex;
use syscall::{
    error::{
        Error, Result, EACCES, EBADF, EBUSY, EEXIST, EINVAL, EIO, ENODEV, ENOENT, ENOMEM, ENOSPC,
        ENXIO,
    },
    flag::CallFlags,
};

use super::{CallerCtx, KernelScheme, OpenResult, StrOrBytes};
use crate::{
    arch::iommu::{
        dma::{DmaError, DmaSpace},
        domain::{self, Device, Domain, DomainError},
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
    }
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
        _payload: UserSliceRw,
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
            _ => Err(Error::new(EINVAL)),
        }
    }
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
/// detach or attach; a device whose attach fails is put back into `id`.
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
    for &device in &devices {
        let (from, into) = move_device(&mut handles, device, id, target)?;
        info!(
            "IOMMU: {:04x}:{} moved from domain {from} into domain {into} (bound by {name})",
            device.segment, device.bdf
        );
    }
    Ok(devices.len())
}

/// Moves `device` from domain handle `from` to domain handle `into`, and returns their
/// domain IDs. When the attach fails, the device goes back into `from`.
fn move_device(
    handles: &mut BTreeMap<usize, Handle>,
    device: Device,
    from: usize,
    into: usize,
) -> Result<(u16, u16)> {
    let from_did = with_domain(handles, from, |space, _| {
        space.domain().detach(device).map_err(|err| errno(&err))?;
        Ok(space.id())
    })?;
    let attached = with_domain(handles, into, |space, _| {
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
        Ok(domain.id())
    });
    match attached {
        Ok(into_did) => Ok((from_did, into_did)),
        Err(err) => {
            let back = with_domain(handles, from, |space, _| {
                space.domain().attach(device).map_err(|e| errno(&e))
            });
            if back.is_err() {
                warn!(
                    "IOMMU: {:04x}:{} stays blocked: it cannot go back into domain {from_did}",
                    device.segment, device.bdf
                );
            }
            Err(err)
        }
    }
}

/// The domain bound by the address space at `owner`, if any.
fn bound_to<'a>(
    handles: &'a mut BTreeMap<usize, Handle>,
    owner: *const AddrSpaceWrapper,
) -> Option<&'a mut DmaSpace> {
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
