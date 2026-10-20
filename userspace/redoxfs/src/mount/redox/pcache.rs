//! Ferric Oxide: serving reads of files opened read-only from the kernel's shared page cache
//! (`kernel.page_cache`, io/pagecache-design.md in the project files, PR 2b).
//!
//! When the kernel has a `kernel.pcache` scheme, RedoxFS binds a queue to its scheme socket at
//! mount. Each regular file with a description opened `O_RDONLY` gets a kernel object (its
//! node ID is the object ID), and those descriptions are attached to it, so the kernel serves
//! their `read` and `pread` from the cached pages without a round trip to RedoxFS. Everything
//! else (writers, directories, `mmap`, `fstat`) still comes here.
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
//! Without a `kernel.pcache` scheme nothing here runs and RedoxFS behaves as upstream.

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex, Weak};

use ferric_pcache::abi::{ObjectId, Record, PAGE_SIZE};
use ferric_pcache::client::{self, Call, ClientError, Object, Pump, Queue, FORWARDED_RECORDS};
use ferric_pcache::pump::FailedFill;
use libredox::Fd;
use redox_scheme::Socket;
use syscall::error::EIO;
use syscall::flag::O_RDWR;
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
        // An object RedoxFS closed failed its fills as it went.
        match object {
            Some(object) => object.fill_failed(fill.pages, fill.generation, fill.errno),
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
        log::info!("page cache on: reads of read-only files are served by the kernel");
        Ok(Some(Self {
            queue,
            files: BTreeMap::new(),
            objects,
            attached: BTreeMap::new(),
            writable: BTreeMap::new(),
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
        if self.writable.contains_key(&node_id) {
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
            let object = Arc::new(self.queue.create_object(ObjectId::new(node_id.into()))?);
            object.set_size(size)?;
            self.objects_mut().insert(node_id, Arc::downgrade(&object));
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
                self.forget(node_id);
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

    /// `node_ptr` was mapped or unmapped, and now has `writable` references to mapped ranges
    /// that `Fmap::sync` writes back ([`super::resource::FileMmapInfo::writable_mappings`]).
    /// While there are any the file is not cached: the first closes its object, dropping
    /// every cached page, and its descriptions go back to RedoxFS. At 0, descriptions opened
    /// read-only afterwards are cached again.
    pub fn mappings_changed(&mut self, node_ptr: TreePtr<Node>, writable: usize) {
        let node_id = node_ptr.id();
        if writable == 0 {
            self.writable.remove(&node_id);
        } else if self.writable.insert(node_id, writable).is_none() {
            self.forget(node_id);
        }
    }

    /// Closes `node_id`'s object, failing its outstanding fills and dropping its pages; its
    /// descriptions go back to RedoxFS.
    fn forget(&mut self, node_id: u32) {
        self.objects_mut().remove(&node_id);
        if self.files.remove(&node_id).is_some() {
            self.attached.retain(|_, node| *node != node_id);
        }
    }

    /// The objects the pump sees. A poisoned lock still holds them.
    fn objects_mut(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, Weak<Object>>> {
        self.objects.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Answers the fill records in `records` (forwarded by the pump) with the files' bytes.
    pub fn fill<D: Disk>(&mut self, fs: &mut FileSystem<D>, fmaps: &mut Fmaps, records: &[u8]) {
        for record in Record::decode_all(records) {
            match record {
                Ok(Record::Fill {
                    object,
                    generation,
                    pages,
                }) => {
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
                        Ok(()) => cached.object.filled(pages, generation, &self.buf),
                        Err(err) => {
                            let errno = u16::try_from(err.errno).ok().and_then(NonZeroU16::new);
                            let errno = errno.unwrap_or(NonZeroU16::new(EIO as u16).expect("EIO"));
                            cached.object.fill_failed(pages, generation, errno)
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
