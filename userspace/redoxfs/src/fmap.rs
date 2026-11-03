//! Ferric Oxide: the buffers behind the file mappings RedoxFS serves (`fmap` and `funmap`).
//!
//! The kernel maps a file by asking the file system for memory holding its contents: RedoxFS
//! keeps one buffer per file, [`FileMmapInfo`], sized to the furthest range mapped, and reads
//! each range into it when it is first mapped. The kernel lends the buffer's pages to the
//! process, copy-on-write for a private mapping and directly for a shared one, so stores
//! through a shared writable mapping land in the buffer.
//!
//! Each mapped range is an [`Fmap`] in a range tree, with a count of the mappings that use it
//! (`rc`) and of those that are shared and writable (`writable`). `funmap` writes a range back
//! to the file (up to the file's size) while any shared writable mapping uses it, then drops
//! the reference; `fsync` writes back the ranges still mapped.
//!
//! A buffer lives exactly as long as its file is in use, by an open description or by a
//! mapping. When the last of them goes (`funmap` of the last mapping after the descriptions
//! are closed, or the close of the last description after the mappings are gone),
//! [`release_unused`] drops the file's entry and with it the buffer, after every shared
//! writable store was written back by its `funmap`. Before #373 the buffer was kept until
//! the file was written, so every program and library mapped since boot stayed in memory;
//! removing the entry on close had been tried but leaked, because dropping it never
//! returned the buffer's memory. [`MapBuffer`] now does on drop.
//!
//! While a description stays open the buffer is kept between mappings, so the next mapping
//! of a current range and reads covered by current ranges need not read the file again. A
//! range written since it was read, or kept from before a reopen (the node ID may name a
//! different file by then, see [`FileMmapInfo::invalidate`]), is read again.
//!
//! # Why freeing the buffer cannot leave a process with dangling pages
//!
//! The process never maps RedoxFS's memory by address; it maps the physical frames behind it,
//! and each mapping holds its own reference to every frame (paths in `crates/kernel/src/`):
//!
//! - `scheme/user.rs`, `UserInner::fmap_inner`: the reply to `MmapPrep` is the buffer address.
//!   The kernel builds the grant with `Grant::borrow_fmap` (`context/memory.rs`) from RedoxFS's
//!   page tables before `fmap` returns. Lazy mappings are disabled there
//!   (`mapping_is_lazy = false`), so no page is fetched from RedoxFS later.
//! - A shared mapping takes `add_ref(RefKind::Shared)` on each frame in `borrow_fmap`. A
//!   private one goes through `Grant::copy_mappings` with `CopyMappingsMode::Owned`, which takes
//!   `RefKind::Cow` references. `borrow_fmap` refuses frames without a `PageInfo` (the only ones
//!   it could not count) for private mappings. A shared mapping would map them unreferenced,
//!   but the buffer is anonymous memory from the frame allocator, which always has a
//!   `PageInfo`. `context/reshare.rs` stops a scheme from handing out pages it only borrows.
//! - Unmapping a frame, whether in the process or in RedoxFS, queues
//!   `TlbShootdownActions::FREE`. That runs `handle_free_action` after the TLB shootdown, and it
//!   deallocates the frame only when `remove_ref` leaves no reference. Freeing the buffer
//!   therefore drops only RedoxFS's own references. Frames a process still maps stay allocated
//!   until it unmaps them.
//!
//! RedoxFS receives `funmap` only after the process's pages are gone. `AddrSpace::munmap`
//! (`context/memory.rs`) unmaps the grants and drops its `Flusher` (shootdown and reference
//! release) before `syscall::fs::funmap` calls `handle_notify_files`, which sends `funmap`
//! through `UnmapResult::unmap` and `UserScheme::kfunmap`. Each case then goes as follows:
//!
//! - Unmapping part of a range: `munmap_inner` splits the grant with `Grant::extract`, which
//!   moves `base_offset` to the first page removed. `funmap` names exactly the removed pages,
//!   always whole pages (`UnmapResult::size`), and the rest of the grant keeps its references.
//! - Process exit: `AddrSpace::inner_drop` unmaps every grant and calls
//!   `UnmapResult::unmap` for each, so every mapping still held gets its `funmap`.
//! - `mremap` of a mapping: `AddrSpace::r#move` carries the grant, its frame references and
//!   its file reference to the new address without a `funmap`. Shrinking it, or replacing a
//!   mapping at a fixed destination, unmaps pages, and `syscall::fs::mremap` sends those
//!   `funmap`s through `handle_notify_files`. It used to drop them, which leaked the buffer
//!   and the description and lost shared write-back (fixed with #373).
//! - `mremap` of the buffer by RedoxFS itself, when a mapping needs it to grow: the move
//!   transfers the page-table entries, so the frames are the same ones the processes map.
//! - `MAP_FIXED` over a mapping: `AddrSpace::mmap` unmaps it first and its caller sends the
//!   `funmap`.
//!
//! So [`release_unused`] runs only after every process mapping has been unmapped and shot
//! down. Even a spurious early release would cost only coherence (a later mapping would read
//! the file into a new buffer), never memory safety.

use alloc::collections::BTreeMap;
use core::ops::Range;
use core::ptr::NonNull;
use std::time::{SystemTime, UNIX_EPOCH};

use range_tree::RangeTree;
use syscall::error::{Error, Result, EINVAL};
use syscall::flag::{MapFlags, MAP_SHARED, PROT_WRITE};
use syscall::PAGE_SIZE;

use crate::{Disk, Node, Transaction, TreePtr};

/// The mapping state of every file in use, by node ID.
pub type Fmaps = BTreeMap<u32, FileMmapInfo>;

/// Drops `node_id`'s entry, and with it its mapping buffer, if no open description or
/// mapping uses the file any more. Returns whether the entry was dropped.
///
/// Every shared writable mapping was written back by its `funmap` before its reference went,
/// so nothing in the buffer is newer than the file.
pub fn release_unused(fmaps: &mut Fmaps, node_id: u32) -> bool {
    if fmaps.get(&node_id).is_some_and(|info| !info.in_use()) {
        fmaps.remove(&node_id);
        true
    } else {
        false
    }
}

/// Whether a mapping with `flags` writes into RedoxFS's buffer. A private mapping does not:
/// the kernel copies a page on its first store.
fn writes_back(flags: MapFlags) -> bool {
    flags.contains(PROT_WRITE) && flags.contains(MAP_SHARED)
}

fn now() -> core::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// One mapped range of a file's buffer.
#[derive(Debug)]
pub struct Fmap {
    rc: usize,
    flags: MapFlags,
    last_page_tail: u16,
    version: u64,
    /// How many of the `rc` mappings are shared and writable. Only those can change the
    /// buffer (the kernel maps private ones copy-on-write), so only while there are any is
    /// the range written back to the file, and the page cache leaves it alone.
    writable: usize,
}

impl Fmap {
    fn new(flags: MapFlags, len: u64, version: u64) -> Self {
        Self {
            rc: 1,
            flags,
            last_page_tail: (len % PAGE_SIZE as u64) as u16,
            version,
            writable: usize::from(writes_back(flags)),
        }
    }
}

impl range_tree::Value for Fmap {
    type K = u64;

    // Ranges of different versions are not merged, or a range read before a write would take
    // the newer version of its neighbour and be served as current.
    fn try_merge_forward(self, other: &Self) -> core::result::Result<Self, Self> {
        if self.rc == other.rc
            && self.flags == other.flags
            && self.version == other.version
            && self.writable == other.writable
            && self.last_page_tail == 0
        {
            Ok(self)
        } else {
            Err(self)
        }
    }

    fn try_merge_backwards(self, other: &Self) -> core::result::Result<Self, Self> {
        if self.rc == other.rc
            && self.flags == other.flags
            && self.version == other.version
            && self.writable == other.writable
            && other.last_page_tail == 0
        {
            Ok(self)
        } else {
            Err(self)
        }
    }

    fn split(
        self,
        prev_range: Option<Range<u64>>,
        _range: Range<u64>,
        next_range: Option<Range<u64>>,
    ) -> (Option<Self>, Self, Option<Self>) {
        let part = |last_page_tail| Fmap {
            rc: self.rc,
            flags: self.flags,
            last_page_tail,
            version: self.version,
            writable: self.writable,
        };
        let middle_tail = if next_range.is_none() {
            self.last_page_tail
        } else {
            0
        };
        (
            prev_range.map(|_| part(0)),
            part(middle_tail),
            next_range.map(|_| part(self.last_page_tail)),
        )
    }
}

/// What a [`FileMmapInfo::unmap`] did.
#[derive(Debug)]
#[must_use]
pub struct Unmapped {
    /// Whether a mapping's reference was dropped. A `funmap` of ranges that are no longer
    /// mapped (a process can make the kernel send one) drops nothing and is ignored.
    pub dropped: bool,
    /// The outcome of writing back the ranges whose shared writable mappings went. The
    /// references are dropped even when this failed.
    pub written_back: Result<()>,
}

/// The mapping buffer of one file and the ranges of it that are mapped.
#[derive(Debug)]
pub struct FileMmapInfo {
    buffer: Option<MapBuffer>,
    ranges: RangeTree<Fmap>,
    /// Open descriptions of the file.
    pub open_fds: usize,
    version: u64,
}

impl Default for FileMmapInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl FileMmapInfo {
    /// A file with no buffer, nothing mapped and no open description.
    pub fn new() -> Self {
        Self {
            buffer: None,
            ranges: RangeTree::new(),
            open_fds: 0,
            version: 0,
        }
    }

    /// Marks every cached range out of date, so the next `fmap` of it reads the file again
    /// and reads stop using the cache. Called when an idle node is opened: its id may now
    /// name a different file, as RedoxFS reuses the blocks of deleted nodes.
    pub fn invalidate(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Whether an open description or a mapping uses the file.
    pub fn in_use(&self) -> bool {
        self.open_fds > 0 || self.mappings() > 0
    }

    /// The references to mapped ranges, of any kind; the page cache holds the file's cached
    /// writes while there are any, so the mapped pages see every write.
    pub fn mappings(&self) -> usize {
        self.ranges.iter().map(|(_, fmap)| fmap.rc).sum()
    }

    /// The references to mapped ranges that are written back to the file, those of shared
    /// writable mappings; the page cache leaves the file alone while there are any.
    pub fn writable_mappings(&self) -> usize {
        self.ranges.iter().map(|(_, fmap)| fmap.writable).sum()
    }

    /// Reads `buf.len()` bytes at `offset` from the buffer, if current ranges cover them all.
    /// Returns `Ok(None)` when the file must be read instead. The read stops at the file's
    /// size, since the buffer is page-sized and zeroed past the end of the file.
    ///
    /// # Errors
    ///
    /// Errors reading the node to learn the file's size.
    pub fn read_cached<D: Disk>(
        &self,
        node_ptr: TreePtr<Node>,
        buf: &mut [u8],
        offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<Option<usize>> {
        let Some(buffer) = &self.buffer else {
            return Ok(None);
        };
        let file_size = tx.read_tree(node_ptr)?.data().size();
        if offset >= file_size {
            return Ok(Some(0));
        }
        let len = (file_size - offset).min(buf.len() as u64) as usize;
        let end = offset + len as u64;
        let mut covered = offset;
        for (range, fmap) in self.ranges.conflicts(offset..end) {
            if range.start > covered || fmap.version != self.version {
                return Ok(None);
            }
            covered = covered.max(range.end);
        }
        if covered < end {
            return Ok(None);
        }
        let Some(cached) = buffer.bytes(offset..end) else {
            return Ok(None);
        };
        buf[..len].copy_from_slice(cached);
        Ok(Some(len))
    }

    /// Marks the ranges out of date before a write to the file, so unmapped ones are read
    /// again when next mapped and reads stop using them.
    pub fn begin_write(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Copies `bytes`, just written to the file at `offset`, into the ranges still mapped,
    /// which are the pages processes see, so a live shared mapping and the file stay the
    /// same. Bytes outside the write are left alone, stores through the mapping included.
    /// Unmapped ranges are not updated: the write made them out of date, and the next `fmap`
    /// reads them again.
    pub fn write_through(&mut self, offset: u64, bytes: &[u8]) {
        let Some(end) = offset.checked_add(bytes.len() as u64) else {
            return;
        };
        let Some(buffer) = &mut self.buffer else {
            return;
        };
        for (range, fmap) in self.ranges.conflicts(offset..end) {
            if fmap.rc == 0 {
                continue;
            }
            let start = range.start.max(offset);
            let stop = range.end.min(end);
            if start >= stop {
                continue;
            }
            if let Some(to) = buffer.bytes_mut(start..stop) {
                to.copy_from_slice(&bytes[(start - offset) as usize..(stop - offset) as usize]);
            }
        }
    }

    /// Maps `unaligned_size` bytes of the file at `offset`, which must be page aligned, and
    /// returns the address in RedoxFS's buffer the kernel lends to the process. Ranges not
    /// mapped already, or out of date, are read from the file; the rest take another
    /// reference.
    ///
    /// # Errors
    ///
    /// `EINVAL` if the range does not fit the address space, an allocation error if the
    /// buffer cannot grow, and errors reading the file. On error the mapped ranges are left
    /// as they were.
    pub fn map<D: Disk>(
        &mut self,
        node_ptr: TreePtr<Node>,
        flags: MapFlags,
        unaligned_size: usize,
        offset: u64,
        tx: &mut Transaction<D>,
    ) -> Result<*mut u8> {
        let aligned_size = unaligned_size
            .checked_next_multiple_of(PAGE_SIZE)
            .ok_or(Error::new(EINVAL))?;
        let end = offset
            .checked_add(aligned_size as u64)
            .ok_or(Error::new(EINVAL))?;
        let buffer_len = usize::try_from(end)
            .ok()
            .and_then(|end| end.checked_next_multiple_of(PAGE_SIZE))
            .ok_or(Error::new(EINVAL))?;
        let start = usize::try_from(offset).map_err(|_| Error::new(EINVAL))?;
        self.reserve(buffer_len)?;

        let pieces = self.ranges.remove_and_unused(offset..end);
        if let Err(err) = self.read_pieces(node_ptr, &pieces, tx) {
            for (range, existing) in pieces {
                if let Some(fmap) = existing {
                    self.insert(range, fmap);
                }
            }
            return Err(err);
        }
        for (range, existing) in pieces {
            let fmap = match existing.filter(|fmap| self.is_current(fmap)) {
                Some(mut fmap) => {
                    fmap.rc += 1;
                    fmap.writable += usize::from(writes_back(flags));
                    fmap.flags |= flags;
                    fmap.version = self.version;
                    fmap
                }
                None => Fmap::new(flags, range.end - range.start, self.version),
            };
            self.insert(range, fmap);
        }

        let buffer = self.buffer.as_ref().ok_or(Error::new(EINVAL))?;
        Ok(buffer.as_ptr().wrapping_add(start))
    }

    /// Drops a reference to each range mapped in `offset..offset + size` (rounded up to whole
    /// pages), writing the range back first while a shared writable mapping uses it.
    /// `writes_back` says the mapping going away was shared and writable (the kernel's
    /// `MunmapFlags::NEEDS_SYNC`).
    ///
    /// The buffer itself stays until [`release_unused`] finds the file unused.
    ///
    /// # Errors
    ///
    /// `EINVAL` if the range does not fit in a file offset. Write-back errors are returned
    /// in [`Unmapped::written_back`], after the references are dropped.
    pub fn unmap<D: Disk>(
        &mut self,
        node_ptr: TreePtr<Node>,
        offset: u64,
        size: usize,
        writes_back: bool,
        tx: &mut Transaction<D>,
    ) -> Result<Unmapped> {
        // Mappings are whole pages; the kernel sends page-sized lengths already.
        let end = size
            .checked_next_multiple_of(PAGE_SIZE)
            .and_then(|size| offset.checked_add(size as u64))
            .ok_or(Error::new(EINVAL))?;
        let mut dropped = false;
        let mut written_back = Ok(());
        for (range, mut fmap) in self.ranges.remove(offset..end) {
            // A process can make the kernel send funmap for a range that is no longer mapped
            // (found by ferric-syscall-fuzz). Ignore it, and don't write back: the kept buffer
            // may be older than later writes to the file.
            let Some(rc) = fmap.rc.checked_sub(1) else {
                #[cfg(feature = "log")]
                log::warn!(
                    "funmap of unmapped range {}..{} ignored",
                    range.start,
                    range.end
                );
                self.insert(range, fmap);
                continue;
            };
            let synced = self.write_back(node_ptr, &fmap, range.clone(), tx);
            if written_back.is_ok() {
                written_back = synced;
            }
            fmap.rc = rc;
            if writes_back {
                fmap.writable = fmap.writable.saturating_sub(1);
            }
            dropped = true;
            self.insert(range, fmap);
        }
        Ok(Unmapped {
            dropped,
            written_back,
        })
    }

    /// Writes the ranges still mapped by a shared writable mapping back to the file
    /// (`fsync`). An unmapped one was written back by its funmap, and the file may have been
    /// written since; writing its old bytes again would undo that write.
    ///
    /// # Errors
    ///
    /// The first error writing the file.
    pub fn sync<D: Disk>(&self, node_ptr: TreePtr<Node>, tx: &mut Transaction<D>) -> Result<()> {
        for (range, fmap) in self.ranges.iter().filter(|(_, fmap)| fmap.rc > 0) {
            self.write_back(node_ptr, fmap, range, tx)?;
        }
        Ok(())
    }

    /// The bytes of mapping buffer held, for tests.
    #[cfg(test)]
    fn buffer_len(&self) -> usize {
        self.buffer.as_ref().map_or(0, MapBuffer::len)
    }

    fn is_current(&self, fmap: &Fmap) -> bool {
        fmap.rc > 0 || fmap.version == self.version
    }

    fn reserve(&mut self, len: usize) -> Result<()> {
        match &mut self.buffer {
            Some(buffer) if buffer.len() >= len => Ok(()),
            Some(buffer) => buffer.grow(len),
            None => {
                self.buffer = Some(MapBuffer::new(len)?);
                Ok(())
            }
        }
    }

    /// Reads from the file every piece of a range about to be mapped that is not mapped and
    /// current already. Only unused or out-of-date parts of the buffer are overwritten.
    fn read_pieces<D: Disk>(
        &mut self,
        node_ptr: TreePtr<Node>,
        pieces: &[(Range<u64>, Option<Fmap>)],
        tx: &mut Transaction<D>,
    ) -> Result<()> {
        let version = self.version;
        let buffer = self.buffer.as_mut().ok_or(Error::new(EINVAL))?;
        for (range, existing) in pieces {
            let current = existing
                .as_ref()
                .is_some_and(|fmap| fmap.rc > 0 || fmap.version == version);
            if current {
                continue;
            }
            let buf = buffer.bytes_mut(range.clone()).ok_or(Error::new(EINVAL))?;
            let atime = now();
            let count = tx.read_node(
                node_ptr,
                range.start,
                buf,
                atime.as_secs(),
                atime.subsec_nanos(),
            )?;
            buf[count..].fill(0);
        }
        Ok(())
    }

    /// Writes `range` back to the file if a shared writable mapping uses it, stopping at the
    /// file's size: the buffer is page-sized, and stores past the end of the file do not
    /// extend it.
    fn write_back<D: Disk>(
        &self,
        node_ptr: TreePtr<Node>,
        fmap: &Fmap,
        range: Range<u64>,
        tx: &mut Transaction<D>,
    ) -> Result<()> {
        if fmap.writable == 0 {
            return Ok(());
        }
        let file_size = tx.read_tree(node_ptr)?.data().size();
        let end = range.end.min(file_size);
        if range.start >= end {
            return Ok(());
        }
        let buffer = self.buffer.as_ref().ok_or(Error::new(EINVAL))?;
        let bytes = buffer.bytes(range.start..end).ok_or(Error::new(EINVAL))?;
        let mtime = now();
        tx.write_node(
            node_ptr,
            range.start,
            bytes,
            mtime.as_secs(),
            mtime.subsec_nanos(),
        )?;
        Ok(())
    }

    fn insert(&mut self, range: Range<u64>, fmap: Fmap) {
        // The range was just removed from the tree, so nothing can occupy it.
        if self
            .ranges
            .insert(range.start, range.end - range.start, fmap)
            .is_err()
        {
            #[cfg(feature = "log")]
            log::error!("fmap range {}..{} already occupied", range.start, range.end);
        }
    }
}

impl Drop for FileMmapInfo {
    fn drop(&mut self) {
        if self.in_use() {
            #[cfg(feature = "log")]
            log::error!("FileMmapInfo dropped while in use");
        }
    }
}

/// Page-aligned, zeroed memory that RedoxFS lends to mapping processes, returned on drop.
#[derive(Debug)]
struct MapBuffer {
    ptr: NonNull<u8>,
    len: usize,
}

impl MapBuffer {
    fn new(len: usize) -> Result<Self> {
        Ok(Self {
            ptr: sys::alloc(len)?,
            len,
        })
    }

    fn len(&self) -> usize {
        self.len
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Grows the buffer to `len` bytes, keeping its contents. It may move.
    fn grow(&mut self, len: usize) -> Result<()> {
        // SAFETY: `ptr` and `self.len` describe the live allocation `sys` made, and the
        // pointer is replaced by the one `grow` returns.
        self.ptr = unsafe { sys::grow(self.ptr, self.len, len)? };
        self.len = len;
        Ok(())
    }

    fn bytes(&self, range: Range<u64>) -> Option<&[u8]> {
        let range = self.checked(range)?;
        // SAFETY: the range lies inside the live allocation of `len` bytes at `ptr`.
        Some(unsafe { core::slice::from_raw_parts(self.as_ptr().add(range.start), range.len()) })
    }

    fn bytes_mut(&mut self, range: Range<u64>) -> Option<&mut [u8]> {
        let range = self.checked(range)?;
        // SAFETY: the range lies inside the live allocation of `len` bytes at `ptr`, borrowed
        // mutably through `self`.
        Some(unsafe {
            core::slice::from_raw_parts_mut(self.as_ptr().add(range.start), range.len())
        })
    }

    fn checked(&self, range: Range<u64>) -> Option<Range<usize>> {
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        (start <= end && end <= self.len).then_some(start..end)
    }
}

impl Drop for MapBuffer {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` describe the live allocation `sys` made; nothing uses it
        // after the drop.
        unsafe { sys::free(self.ptr, self.len) }
    }
}

/// Anonymous memory from the kernel, which it can lend to processes as file pages.
#[cfg(target_os = "redox")]
mod sys {
    use core::ptr::NonNull;

    use libredox::call::MmapArgs;
    use syscall::error::{Error, Result, ENOMEM};
    use syscall::flag::{PROT_READ, PROT_WRITE};

    pub fn alloc(len: usize) -> Result<NonNull<u8>> {
        // SAFETY: an anonymous mapping at an address the kernel chooses touches no existing
        // memory.
        let ptr = unsafe {
            libredox::call::mmap(MmapArgs {
                length: len,
                // PRIVATE/SHARED doesn't matter once the pages are passed in the fmap handler.
                prot: libredox::flag::PROT_READ | libredox::flag::PROT_WRITE,
                flags: libredox::flag::MAP_PRIVATE,
                offset: 0,
                fd: !0,
                addr: core::ptr::null_mut(),
            })?
        };
        NonNull::new(ptr.cast()).ok_or(Error::new(ENOMEM))
    }

    /// # Safety
    ///
    /// `ptr` and `old` must describe a live mapping from [`alloc`] or `grow`, which is not
    /// used afterwards.
    pub unsafe fn grow(ptr: NonNull<u8>, old: usize, new: usize) -> Result<NonNull<u8>> {
        // SAFETY: the caller guarantees `ptr..ptr + old` is our mapping; mremap moves or
        // extends it and returns the new address.
        let moved = unsafe {
            syscall::syscall5(
                syscall::SYS_MREMAP,
                ptr.as_ptr() as usize,
                old,
                0,
                new,
                syscall::MremapFlags::empty().bits() | (PROT_READ | PROT_WRITE).bits(),
            )?
        };
        NonNull::new(moved as *mut u8).ok_or(Error::new(ENOMEM))
    }

    /// # Safety
    ///
    /// `ptr` and `len` must describe a live mapping from [`alloc`] or [`grow`], which is not
    /// used afterwards.
    pub unsafe fn free(ptr: NonNull<u8>, len: usize) {
        // SAFETY: the caller guarantees the mapping is ours and unused. Pages the kernel lent
        // to processes keep their own references.
        if let Err(err) = unsafe { libredox::call::munmap(ptr.as_ptr().cast(), len) } {
            log::error!("munmap of fmap buffer failed: {err}");
        }
    }
}

/// Heap memory for host tests, with a per-thread count of the bytes held so a test can check
/// that every buffer is returned.
#[cfg(not(target_os = "redox"))]
mod sys {
    use core::ptr::NonNull;
    use std::alloc::{alloc_zeroed, dealloc, realloc, Layout};
    use std::cell::Cell;

    use syscall::error::{Error, Result, EINVAL, ENOMEM};
    use syscall::PAGE_SIZE;

    thread_local! {
        static HELD: Cell<usize> = const { Cell::new(0) };
    }

    /// The bytes of mapping buffer this thread holds.
    pub fn held() -> usize {
        HELD.with(Cell::get)
    }

    fn layout(len: usize) -> Result<Layout> {
        Layout::from_size_align(len.max(1), PAGE_SIZE).map_err(|_| Error::new(EINVAL))
    }

    pub fn alloc(len: usize) -> Result<NonNull<u8>> {
        let layout = layout(len)?;
        // SAFETY: the layout has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }).ok_or(Error::new(ENOMEM))?;
        HELD.with(|held| held.set(held.get() + len));
        Ok(ptr)
    }

    /// # Safety
    ///
    /// `ptr` and `old` must describe a live allocation from [`alloc`] or `grow`, which is not
    /// used afterwards.
    pub unsafe fn grow(ptr: NonNull<u8>, old: usize, new: usize) -> Result<NonNull<u8>> {
        let new_layout = layout(new)?;
        // SAFETY: the caller guarantees the allocation and its layout; the new size is
        // non-zero.
        let grown = unsafe { realloc(ptr.as_ptr(), layout(old)?, new_layout.size()) };
        let grown = NonNull::new(grown).ok_or(Error::new(ENOMEM))?;
        if new > old {
            // SAFETY: `old..new` lies inside the grown allocation.
            unsafe { grown.as_ptr().add(old).write_bytes(0, new - old) };
        }
        HELD.with(|held| held.set(held.get() - old + new));
        Ok(grown)
    }

    /// # Safety
    ///
    /// `ptr` and `len` must describe a live allocation from [`alloc`] or [`grow`], which is
    /// not used afterwards.
    pub unsafe fn free(ptr: NonNull<u8>, len: usize) {
        if let Ok(layout) = layout(len) {
            // SAFETY: the caller guarantees the allocation and its layout.
            unsafe { dealloc(ptr.as_ptr(), layout) };
            HELD.with(|held| held.set(held.get() - len));
        }
    }
}

#[cfg(test)]
mod tests;
