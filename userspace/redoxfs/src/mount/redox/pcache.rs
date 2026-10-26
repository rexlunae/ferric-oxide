//! Ferric Oxide: serving reads of files opened read-only from the kernel's shared page cache
//! (`kernel.page_cache`, io/pagecache-design.md in the project files, PR 2b).
//!
//! When the kernel has a `kernel.pcache` scheme, RedoxFS binds a queue to its scheme socket at
//! mount. Each regular file with a description opened `O_RDONLY` (and not `O_FSYNC`) gets a
//! kernel object (its node ID is the object ID), and those descriptions are attached to it,
//! so the kernel serves their `read` and `pread` from the cached pages without a round trip
//! to RedoxFS. Everything else (writers, directories, `mmap`, `fstat`) still comes here.
//!
//! A file with a shared writable mapping is not cached while the mapping lasts: RedoxFS
//! writes such mappings back to the file on `fsync` and `munmap` (`Fmap::sync`), which
//! changes the bytes without a write call, so cached pages could not follow them. (A private
//! mapping never changes RedoxFS's buffer, since the kernel copies a page on its first
//! store, so RedoxFS no longer writes it back, and it does not affect the cache.)
//! [`PageCache::mappings_changed`] keeps a count per file of those mappings, after every
//! `mmap` and `munmap` whatever happens to the descriptors, and closes the file's object
//! (dropping its pages) when the count leaves 0. Descriptions opened read-only once it is
//! back to 0 are cached again. (PR 3 moves mappings to the cache itself.)
//!
//! A miss is a fill record on the queue. A pump thread (`ferric_pcache::client::spawn_pump`)
//! waits on the queue and forwards the records to RedoxFS's own scheme as a call, so the main
//! loop answers them in order with the writes it serves: [`PageCache::fill`] reads the pages
//! through the same path a `read` takes (including mapped buffers) and hands them to the
//! kernel. When a forward fails, the pump fails the batch's fills with `EIO` itself, through
//! the objects it shares with the main loop, and closes the queue if even that fails, so no
//! reader waits on a record nobody will see. A write or truncation through the scheme drops
//! the pages it changed ([`PageCache::changed`]) before it replies, so a read after a write
//! sees the new bytes.
//!
//! When the kernel also has cached writes (`kernel.page_cache_writeback`), RedoxFS turns them
//! on at mount and attaches every description of a regular file, for reading, writing or
//! both as it was opened, except `O_SYNC` (`O_FSYNC`) ones, which it keeps serving itself.
//! A write inside the file's size then lands in the kernel's pages, and RedoxFS writes the
//! dirty pages back with `ferric_pcache::writeback::flush`, in page order:
//!
//! - on `fsync`, before it syncs the file, so `fsync` returns once the data is committed (and
//!   fails with `EIO` if a page could not be written; the page stays dirty);
//! - before it reads the file itself (a read of a description it serves, a mapping), and
//!   before it changes it (a write it serves, a truncation): those also hold the object's
//!   cached writes from before the writeback until the change is done
//!   ([`PageCache::begin_change`], [`PageCache::changed`]), so no cached write slips in
//!   between;
//! - when the last attached description closes, before the object goes;
//! - when a file has been dirty for `DEFAULT_AGE_MS` (5 s): the kernel's writeback record
//!   says it became dirty, and a timer thread wakes the main loop once a second while any
//!   file is ([`PageCache::flush_due`]);
//! - at once, every dirty file oldest first, on an urgent record (the cache is past its dirty
//!   limit);
//! - at unmount ([`PageCache::flush_all`]).
//!
//! A mapped file's cached writes are held while it has any mapping, so the mapped pages,
//! which RedoxFS keeps, see every write. Without cached writes in the kernel nothing of this
//! runs, and only descriptions opened read-only are attached, as before.
//!
//! Without a `kernel.pcache` scheme nothing here runs and RedoxFS behaves as upstream.

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use ferric_pcache::abi::{Access, ObjectId, Outcome, Record, RecordPages, PAGE_SIZE};
use ferric_pcache::client::{
    self, Answered, Call, ClientError, FlushTimer, Object, Pump, Queue, FLUSH_DUE,
    FORWARDED_RECORDS,
};
use ferric_pcache::pump::FailedFill;
use ferric_pcache::writeback::{
    self, DirtyObjects, Flush, Flushed, DEFAULT_AGE_MS, DEFAULT_MAX_DIRTY_PAGES, FLUSH_PAGES,
};
use libredox::Fd;
use redox_scheme::Socket;
use syscall::error::EIO;
use syscall::flag::{O_ACCMODE, O_FSYNC, O_RDONLY, O_RDWR, O_WRONLY};
use syscall::CallFlags;

use super::resource::{FileResource, Fmaps, Resource};
use crate::{Disk, FileSystem, Node, TreePtr};

/// The handle number of the description the pump forwards records through. RedoxFS numbers
/// the descriptions it opens from 1, so only the pump's has this one.
pub const PUMP_HANDLE: usize = 0;

/// Why the page cache could not start.
#[derive(Debug)]
enum StartError {
    /// Opening or binding the queue, or starting the pump, failed.
    Client(ClientError),
    /// The description of this scheme the pump forwards records through could not be made.
    OwnHandle(syscall::Error),
}

impl From<ClientError> for StartError {
    fn from(err: ClientError) -> Self {
        Self::Client(err)
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(_) => f.write_str("cannot start the queue"),
            Self::OwnHandle(_) => f.write_str("cannot make the pump's description"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(err) => Some(err),
            Self::OwnHandle(err) => Some(err),
        }
    }
}

/// One cached file: its object and how many attached descriptions it has.
struct Cached {
    object: Arc<Object>,
    handles: usize,
}

/// The objects of the cached files by node ID, shared with the pump so it can fail the fills
/// of a batch it could not forward. The main loop owns the objects; an entry whose object
/// has closed no longer upgrades.
type Objects = Arc<Mutex<BTreeMap<u32, Weak<Object>>>>;

/// Cached writes: the dirty files, the timer that wakes the loop for them, and the buffer
/// their pages are written back from.
struct Writeback {
    /// Dirty files by node ID, since when.
    dirty: DirtyObjects<u32, Instant>,
    timer: FlushTimer,
    /// [`FLUSH_PAGES`] pages.
    buf: Vec<u8>,
    /// Nodes with a mapping: their cached writes are held while it lasts.
    mapped: BTreeSet<u32>,
}

/// Why a file's dirty pages were not all written back.
#[derive(Debug)]
enum FlushError {
    /// A call on the object failed.
    Client(ClientError),
    /// RedoxFS could not write some of the pages (they stay dirty).
    Write(NonZeroU16),
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(_) => f.write_str("a writeback call failed"),
            Self::Write(errno) => write!(f, "writing dirty pages failed with errno {errno}"),
        }
    }
}

impl std::error::Error for FlushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(err) => Some(err),
            Self::Write(_) => None,
        }
    }
}

/// The error a client gets for a file whose dirty pages could not be written back.
fn flush_errno(_err: &FlushError) -> syscall::Error {
    syscall::Error::new(EIO)
}

/// RedoxFS's queue and the objects of its cached files.
pub struct PageCache {
    queue: Arc<Queue>,
    /// Cached files by node ID.
    files: BTreeMap<u32, Cached>,
    /// The same objects, for the pump.
    objects: Objects,
    /// The node of each attached handle.
    attached: BTreeMap<usize, u32>,
    /// Per node with any, the references of shared writable mappings to its mapped ranges,
    /// which `Fmap::sync` writes back: the node is not cached while it has one.
    writable: BTreeMap<u32, usize>,
    /// The buffer fills are read into.
    buf: Vec<u8>,
    /// Cached writes, when the kernel has them.
    writeback: Option<Writeback>,
}

/// The pump's side: forwards records to RedoxFS's own scheme, and fails their fills when
/// that does not work.
struct RedoxPump {
    /// The description of this scheme numbered [`PUMP_HANDLE`].
    own: Fd,
    objects: Objects,
}

impl Pump for RedoxPump {
    fn forward(&mut self, records: &[u8]) -> Result<(), ClientError> {
        self.own
            .call_wo(records, CallFlags::empty(), &[FORWARDED_RECORDS])
            .map(drop)
            .map_err(|source| ClientError::Call {
                call: Call::Forward,
                source,
            })
    }

    fn fail_fill(&mut self, fill: FailedFill) -> Result<(), ClientError> {
        let object = u32::try_from(fill.object.get()).ok().and_then(|node_id| {
            let objects = self.objects.lock().unwrap_or_else(|p| p.into_inner());
            objects.get(&node_id).and_then(Weak::upgrade)
        });
        // An object RedoxFS closed failed its fills as it went. A fill that ended otherwise,
        // or one of an earlier object with this node's ID, comes back stale, which is fine.
        match object {
            Some(object) => object.fill_failed(fill.fill, fill.errno).map(drop),
            None => Ok(()),
        }
    }

    fn forward_failed(&mut self, error: ClientError, answered: usize) {
        log::error!(
            "page cache: {}; failed its {answered} fill(s) with EIO",
            Chain(&error)
        );
    }
}

impl PageCache {
    /// Binds a queue to `socket` and starts the pump that forwards its records to this
    /// scheme through a description numbered [`PUMP_HANDLE`]; `None` without a
    /// `kernel.pcache` scheme or when that fails (logged).
    pub fn start(socket: &Socket) -> Option<Self> {
        match Self::try_start(socket) {
            Ok(cache) => cache,
            Err(err) => {
                log::warn!("page cache off: {}", Chain(&err));
                None
            }
        }
    }

    fn try_start(socket: &Socket) -> Result<Option<Self>, StartError> {
        let Some(queue) = Queue::open()? else {
            return Ok(None);
        };
        // The pump cannot open the mounted path: RedoxFS enters the null namespace once it
        // is mounted, and the root file system is not reachable by name until then anyway.
        // A description the socket makes of its own scheme needs no namespace.
        let own = socket
            .create_this_scheme_fd(0, PUMP_HANDLE, O_RDWR, 0)
            .map_err(StartError::OwnHandle)?;
        let own = Fd::new(own);
        queue.bind(socket.inner().raw())?;
        let queue = Arc::new(queue);
        let objects = Objects::default();
        let pump = RedoxPump {
            own,
            objects: Arc::clone(&objects),
        };
        let pump = client::spawn_pump(Arc::clone(&queue), pump)?;
        // A pump that stops leaves cached reads waiting, so say why where it can be seen.
        let watched = std::thread::Builder::new()
            .name("pcache-watch".into())
            .spawn(move || match pump.join() {
                Ok(Ok(())) => log::info!("page cache: the queue closed"),
                Ok(Err(err)) => log::error!("page cache: the pump stopped: {}", Chain(&err)),
                Err(_) => log::error!("page cache: the pump panicked"),
            });
        if let Err(err) = watched {
            log::warn!("page cache: cannot watch the pump: {err}");
        }
        let writeback = start_writeback(socket, &queue);
        if writeback.is_some() {
            log::info!("page cache on: reads and writes of files are served by the kernel");
        } else {
            log::info!("page cache on: reads of read-only files are served by the kernel");
        }
        Ok(Some(Self {
            queue,
            files: BTreeMap::new(),
            objects,
            attached: BTreeMap::new(),
            writable: BTreeMap::new(),
            buf: Vec::new(),
            writeback,
        }))
    }

    /// What the kernel should serve of a description opened with `flags`, `None` for one
    /// RedoxFS keeps: never an `O_FSYNC` one; with cached writes, any other, as opened;
    /// without, only one opened read-only, for reading. `O_FSYNC` descriptions staying with
    /// RedoxFS either way is what lets a program (and the tests and benchmarks) reach
    /// RedoxFS itself whether or not the cache is on.
    fn access(&self, flags: usize) -> Option<Access> {
        if flags & O_FSYNC != 0 {
            return None;
        }
        if self.writeback.is_none() {
            return (flags & O_ACCMODE == O_RDONLY).then_some(Access::Read);
        }
        match flags & O_ACCMODE {
            O_RDONLY => Some(Access::Read),
            O_WRONLY => Some(Access::Write),
            O_RDWR => Some(Access::ReadWrite),
            _ => None,
        }
    }

    /// Serves the description `handle` of `node_ptr`, opened with `flags`, from the file's
    /// object, creating it for the first such description, when it is a regular file and
    /// the kernel should serve the description ([`PageCache::access`]). A failure is logged
    /// and leaves the description to RedoxFS.
    pub fn attach<D: Disk>(
        &mut self,
        fs: &mut FileSystem<D>,
        handle: usize,
        node_ptr: TreePtr<Node>,
        flags: usize,
    ) {
        let node_id = node_ptr.id();
        let Some(access) = self.access(flags) else {
            return;
        };
        if self.writable.contains_key(&node_id) {
            return;
        }
        let Ok(node) = fs.tx(|tx| tx.read_tree(node_ptr)) else {
            return;
        };
        if !node.data().is_file() {
            return;
        }
        if let Err(err) = self.try_attach(handle, node.data().size(), node_id, access) {
            log::warn!("page cache: not caching node {node_id}: {}", Chain(&err));
        }
    }

    fn try_attach(
        &mut self,
        handle: usize,
        size: u64,
        node_id: u32,
        access: Access,
    ) -> Result<(), ClientError> {
        if !self.files.contains_key(&node_id) {
            let object = Arc::new(self.queue.create_object(ObjectId::new(node_id.into()))?);
            object.set_size(size)?;
            // A file mapped already keeps its writes with RedoxFS while it is.
            if self
                .writeback
                .as_ref()
                .is_some_and(|wb| wb.mapped.contains(&node_id))
            {
                object.hold_writes(true)?;
            }
            self.objects_mut().insert(node_id, Arc::downgrade(&object));
            self.files.insert(node_id, Cached { object, handles: 0 });
        }
        let cached = self
            .files
            .get_mut(&node_id)
            .expect("inserted above when missing");
        // A read-only attachment is the call a kernel without cached writes knows.
        match access {
            Access::Read => cached.object.attach(handle)?,
            access => cached.object.attach_access(handle, access)?,
        }
        cached.handles += 1;
        self.attached.insert(handle, node_id);
        Ok(())
    }

    /// Forgets `handle`, which its client closed (the kernel has detached it); the file's
    /// object goes with its last attached description, once its dirty pages are written
    /// back. If they cannot be, they are lost with the object (logged), as a failed
    /// writeback at `close` loses them on any system: the node may be freed now.
    pub fn closed<D: Disk>(&mut self, fs: &mut FileSystem<D>, fmaps: &mut Fmaps, handle: usize) {
        let Some(node_id) = self.attached.remove(&handle) else {
            return;
        };
        let Some(cached) = self.files.get_mut(&node_id) else {
            return;
        };
        cached.handles -= 1;
        if cached.handles == 0 {
            if let Err(err) = self.flush(fs, fmaps, node_id) {
                log::error!(
                    "page cache: node {node_id} closed with dirty pages not written: {}",
                    Chain(&err)
                );
            }
            self.forget(node_id);
        }
    }

    /// RedoxFS is about to change `node_ptr` itself (a write or a truncation it serves):
    /// holds the file's cached writes and writes its dirty pages back, so the change works
    /// from the newest bytes and no cached write lands in the middle of it. Returns the size
    /// before the change when the file is cached; pass it to [`PageCache::changed`]
    /// afterwards, which releases the hold.
    ///
    /// # Errors
    ///
    /// `EIO` when dirty pages could not be written back; the hold is released, nothing has
    /// changed, and the change must not be made (it would drop them).
    pub fn begin_change<D: Disk>(
        &mut self,
        fs: &mut FileSystem<D>,
        fmaps: &mut Fmaps,
        node_ptr: TreePtr<Node>,
    ) -> syscall::Result<Option<u64>> {
        if !self.files.contains_key(&node_ptr.id()) {
            return Ok(None);
        }
        self.before_map(fs, fmaps, node_ptr)?;
        // If the size cannot be read, the change will fail the same way.
        Ok(fs
            .tx(|tx| tx.read_tree(node_ptr))
            .ok()
            .map(|n| n.data().size()))
    }

    /// RedoxFS is about to map `node_ptr` (or change it, for [`PageCache::begin_change`]):
    /// with cached writes, holds the file's writes and writes its dirty pages back, so the
    /// mapping shows the newest bytes. [`PageCache::mappings_changed`] then keeps the hold
    /// while the file is mapped, or releases it.
    ///
    /// # Errors
    ///
    /// `EIO` when dirty pages could not be written back; the hold is released.
    pub fn before_map<D: Disk>(
        &mut self,
        fs: &mut FileSystem<D>,
        fmaps: &mut Fmaps,
        node_ptr: TreePtr<Node>,
    ) -> syscall::Result<()> {
        let node_id = node_ptr.id();
        let (Some(_), Some(cached)) = (&self.writeback, self.files.get(&node_id)) else {
            return Ok(());
        };
        if let Err(err) = cached.object.hold_writes(true) {
            // An object the kernel no longer takes calls on is no use: close it.
            log::error!("page cache: node {node_id}: {}", Chain(&err));
            self.forget(node_id);
            return Ok(());
        }
        if let Err(err) = self.flush(fs, fmaps, node_id) {
            log::error!("page cache: node {node_id}: {}", Chain(&err));
            self.release(node_id);
            return Err(flush_errno(&err));
        }
        Ok(())
    }

    /// RedoxFS is about to read `node_ptr` itself (a read it serves, a mapping): writes the
    /// file's dirty pages back first, so it reads the newest bytes.
    ///
    /// # Errors
    ///
    /// `EIO` when they could not be written back.
    pub fn before_read<D: Disk>(
        &mut self,
        fs: &mut FileSystem<D>,
        fmaps: &mut Fmaps,
        node_ptr: TreePtr<Node>,
    ) -> syscall::Result<()> {
        let node_id = node_ptr.id();
        self.flush(fs, fmaps, node_id).map_err(|err| {
            log::error!("page cache: node {node_id}: {}", Chain(&err));
            flush_errno(&err)
        })
    }

    /// Writes `node_ptr`'s dirty pages back, for `fsync`: it then syncs the file.
    ///
    /// # Errors
    ///
    /// `EIO` when some could not be written; they stay dirty for a later writeback.
    pub fn fsync<D: Disk>(
        &mut self,
        fs: &mut FileSystem<D>,
        fmaps: &mut Fmaps,
        node_ptr: TreePtr<Node>,
    ) -> syscall::Result<()> {
        self.before_read(fs, fmaps, node_ptr)
    }

    /// Writes back the files that have been dirty for the age limit, oldest first (the
    /// timer's [`FLUSH_DUE`] call), and stops the timer once no file is dirty.
    pub fn flush_due<D: Disk>(&mut self, fs: &mut FileSystem<D>, fmaps: &mut Fmaps) {
        let Some(wb) = &self.writeback else {
            return;
        };
        let due = Instant::now()
            .checked_sub(Duration::from_millis(DEFAULT_AGE_MS))
            .map_or_else(Vec::new, |before| wb.dirty.due(before));
        self.flush_each(fs, fmaps, &due);
    }

    /// Writes back every dirty file, oldest first: on an urgent record, and at unmount.
    pub fn flush_all<D: Disk>(&mut self, fs: &mut FileSystem<D>, fmaps: &mut Fmaps) {
        let Some(wb) = &self.writeback else {
            return;
        };
        let all = wb.dirty.all();
        self.flush_each(fs, fmaps, &all);
    }

    /// Writes back `nodes` in order, logging failures, and stops the timer if no file is
    /// dirty after.
    fn flush_each<D: Disk>(&mut self, fs: &mut FileSystem<D>, fmaps: &mut Fmaps, nodes: &[u32]) {
        for &node_id in nodes {
            if let Err(err) = self.flush(fs, fmaps, node_id) {
                log::warn!("page cache: writeback of node {node_id}: {}", Chain(&err));
            }
        }
        if let Some(wb) = &self.writeback {
            if wb.dirty.is_empty() {
                wb.timer.disarm();
            }
        }
    }

    /// Writes `node_id`'s dirty pages back, when it is cached with cached writes, and notes
    /// whether it is still dirty after (written to meanwhile, or not all written).
    fn flush<D: Disk>(
        &mut self,
        fs: &mut FileSystem<D>,
        fmaps: &mut Fmaps,
        node_id: u32,
    ) -> Result<(), FlushError> {
        let (Some(wb), Some(cached)) = (self.writeback.as_mut(), self.files.get(&node_id)) else {
            return Ok(());
        };
        let mut target = NodeFlush {
            object: &cached.object,
            fs,
            fmaps,
            node_id,
            buf: &mut wb.buf,
        };
        let flushed = writeback::flush(&mut target);
        match &flushed {
            Ok(Flushed { dirty_left: 0, .. }) => {
                wb.dirty.cleaned(node_id);
            }
            Ok(_) | Err(_) => {
                wb.dirty.dirtied(node_id, Instant::now());
                wb.timer.arm();
            }
        }
        match flushed {
            Ok(Flushed {
                failed: Some(errno),
                ..
            }) => Err(FlushError::Write(errno)),
            Ok(_) => Ok(()),
            Err(err) => Err(FlushError::Client(err)),
        }
    }

    /// Serves `node_id`'s cached writes again after [`PageCache::begin_change`], unless it is
    /// mapped (they stay held while it is).
    fn release(&mut self, node_id: u32) {
        let Some(wb) = &self.writeback else {
            return;
        };
        if wb.mapped.contains(&node_id) {
            return;
        }
        let Some(cached) = self.files.get(&node_id) else {
            return;
        };
        if let Err(err) = cached.object.hold_writes(false) {
            log::error!("page cache: node {node_id}: {}", Chain(&err));
            self.forget(node_id);
        }
    }

    /// RedoxFS changed bytes `start..end` of `node_ptr` itself (a write, or a truncation with
    /// `start == end` at the new size), and the file had `before` bytes: sets the object's
    /// new size (a truncation drops the pages past it, dirty or not), drops the pages that
    /// changed, and serves the file's cached writes again ([`PageCache::begin_change`]). On
    /// failure the object is closed, so no stale page is ever served.
    pub fn changed<D: Disk>(
        &mut self,
        fs: &mut FileSystem<D>,
        node_ptr: TreePtr<Node>,
        before: u64,
        start: u64,
        end: u64,
    ) {
        let node_id = node_ptr.id();
        let Some(cached) = self.files.get(&node_id) else {
            return;
        };
        let result = fs
            .tx(|tx| tx.read_tree(node_ptr))
            .map_err(|err| log::error!("page cache: node {node_id}: {err}"))
            .and_then(|node| {
                let size = node.data().size();
                // Bytes between the old and the new end changed too (zero-filled or cut).
                // The size first: a truncation discards the dirty pages past the new end.
                cached
                    .object
                    .set_size(size)
                    .and_then(|()| {
                        cached
                            .object
                            .invalidate(start.min(before), end.max(size).max(before))
                    })
                    .map_err(|err| log::error!("page cache: node {node_id}: {}", Chain(&err)))
            });
        if result.is_err() {
            self.forget(node_id);
        } else {
            self.release(node_id);
        }
    }

    /// `node_ptr` was mapped or unmapped, and now has `mapped` references to mapped ranges,
    /// `writable` of them ranges that `Fmap::sync` writes back
    /// ([`super::resource::FileMmapInfo::writable_mappings`]). While there are writable ones
    /// the file is not cached: the first closes its object, dropping every cached page (a
    /// mapping is made after [`PageCache::begin_change`], so none is dirty), and its
    /// descriptions go back to RedoxFS. At 0, descriptions opened afterwards are cached
    /// again. With cached writes, the file's writes are held while it has any mapping, so
    /// the mapped pages see them, and served again when the last one goes.
    pub fn mappings_changed(&mut self, node_ptr: TreePtr<Node>, mapped: usize, writable: usize) {
        let node_id = node_ptr.id();
        if let Some(wb) = &mut self.writeback {
            if mapped == 0 {
                wb.mapped.remove(&node_id);
            } else {
                wb.mapped.insert(node_id);
            }
        }
        if writable == 0 {
            self.writable.remove(&node_id);
        } else if self.writable.insert(node_id, writable).is_none() {
            self.forget(node_id);
            return;
        }
        // Held from `begin_change` before the mapping: keep it while mapped.
        if mapped == 0 {
            self.release(node_id);
        }
    }

    /// Closes `node_id`'s object, failing its outstanding fills and dropping its pages; its
    /// descriptions go back to RedoxFS.
    fn forget(&mut self, node_id: u32) {
        self.objects_mut().remove(&node_id);
        if self.files.remove(&node_id).is_some() {
            self.attached.retain(|_, node| *node != node_id);
        }
        if let Some(wb) = &mut self.writeback {
            wb.dirty.cleaned(node_id);
        }
    }

    /// The objects the pump sees. A poisoned lock still holds them.
    fn objects_mut(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, Weak<Object>>> {
        self.objects.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Handles the records in `records` (forwarded by the pump): answers fills with the
    /// files' bytes, and notes the files writeback records say are dirty, writing every
    /// dirty file back at once after an urgent one.
    pub fn fill<D: Disk>(&mut self, fs: &mut FileSystem<D>, fmaps: &mut Fmaps, records: &[u8]) {
        let mut urgent = false;
        for record in Record::decode_all(records) {
            if let Ok(Record::Writeback {
                object,
                urgent: now,
                ..
            }) = record
            {
                urgent |= now;
                self.dirtied(object);
                continue;
            }
            match record.as_ref().map(Record::fill) {
                Ok(Some((object, fill))) => {
                    let Ok(node_id) = u32::try_from(object.get()) else {
                        continue;
                    };
                    // The object may have closed since the record was queued.
                    let Some(cached) = self.files.get(&node_id) else {
                        continue;
                    };
                    let range = fill.pages.pages();
                    let len = range.count() as usize * PAGE_SIZE as usize;
                    self.buf.clear();
                    self.buf.resize(len, 0);
                    let offset = range.first() * PAGE_SIZE;
                    let read = read_pages(fs, fmaps, node_id, offset, &mut self.buf);
                    let answer = match read {
                        Ok(()) => cached.object.filled(fill, &self.buf),
                        Err(err) => {
                            let errno = u16::try_from(err.errno).ok().and_then(NonZeroU16::new);
                            let errno = errno.unwrap_or(NonZeroU16::new(EIO as u16).expect("EIO"));
                            cached.object.fill_failed(fill, errno)
                        }
                    };
                    match answer {
                        Ok(Answered::Taken(_)) => {}
                        Ok(Answered::Stale) => {
                            log::debug!("page cache: fill of node {node_id} was stale");
                        }
                        Err(err) => {
                            log::warn!("page cache: fill of node {node_id}: {}", Chain(&err));
                        }
                    }
                }
                // Release and access records come with later steps.
                Ok(None) => {}
                Err(err) => log::error!("page cache: bad record from the kernel: {err}"),
            }
        }
        if urgent {
            self.flush_all(fs, fmaps);
        }
    }

    /// The kernel says `object` became dirty: writes it back once the age limit has passed.
    fn dirtied(&mut self, object: ObjectId) {
        let (Some(wb), Ok(node_id)) = (&mut self.writeback, u32::try_from(object.get())) else {
            return;
        };
        // Fsync may have cleaned the object while this record waited in the pump.
        // The main loop serializes this query with flush completions. A write after
        // a zero answer posts another record, so it cannot be missed here.
        let Some(cached) = self.files.get(&node_id) else {
            return;
        };
        let pages = match cached.object.dirty_pages() {
            Ok(pages) => pages,
            Err(err) => {
                log::warn!("page cache: checking node {node_id}: {}", Chain(&err));
                // Retain the timer on a query failure; dirty data must get another try.
                1
            }
        };
        if wb.dirty.observe(node_id, pages, Instant::now()) {
            wb.timer.arm();
        } else {
            wb.timer.disarm();
        }
    }
}

/// Turns cached writes on for `queue`, with a timer thread that makes the [`FLUSH_DUE`] call
/// through a description of RedoxFS's own scheme; `None` when the kernel has no cached
/// writes, or when that fails (logged).
fn start_writeback(socket: &Socket, queue: &Queue) -> Option<Writeback> {
    let max = NonZeroU32::new(DEFAULT_MAX_DIRTY_PAGES).expect("the default limit is not 0");
    match queue.set_writeback(max) {
        Ok(true) => {}
        Ok(false) => return None,
        Err(err) => {
            log::warn!("page cache: cached writes off: {}", Chain(&err));
            return None;
        }
    }
    // Once on, every object the queue makes takes writes: a timer that cannot start would
    // leave dirty files to fsync and close alone, so turn them off again.
    let timer = socket
        .create_this_scheme_fd(0, PUMP_HANDLE, O_RDWR, 0)
        .map_err(|err| log::warn!("page cache: cached writes off: no timer description: {err}"))
        .ok()
        .map(Fd::new)
        .and_then(|own| {
            let tick = move || {
                if let Err(err) = own.call_wo(&[], CallFlags::empty(), &[FLUSH_DUE]) {
                    log::warn!("page cache: writeback timer: {err}");
                }
            };
            client::spawn_flush_timer(Duration::from_secs(1), tick)
                .map_err(|err| log::warn!("page cache: cached writes off: {}", Chain(&err)))
                .ok()
        });
    let Some(timer) = timer else {
        // A limit the kernel takes but no description is attached to write with: harmless.
        return None;
    };
    Some(Writeback {
        dirty: DirtyObjects::new(),
        timer,
        buf: vec![0; FLUSH_PAGES as usize * PAGE_SIZE as usize],
        mapped: BTreeSet::new(),
    })
}

/// One file's writeback: the kernel's object and the file its pages go to.
struct NodeFlush<'a, D: Disk> {
    object: &'a Object,
    fs: &'a mut FileSystem<D>,
    fmaps: &'a mut Fmaps,
    node_id: u32,
    /// Holds the run taken.
    buf: &'a mut [u8],
}

impl<D: Disk> Flush for NodeFlush<'_, D> {
    type Error = ClientError;

    fn take(&mut self, from: u64) -> Result<Option<RecordPages>, ClientError> {
        self.object.take_dirty(from, self.buf)
    }

    fn write(&mut self, pages: RecordPages) -> Result<(), NonZeroU16> {
        let range = pages.pages();
        let offset = range.first() * PAGE_SIZE;
        let len = range.count() as usize * PAGE_SIZE as usize;
        let Some(bytes) = self.buf.get(..len) else {
            return Err(NonZeroU16::new(EIO as u16).expect("EIO"));
        };
        write_pages(self.fs, self.fmaps, self.node_id, offset, bytes).map_err(|err| {
            u16::try_from(err.errno)
                .ok()
                .and_then(NonZeroU16::new)
                .unwrap_or(NonZeroU16::new(EIO as u16).expect("EIO"))
        })
    }

    fn answer(&mut self, pages: RecordPages, outcome: Outcome) -> Result<u64, ClientError> {
        self.object.written_back(pages, outcome)
    }
}

/// Writes `bytes`, pages of `node_id` from `offset`, back to the file as a client `write`
/// would, up to the file's end (the bytes past it are the zero tail of its last page).
fn write_pages<D: Disk>(
    fs: &mut FileSystem<D>,
    fmaps: &mut Fmaps,
    node_id: u32,
    offset: u64,
    bytes: &[u8],
) -> syscall::Result<()> {
    let node_ptr: TreePtr<Node> = TreePtr::new(node_id);
    let size = fs.tx(|tx| tx.read_tree(node_ptr))?.data().size();
    let len = usize::try_from(size.saturating_sub(offset))
        .map_or(bytes.len(), |left| left.min(bytes.len()));
    let mut file = FileResource::new(String::new(), None, node_ptr, O_WRONLY, 0);
    let mut done = 0;
    while done < len {
        let at = offset + done as u64;
        let n = fs.tx(|tx| Resource::<D>::write(&mut file, fmaps, &bytes[done..len], at, tx))?;
        if n == 0 {
            return Err(syscall::Error::new(EIO));
        }
        done += n;
    }
    Ok(())
}

/// Reads `buf.len()` bytes of `node_id` from `offset` as a client `read` would, zero past the
/// end of the file.
fn read_pages<D: Disk>(
    fs: &mut FileSystem<D>,
    fmaps: &mut Fmaps,
    node_id: u32,
    offset: u64,
    buf: &mut [u8],
) -> syscall::Result<()> {
    let mut file = FileResource::new(
        String::new(),
        None,
        TreePtr::new(node_id),
        syscall::flag::O_RDONLY,
        0,
    );
    let mut done = 0;
    while done < buf.len() {
        let at = offset + done as u64;
        let n = fs.tx(|tx| Resource::<D>::read(&mut file, fmaps, &mut buf[done..], at, tx))?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(())
}

/// Shows an error and its sources, for a log line.
struct Chain<'a>(&'a dyn std::error::Error);

impl std::fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(err) = source {
            write!(f, ": {err}")?;
            source = err.source();
        }
        Ok(())
    }
}
