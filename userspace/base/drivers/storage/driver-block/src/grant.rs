//! Ferric Oxide: registered-buffer grants on disk descriptors (`grant-dma` feature;
//! io/registered-buffers-design.md in the project files, PR 4). The calls are defined in
//! `ferric_blkgrant::abi`.
//!
//! An application attaches a `kernel.regbuf` grant to a slot of its disk descriptor by passing
//! the grant's descriptor; the driver maps it into its DMA domain once ([`GrantDma`]) and then
//! serves `Read`/`Write` calls naming the slot by pointing the device straight at the mapped
//! IOVA range: no bounce buffer, no copy and no per-request mapping of the caller's memory.
//!
//! Before each transfer the driver asks the kernel whether the grant has been revoked
//! (`Info`), and refuses it with `ECANCELED` if so, so the device is not sent to an address the
//! kernel has unmapped. The kernel keeps a revoked grant's IOVA reserved until the driver
//! unmaps it, so even a transfer that races with the revocation faults at the IOMMU instead of
//! reaching memory mapped later.

use std::collections::BTreeMap;
use std::rc::Rc;

use ferric_blkgrant::abi::{Slot, Transfer};
use ferric_blkgrant::slots::{Attached, Direction, GrantSlots, SlotError};
use ferric_regbuf::abi::{GrantInfo, Request as RegbufRequest};
use libredox::Fd;
use syscall::{CallFlags, Error, Result, EACCES, ECANCELED, EINVAL, ENOENT};

/// How a driver maps grants into its DMA domain (`MapGrant`/`UnmapGrant` on the domain it
/// bound, `ferric_iommu::abi`). PCI drivers implement it with `pcid_interface`'s
/// `map_dma_grant` and `unmap_dma_grant`.
pub trait GrantDma {
    /// Maps the grant behind `grant` and returns the IOVA of its first byte.
    ///
    /// # Errors
    ///
    /// `ENODEV` when the driver has no DMA domain, and the kernel's errno otherwise.
    fn map_grant(&self, grant: &Fd) -> Result<u64>;

    /// Unmaps the grant mapping at `iova`, or gives back the IOVA of a revoked one.
    ///
    /// # Errors
    ///
    /// The kernel's errno.
    fn unmap_grant(&self, iova: u64) -> Result<()>;
}

/// The grant slots of every open disk descriptor of a scheme.
pub(crate) struct GrantState {
    dma: Rc<dyn GrantDma>,
    handles: BTreeMap<usize, GrantSlots<Fd>>,
}

/// Where the device must go for a checked transfer.
pub(crate) struct Target {
    /// The IOVA of the transfer's first byte.
    pub iova: u64,
    /// The transfer's length in blocks.
    pub blocks: u32,
}

impl GrantState {
    /// No descriptor has a grant attached yet; grants are mapped with `dma`.
    pub fn new(dma: Rc<dyn GrantDma>) -> Self {
        Self {
            dma,
            handles: BTreeMap::new(),
        }
    }

    /// Attaches `grant` to slot `slot` of descriptor `id`, mapping it into the DMA domain.
    /// A grant the slot held before is unmapped and closed.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a slot out of range or a descriptor that is not a grant, `ECANCELED` for a
    /// revoked grant, and [`GrantDma::map_grant`]'s errors.
    pub fn attach(&mut self, id: usize, slot: u64, grant: Fd) -> Result<()> {
        let slot = Slot::new(slot).ok_or(Error::new(EINVAL))?;
        let info = grant_info(&grant)?;
        if info.revoked {
            return Err(Error::new(ECANCELED));
        }
        let iova = self.dma.map_grant(&grant)?;
        let attached = Attached {
            grant,
            iova,
            len: info.spec.len(),
            writable: info.spec.access().allows_write(),
        };
        log::debug!(
            "grant {} attached to {slot} of handle {id} at IOVA {iova:#x}",
            info.id.get()
        );
        let old = self.handles.entry(id).or_default().attach(slot, attached);
        if let Some(old) = old {
            self.release(old);
        }
        Ok(())
    }

    /// Detaches the grant in slot `slot` of descriptor `id`.
    ///
    /// # Errors
    ///
    /// `ENOENT` when the slot is empty.
    pub fn detach(&mut self, id: usize, slot: Slot) -> Result<()> {
        let attached = self
            .handles
            .get_mut(&id)
            .and_then(|slots| slots.detach(slot))
            .ok_or(Error::new(ENOENT))?;
        self.release(attached);
        Ok(())
    }

    /// Detaches every grant of descriptor `id`, which has closed.
    pub fn close(&mut self, id: usize) {
        let Some(mut slots) = self.handles.remove(&id) else {
            return;
        };
        for attached in slots.take_all() {
            self.release(attached);
        }
    }

    /// Checks `transfer` on descriptor `id` for a disk of `block_size`-byte blocks, and that
    /// its grant is not revoked, and returns where the device must go.
    ///
    /// # Errors
    ///
    /// `ENOENT` for an empty slot, `EINVAL` for a range outside the rules
    /// (`ferric_blkgrant::abi`), `EACCES` for a read into a read-only grant, and `ECANCELED`
    /// once the grant is revoked.
    pub fn resolve(
        &self,
        id: usize,
        transfer: &Transfer,
        direction: Direction,
        block_size: u32,
    ) -> Result<Target> {
        let slots = self
            .handles
            .get(&id)
            .ok_or(slot_errno(SlotError::Empty(transfer.slot)))?;
        let resolved = slots
            .resolve(transfer, direction, block_size)
            .map_err(slot_errno)?;
        if grant_info(resolved.grant)?.revoked {
            return Err(Error::new(ECANCELED));
        }
        Ok(Target {
            iova: resolved.iova,
            blocks: resolved.blocks,
        })
    }

    /// Unmaps a detached grant; dropping it closes the driver's descriptor of it.
    fn release(&self, attached: Attached<Fd>) {
        if let Err(err) = self.dma.unmap_grant(attached.iova) {
            log::warn!("cannot unmap the grant at IOVA {:#x}: {err}", attached.iova);
        }
    }
}

/// The errno a refused transfer returns to the application.
fn slot_errno(err: SlotError) -> Error {
    Error::new(match err {
        SlotError::Empty(_) => ENOENT,
        SlotError::ReadOnly(_) => EACCES,
        _ => EINVAL,
    })
}

/// What the kernel reports about `grant`.
///
/// # Errors
///
/// The kernel's errno for a descriptor that is not a grant, and `EINVAL` for an answer that is
/// not a grant's.
fn grant_info(grant: &Fd) -> Result<GrantInfo> {
    let mut bytes = [0; GrantInfo::BYTES];
    let (words, len) = RegbufRequest::Info.encode();
    grant.call_ro(&mut bytes, CallFlags::empty(), &words[..len])?;
    GrantInfo::from_bytes(&bytes).map_err(|_| Error::new(EINVAL))
}
