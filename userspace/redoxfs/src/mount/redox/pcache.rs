//! Ferric Oxide: serving reads of files opened read-only from the kernel's shared page cache
//! (`kernel.page_cache`, io/pagecache-design.md in the project files, PR 2b).
//!
//! When the kernel has a `kernel.pcache` scheme, RedoxFS binds a queue to its scheme socket at
//! mount. Each regular file with a description opened `O_RDONLY` gets a kernel object (its
//! node ID is the object ID), and those descriptions are attached to it, so the kernel serves
//! their `read` and `pread` from the cached pages without a round trip to RedoxFS. Everything
//! else (writers, directories, `mmap`, `fstat`) still comes here.
//!
//! A file mapped shared and writable is not cached until it is closed and unmapped
//! everywhere: stores through the mapping change the bytes RedoxFS reads without a write
//! call, so cached pages could not follow them. (PR 3 moves mappings to the cache itself.)
//!
//! A miss is a fill record on the queue. A pump thread (`ferric_pcache::client::spawn_pump`)
//! waits on the queue and forwards the records to RedoxFS's own scheme as a call, so the main
//! loop answers them in order with the writes it serves: [`PageCache::fill`] reads the pages
//! through the same path a `read` takes (including mapped buffers) and hands them to the
//! kernel. A write or truncation through the scheme drops the pages it changed
//! ([`PageCache::changed`]) before it replies, so a read after a write sees the new bytes.
//!
//! Without a `kernel.pcache` scheme nothing here runs and RedoxFS behaves as upstream.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::sync::Arc;

use ferric_pcache::abi::{ObjectId, Record, PAGE_SIZE};
use ferric_pcache::client::{self, ClientError, Object, Queue, FORWARDED_RECORDS};
use libredox::flag::{O_CLOEXEC, O_RDONLY};
use libredox::Fd;
use redox_scheme::Socket;
use syscall::error::EIO;
use syscall::flag::MapFlags;
use syscall::CallFlags;

use super::resource::{FileResource, Fmaps, Resource};
use crate::{Disk, FileSystem, Node, TreePtr};

/// One cached file: its object and how many attached descriptions it has.
struct Cached {
    object: Object,
    handles: usize,
}

/// RedoxFS's queue and the objects of its cached files.
pub struct PageCache {
    queue: Arc<Queue>,
    /// Cached files by node ID.
    files: BTreeMap<u32, Cached>,
    /// The node of each attached handle.
    attached: BTreeMap<usize, u32>,
    /// Nodes mapped shared and writable, which are not cached.
    mapped: BTreeSet<u32>,
    /// The buffer fills are read into.
    buf: Vec<u8>,
}

impl PageCache {
    /// Binds a queue to `socket` and starts the pump that forwards its records to the scheme
    /// at `mounted_path`; `None` without a `kernel.pcache` scheme or when that fails (logged).
    pub fn start(socket: &Socket, mounted_path: &str) -> Option<Self> {
        match Self::try_start(socket, mounted_path) {
            Ok(cache) => cache,
            Err(err) => {
                log::warn!("page cache off: {}", Chain(&err));
                None
            }
        }
    }

    fn try_start(socket: &Socket, mounted_path: &str) -> Result<Option<Self>, ClientError> {
        let Some(queue) = Queue::open()? else {
            return Ok(None);
        };
        queue.bind(socket.inner().raw())?;
        let queue = Arc::new(queue);
        let mounted_path = mounted_path.to_string();
        let mut own: Option<Fd> = None;
        client::spawn_pump(Arc::clone(&queue), move |records| {
            // The scheme is registered and served by now: a record means a client read.
            if own.is_none() {
                match Fd::open(&mounted_path, O_RDONLY | O_CLOEXEC, 0) {
                    Ok(fd) => own = Some(fd),
                    Err(err) => {
                        log::error!("page cache: cannot open {mounted_path}: {err}");
                        return Ok(());
                    }
                }
            }
            if let Some(fd) = &own {
                if let Err(err) = fd.call_wo(records, CallFlags::empty(), &[FORWARDED_RECORDS]) {
                    log::error!("page cache: forwarding fills failed: {err}");
                }
            }
            Ok(())
        })?;
        log::info!("page cache on: reads of read-only files are served by the kernel");
        Ok(Some(Self {
            queue,
            files: BTreeMap::new(),
            attached: BTreeMap::new(),
            mapped: BTreeSet::new(),
            buf: Vec::new(),
        }))
    }

    /// Serves the description `handle` of `node_ptr` from the file's object, creating it for
    /// the first such description, when it is a regular file. A failure is logged and leaves
    /// the description to RedoxFS.
    pub fn attach<D: Disk>(
        &mut self,
        fs: &mut FileSystem<D>,
        handle: usize,
        node_ptr: TreePtr<Node>,
    ) {
        let node_id = node_ptr.id();
        if self.mapped.contains(&node_id) {
            return;
        }
        let Ok(node) = fs.tx(|tx| tx.read_tree(node_ptr)) else {
            return;
        };
        if !node.data().is_file() {
            return;
        }
        if let Err(err) = self.try_attach(handle, node.data().size(), node_id) {
            log::warn!("page cache: not caching node {node_id}: {}", Chain(&err));
        }
    }

    fn try_attach(&mut self, handle: usize, size: u64, node_id: u32) -> Result<(), ClientError> {
        if !self.files.contains_key(&node_id) {
            let object = self.queue.create_object(ObjectId::new(node_id.into()))?;
            object.set_size(size)?;
            self.files.insert(node_id, Cached { object, handles: 0 });
        }
        let cached = self
            .files
            .get_mut(&node_id)
            .expect("inserted above when missing");
        cached.object.attach(handle)?;
        cached.handles += 1;
        self.attached.insert(handle, node_id);
        Ok(())
    }

    /// Forgets `handle`, which its client closed (the kernel has detached it); the file's
    /// object goes with its last attached description.
    pub fn closed(&mut self, handle: usize) {
        let Some(node_id) = self.attached.remove(&handle) else {
            return;
        };
        if let Some(cached) = self.files.get_mut(&node_id) {
            cached.handles -= 1;
            if cached.handles == 0 {
                self.files.remove(&node_id);
            }
        }
    }

    /// The size of `node_ptr` before RedoxFS changes it, when the file is cached; pass it to
    /// [`PageCache::changed`] afterwards.
    pub fn size_before<D: Disk>(
        &self,
        fs: &mut FileSystem<D>,
        node_ptr: TreePtr<Node>,
    ) -> Option<u64> {
        if !self.files.contains_key(&node_ptr.id()) {
            return None;
        }
        // If the size cannot be read, the change will fail the same way.
        fs.tx(|tx| tx.read_tree(node_ptr))
            .ok()
            .map(|n| n.data().size())
    }

    /// RedoxFS changed bytes `start..end` of `node_ptr` itself (a write, or a truncation with
    /// `start == end` at the new size), and the file had `before` bytes: drops the pages that
    /// changed from its object and sets its new size. On failure the object is closed, so no
    /// stale page is ever served.
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
                cached
                    .object
                    .invalidate(start.min(before), end.max(size).max(before))
                    .and_then(|()| cached.object.set_size(size))
                    .map_err(|err| log::error!("page cache: node {node_id}: {}", Chain(&err)))
            });
        if result.is_err() {
            self.forget(node_id);
        }
    }

    /// `node_ptr` was mapped with `flags`: a shared writable mapping ends its caching.
    pub fn mapped(&mut self, node_ptr: TreePtr<Node>, flags: MapFlags) {
        if flags.contains(MapFlags::PROT_WRITE) && flags.contains(MapFlags::MAP_SHARED) {
            self.mapped.insert(node_ptr.id());
            self.forget(node_ptr.id());
        }
    }

    /// `node_ptr` has no descriptions or mappings left: it may be cached again.
    pub fn node_closed(&mut self, node_ptr: TreePtr<Node>) {
        self.mapped.remove(&node_ptr.id());
    }

    /// Closes `node_id`'s object; its descriptions go back to RedoxFS.
    fn forget(&mut self, node_id: u32) {
        if self.files.remove(&node_id).is_some() {
            self.attached.retain(|_, node| *node != node_id);
        }
    }

    /// Answers the fill records in `records` (forwarded by the pump) with the files' bytes.
    pub fn fill<D: Disk>(&mut self, fs: &mut FileSystem<D>, fmaps: &mut Fmaps, records: &[u8]) {
        for record in Record::decode_all(records) {
            match record {
                Ok(Record::Fill { object, pages }) => {
                    let Ok(node_id) = u32::try_from(object.get()) else {
                        continue;
                    };
                    // The object may have closed since the record was queued.
                    let Some(cached) = self.files.get(&node_id) else {
                        continue;
                    };
                    let range = pages.pages();
                    let len = range.count() as usize * PAGE_SIZE as usize;
                    self.buf.clear();
                    self.buf.resize(len, 0);
                    let offset = range.first() * PAGE_SIZE;
                    let read = read_pages(fs, fmaps, node_id, offset, &mut self.buf);
                    let answer = match read {
                        Ok(()) => cached.object.filled(pages, &self.buf),
                        Err(err) => {
                            let errno = u16::try_from(err.errno).ok().and_then(NonZeroU16::new);
                            let errno = errno.unwrap_or(NonZeroU16::new(EIO as u16).expect("EIO"));
                            cached.object.fill_failed(pages, errno)
                        }
                    };
                    if let Err(err) = answer {
                        log::warn!("page cache: fill of node {node_id}: {}", Chain(&err));
                    }
                }
                // Writeback, release and access records come with later steps.
                Ok(_) => {}
                Err(err) => log::error!("page cache: bad record from the kernel: {err}"),
            }
        }
    }
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
