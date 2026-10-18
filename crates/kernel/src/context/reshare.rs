//! Ferric Oxide: pages a process only borrows must not be shared onward.
//!
//! Two kinds of grant map another process's frames into a process for a bounded time, and the
//! kernel takes them down when that time ends:
//!
//! - a user scheme's per-request borrow of its caller's buffer (`scheme/user.rs`,
//!   `capture_inner`), unmapped when the scheme answers the request;
//! - a mapping of a regbuf grant (`scheme/regbuf.rs`), unmapped when the region's owner
//!   revokes the grant or closes the region.
//!
//! Both are *pinned borrows*: the holder cannot `munmap`, `mprotect` or `mremap` them, and a
//! fork does not inherit them, so the kernel knows every place their frames are mapped. That
//! only holds if the holder cannot hand the frames to anyone else, which would let them outlive
//! the borrow and escape revocation. This module is the one rule for that: [`check_span`]
//! fails for any span that touches a pinned borrow, and every path that would share a
//! process's pages with another process or a device checks it first:
//!
//! - a scheme answering `mmap` with an address in its own memory (`Grant::borrow_fmap`) or
//!   providing a page with `ProvideMmap` (`scheme/user.rs`);
//! - borrowing another address space's pages through `proc:` (`Grant::borrow`);
//! - registering pages in a regbuf region (`AddrSpaceWrapper::borrow_frame_enforce_rw`);
//! - translating a page to a physical address or an IOVA for DMA (`scheme/memory.rs`).
//!
//! A user scheme that passes a borrowed buffer on in a request of its own (logd writing out a
//! message it was sent, say) is not refused: `capture_inner` copies such a buffer into fresh
//! pages instead of borrowing it again ([`span_has_pinned_borrow`], [`Bounce`]).

use alloc::vec::Vec;
use core::fmt;

use syscall::error::{Error, Result, EINVAL, ENOMEM, EPERM};

use super::memory::{AddrSpace, GrantInfo, PageSpan, Provider};
use crate::{
    memory::{Frame, Page, RaiiFrame, PAGE_SIZE},
    syscall::usercopy::{UserSlice, UserSliceRw},
};

/// Why pages may not be shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReshareError {
    /// The page is part of a pinned borrow of another process's memory.
    PinnedBorrow {
        /// The first byte of the page.
        page: usize,
    },
}

impl fmt::Display for ReshareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PinnedBorrow { page } => {
                write!(f, "page {page:#x} is borrowed from another process")
            }
        }
    }
}

impl core::error::Error for ReshareError {}

impl From<ReshareError> for Error {
    /// `EPERM`, what user space sees for an attempt to share borrowed pages.
    fn from(err: ReshareError) -> Self {
        match err {
            ReshareError::PinnedBorrow { .. } => Error::new(EPERM),
        }
    }
}

/// Whether `info` is a pinned borrow: a per-request scheme borrow or a regbuf grant mapping.
pub fn is_pinned_borrow(info: &GrantInfo) -> bool {
    matches!(
        info.provider,
        Provider::External {
            is_pinned_userscheme_borrow: true,
            ..
        } | Provider::AllocatedShared {
            is_pinned_userscheme_borrow: true,
        }
    )
}

/// Checks that the grant at `base` may be shared.
///
/// # Errors
///
/// [`ReshareError::PinnedBorrow`] when it is a pinned borrow.
pub fn check_grant(base: Page, info: &GrantInfo) -> Result<(), ReshareError> {
    if is_pinned_borrow(info) {
        warn!(
            "refusing to share borrowed pages at {:#x}",
            base.start_address().data()
        );
        return Err(ReshareError::PinnedBorrow {
            page: base.start_address().data(),
        });
    }
    Ok(())
}

/// Checks that no page of `span` in `space` is part of a pinned borrow.
///
/// # Errors
///
/// [`ReshareError::PinnedBorrow`] for the first grant of the span that is one.
pub fn check_span(space: &AddrSpace, span: PageSpan) -> Result<(), ReshareError> {
    space
        .grants
        .conflicts(span)
        .try_for_each(|(base, info)| check_grant(base, info))
}

/// Whether any page of `span` in `space` is part of a pinned borrow, without logging.
pub fn span_has_pinned_borrow(space: &AddrSpace, span: PageSpan) -> bool {
    space
        .grants
        .conflicts(span)
        .any(|(_, info)| is_pinned_borrow(info))
}

/// Fresh pages holding a copy of a user buffer, mapped in place of borrowing it when the buffer
/// is itself borrowed: a scheme's request on a buffer it was lent (`capture_inner`).
#[derive(Default)]
pub struct Bounce {
    frames: Vec<RaiiFrame>,
    /// Where the pages are copied back to when the borrower may have written them.
    dst: Option<UserSliceRw>,
}

impl Bounce {
    /// No pages.
    pub const fn none() -> Self {
        Self {
            frames: Vec::new(),
            dst: None,
        }
    }

    /// Copies the whole pages of `buf` (of the current address space) into fresh frames when
    /// `READ` (the borrower reads them), zeroed otherwise, and remembers `buf` to copy them back
    /// to when `WRITE`.
    ///
    /// # Errors
    ///
    /// `EINVAL` when `buf` is not a whole number of pages, `ENOMEM` when memory runs out and
    /// `EFAULT` when `buf` cannot be read.
    pub fn capture<const READ: bool, const WRITE: bool>(
        buf: UserSlice<READ, WRITE>,
    ) -> Result<Self> {
        if !buf.len().is_multiple_of(PAGE_SIZE) {
            return Err(Error::new(EINVAL));
        }
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(buf.len() / PAGE_SIZE)
            .map_err(|_| Error::new(ENOMEM))?;
        for chunk in buf.in_exact_chunks(PAGE_SIZE) {
            let mut frame = RaiiFrame::allocate().map_err(|_| Error::new(ENOMEM))?;
            if READ {
                chunk
                    .reinterpret_unchecked::<true, false>()
                    .copy_to_slice(frame.bytes_mut())?;
            }
            frames.push(frame);
        }
        Ok(Self {
            frames,
            dst: WRITE.then(|| buf.reinterpret_unchecked()),
        })
    }

    /// The frames, in order, to map.
    pub fn frames(&self) -> Vec<Frame> {
        self.frames.iter().map(RaiiFrame::get).collect()
    }

    /// Whether there are no pages.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Copies the pages back into the buffer they were captured from, if the borrower may have
    /// written them. Call it once the borrower can no longer reach them.
    ///
    /// # Errors
    ///
    /// `EFAULT` when the buffer cannot be written any more.
    pub fn write_back(&mut self) -> Result<()> {
        let Some(dst) = self.dst.take() else {
            return Ok(());
        };
        for (chunk, frame) in dst.in_exact_chunks(PAGE_SIZE).zip(&self.frames) {
            chunk.copy_from_slice(frame.bytes())?;
        }
        Ok(())
    }
}
