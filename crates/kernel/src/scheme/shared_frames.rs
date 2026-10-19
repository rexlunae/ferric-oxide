//! Ferric Oxide: mapping frames the kernel owns into a process, for kernel schemes that share
//! memory with user space as a protocol (`kernel.ring` rings, `kernel.trace` trace rings).

use alloc::{sync::Arc, vec::Vec};
use core::num::NonZeroUsize;

use syscall::{
    error::{Error, Result, EINVAL, ENOMEM},
    flag::MapFlags,
};

use crate::{
    context::memory::{handle_notify_files, AddrSpaceWrapper, Grant, PageSpan},
    memory::{Frame, RaiiFrame},
    sync::CleanLockToken,
};

/// Maps `frames` into `space`, anywhere, with protection `prot`, as one shared grant the
/// process may unmap, and returns its address. The grant holds its own references to the
/// frames, so they outlive the owner while mapped. A grant mapped without `PROT_WRITE` cannot
/// be given it later (`mprotect` only narrows a shared grant).
///
/// # Errors
///
/// `EINVAL` when `frames` is empty, `ENOMEM` when the address space or its page tables have no
/// room.
pub fn map_frames(
    space: &Arc<AddrSpaceWrapper>,
    frames: &[RaiiFrame],
    prot: MapFlags,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let frames: Vec<Frame> = frames.iter().map(RaiiFrame::get).collect();
    let count = NonZeroUsize::new(frames.len()).ok_or(Error::new(EINVAL))?;
    let span = {
        let mut lock_token = token.token();
        let mut guard = space.acquire_write(lock_token.downgrade());
        let base = guard.mmap(
            space,
            None,
            count,
            prot | MapFlags::MAP_SHARED,
            None,
            |base, flags, mapper, flusher| {
                let is_pinned = false;
                Grant::allocated_shared(&frames, base, flags, mapper, flusher, is_pinned)
            },
        )?;
        let pages = guard
            .grants
            .contains(base)
            .map_or(0, |(_, info)| info.page_count());
        PageSpan::new(base, pages)
    };
    if span.count == frames.len() {
        return Ok(span.base.start_address().data());
    }
    // The page tables ran out part way: undo the part that was mapped.
    let notify = space.munmap(span, false, token)?;
    handle_notify_files(notify, token);
    Err(Error::new(ENOMEM))
}
