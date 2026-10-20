//! The file system's side of the page cache on Redox: its queue, its objects and the thread
//! that takes fill records off the queue (with the `client` feature, on Redox only).
//!
//! A file system that caches its files in the kernel does this:
//!
//! 1. [`Queue::open`] opens a queue (`None` when the kernel was built without
//!    `kernel.page_cache`, so the file system carries on as it would without one), and
//!    [`Queue::bind`] ties it to the file system's scheme socket.
//! 2. For each file it caches, [`Queue::create_object`] makes an [`Object`];
//!    [`Object::set_size`] gives it the file's size, and [`Object::attach`] each description
//!    that should be served from it, before the open returns to the client.
//! 3. [`spawn_pump`] starts a thread that waits on the queue and hands each batch of records
//!    to the file system through its [`Pump`], which decodes them with
//!    [`Record::decode_all`]. A file system with one request loop forwards them to itself as
//!    a call on its own scheme ([`FORWARDED_RECORDS`]), so the records are answered from the
//!    same thread that owns the file system, in order with the writes it serves. A batch
//!    that cannot be handed on has its fills failed with `EIO` ([`crate::pump`]); if even
//!    that fails, the pump closes the queue, which fails every fill left.
//! 4. It answers each fill with [`Object::filled`] or [`Object::fill_failed`], and after it
//!    has changed a file's bytes itself (a write or truncation through its scheme) calls
//!    [`Object::invalidate`] and [`Object::set_size`] before replying.
//!
//! Closing a description in the client detaches it in the kernel; dropping the [`Object`]
//! ends the cache of the file.

extern crate std;

use alloc::sync::Arc;
use core::fmt;
use core::num::NonZeroU16;
use std::io;
use std::sync::{RwLock, RwLockReadGuard};
use std::thread::{self, JoinHandle};

use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_RDWR};
use syscall::CallFlags;
use syscall::error::{EBADF, ENODEV, ENOENT};

use crate::abi::{
    Generation, ObjectId, Outcome, PageRange, QUEUE_PATH, ReadaheadMax, Record, RecordError,
    RecordPages, Request, SCHEME_NAME,
};
use crate::pump::{FailFillsError, FailedFill, fail_fills};

/// The first metadata word of a call that forwards queue records to the file system's own
/// scheme (see [`spawn_pump`]); the payload is the records' bytes. The value spells
/// `"pcacherc"` so it cannot be mistaken for a small verb of another protocol.
pub const FORWARDED_RECORDS: u64 = u64::from_be_bytes(*b"pcacherc");

/// Records the pump reads from the queue at most at once.
const BATCH: usize = 64;

/// The call that failed, in a [`ClientError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Call {
    /// Opening the queue.
    OpenQueue,
    /// [`Request::Bind`].
    Bind,
    /// [`Request::SetReadaheadMax`].
    SetReadaheadMax,
    /// Opening an object.
    CreateObject,
    /// [`Request::SetSize`].
    SetSize,
    /// [`Request::Attach`].
    Attach,
    /// [`Request::Invalidate`].
    Invalidate,
    /// [`Request::Filled`].
    Filled,
    /// Reading records from the queue.
    ReadQueue,
    /// Handing records on to the file system ([`Pump::forward`]).
    Forward,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OpenQueue => "opening the pcache queue",
            Self::Bind => "binding the pcache queue",
            Self::SetReadaheadMax => "setting the readahead maximum",
            Self::CreateObject => "creating a pcache object",
            Self::SetSize => "setting an object's size",
            Self::Attach => "attaching a description",
            Self::Invalidate => "invalidating pages",
            Self::Filled => "answering a fill",
            Self::ReadQueue => "reading the pcache queue",
            Self::Forward => "forwarding records to the file system",
        })
    }
}

/// Why a page cache call failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClientError {
    /// The kernel refused a call.
    Call {
        /// The call.
        call: Call,
        /// The error it returned.
        source: libredox::error::Error,
    },
    /// The queue gave bytes that are not a record.
    Record(RecordError),
    /// The pump thread could not be started.
    Spawn(io::Error),
    /// The queue was closed (by the pump, after a fill it could not answer).
    Closed,
    /// The pump could not answer the fills of a batch it failed to forward, and closed the
    /// queue.
    Unanswered(alloc::boxed::Box<FailFillsError<ClientError>>),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Call { call, .. } => write!(f, "{call} failed"),
            Self::Record(_) => f.write_str("the pcache queue gave a malformed record"),
            Self::Spawn(_) => f.write_str("the pcache pump thread could not be started"),
            Self::Closed => f.write_str("the pcache queue is closed"),
            Self::Unanswered(_) => {
                f.write_str("fills the pump could not forward went unanswered; queue closed")
            }
        }
    }
}

impl core::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Call { source, .. } => Some(source),
            Self::Record(source) => Some(source),
            Self::Spawn(source) => Some(source),
            Self::Closed => None,
            Self::Unanswered(source) => Some(&**source),
        }
    }
}

impl ClientError {
    /// The errno to give a client when the error ends one of its calls.
    pub fn errno(&self) -> i32 {
        match self {
            Self::Call { source, .. } => source.errno(),
            Self::Record(_) => syscall::error::EIO,
            Self::Spawn(source) => source.raw_os_error().unwrap_or(syscall::error::EIO),
            Self::Closed => EBADF,
            Self::Unanswered(_) => syscall::error::EIO,
        }
    }
}

/// Tags a libredox result with the call it was.
fn tag<T>(call: Call, result: libredox::error::Result<T>) -> Result<T, ClientError> {
    result.map_err(|source| ClientError::Call { call, source })
}

/// Sends `request` (with `payload`) on `fd`.
fn send(fd: &Fd, call: Call, request: Request, payload: &[u8]) -> Result<(), ClientError> {
    let (words, len) = request.encode();
    tag(call, fd.call_wo(payload, CallFlags::empty(), &words[..len])).map(drop)
}

/// A file system's fill queue.
pub struct Queue {
    /// `None` once [`Queue::close`] closed it. The pump's blocking read holds a read lock;
    /// only the pump itself takes the write lock, after its read returned.
    fd: RwLock<Option<Fd>>,
}

impl fmt::Debug for Queue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fd = self.fd.read().ok().and_then(|fd| fd.as_ref().map(Fd::raw));
        f.debug_struct("Queue").field("fd", &fd).finish()
    }
}

/// The queue's descriptor, while it is open.
struct OpenFd<'a>(RwLockReadGuard<'a, Option<Fd>>);

impl OpenFd<'_> {
    fn get(&self) -> Result<&Fd, ClientError> {
        self.0.as_ref().ok_or(ClientError::Closed)
    }
}

impl Queue {
    /// Opens a queue; `None` when the kernel has no `kernel.pcache` scheme.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the scheme is there but refuses the open (it needs root).
    pub fn open() -> Result<Option<Self>, ClientError> {
        let path = alloc::format!("/scheme/{SCHEME_NAME}/{QUEUE_PATH}");
        match Fd::open(&path, O_RDWR | O_CLOEXEC, 0) {
            Ok(fd) => Ok(Some(Self {
                fd: RwLock::new(Some(fd)),
            })),
            Err(err) if err.errno() == ENOENT || err.errno() == ENODEV => Ok(None),
            Err(source) => Err(ClientError::Call {
                call: Call::OpenQueue,
                source,
            }),
        }
    }

    /// Binds the queue to the scheme whose socket is the descriptor `socket`; the queue then
    /// serves only that scheme's descriptions.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses (the queue is bound already, or `socket`
    /// is not a scheme socket).
    pub fn bind(&self, socket: usize) -> Result<(), ClientError> {
        let (words, len) = Request::Bind.encode();
        let fd = self.fd();
        tag(
            Call::Bind,
            fd.get()?.call_wo(
                &socket.to_ne_bytes(),
                CallFlags::FD | CallFlags::FD_CLONE,
                &words[..len],
            ),
        )
        .map(drop)
    }

    /// Sets the largest readahead window of the queue's objects.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses, and [`ClientError::Closed`] after
    /// [`Queue::close`].
    pub fn set_readahead_max(&self, pages: ReadaheadMax) -> Result<(), ClientError> {
        send(
            self.fd().get()?,
            Call::SetReadaheadMax,
            Request::SetReadaheadMax { pages },
            &[],
        )
    }

    /// Creates the object `id`, empty and of size 0.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses (an object `id` exists on this queue, or
    /// there is no memory), and [`ClientError::Closed`] after [`Queue::close`].
    pub fn create_object(&self, id: ObjectId) -> Result<Object, ClientError> {
        let mut path = [0; ObjectId::PATH_MAX];
        let queue = self.fd();
        let fd = tag(
            Call::CreateObject,
            queue
                .get()?
                .openat(id.encode_path(&mut path), O_RDWR | O_CLOEXEC, 0),
        )?;
        Ok(Object { fd })
    }

    /// Closes the queue: the kernel ends its objects, failing every fill still outstanding,
    /// and later calls on it return [`ClientError::Closed`].
    pub fn close(&self) {
        // A poisoned lock still holds the descriptor; close it all the same.
        let mut fd = self
            .fd
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(fd.take());
    }

    /// The descriptor, unless the queue is closed. A poisoned lock still holds it.
    fn fd(&self) -> OpenFd<'_> {
        OpenFd(
            self.fd
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Waits for records and copies whole ones to `buf`; returns the bytes copied, 0 once the
    /// queue has ended.
    fn read(&self, buf: &mut [u8]) -> Result<usize, ClientError> {
        let fd = self.fd();
        tag(Call::ReadQueue, fd.get()?.read(buf))
    }
}

/// One cached file's object.
pub struct Object {
    fd: Fd,
}

impl fmt::Debug for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Object")
            .field("fd", &self.fd.raw())
            .finish()
    }
}

impl Object {
    /// Sets the file's size. Pages past the new end are dropped.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses.
    pub fn set_size(&self, size: u64) -> Result<(), ClientError> {
        send(&self.fd, Call::SetSize, Request::SetSize { size }, &[])
    }

    /// Serves the description with handle number `handle` of the bound scheme from this
    /// object.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses (the handle is attached already).
    pub fn attach(&self, handle: usize) -> Result<(), ClientError> {
        let handle = handle as u64;
        send(&self.fd, Call::Attach, Request::Attach { handle }, &[])
    }

    /// Drops the cached copies of bytes `start..end`, which the file system changed itself;
    /// a fill of them in flight is not taken.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses.
    pub fn invalidate(&self, start: u64, end: u64) -> Result<(), ClientError> {
        PageRange::covering(start, end).try_for_each(|pages| {
            send(
                &self.fd,
                Call::Invalidate,
                Request::Invalidate { pages },
                &[],
            )
        })
    }

    /// Answers the fill record of `pages` in `generation` (both copied from the record) with
    /// their bytes, `pages.count()` whole pages (zero past the end of the file).
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses (the pages are not being filled, the
    /// record was for an earlier object with this ID, the payload has the wrong length, or
    /// there is no memory for it).
    pub fn filled(
        &self,
        pages: RecordPages,
        generation: Generation,
        bytes: &[u8],
    ) -> Result<(), ClientError> {
        let outcome = Outcome::Done;
        send(
            &self.fd,
            Call::Filled,
            Request::Filled {
                pages,
                outcome,
                generation,
            },
            bytes,
        )
    }

    /// Answers the fill record of `pages` in `generation` as failed with `errno`: the readers
    /// waiting for them get `EIO`, and the next read fills again.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses (the pages are not being filled, or the
    /// record was for an earlier object with this ID).
    pub fn fill_failed(
        &self,
        pages: RecordPages,
        generation: Generation,
        errno: NonZeroU16,
    ) -> Result<(), ClientError> {
        let outcome = Outcome::Failed(errno);
        send(
            &self.fd,
            Call::Filled,
            Request::Filled {
                pages,
                outcome,
                generation,
            },
            &[],
        )
    }
}

/// What [`spawn_pump`] hands the file system's records to.
pub trait Pump: Send + 'static {
    /// Hands a batch of whole records to the file system (at most 64).
    ///
    /// # Errors
    ///
    /// Why the batch could not be handed on; the pump then fails its fills.
    fn forward(&mut self, records: &[u8]) -> Result<(), ClientError>;

    /// Answers one fill of a batch [`Pump::forward`] failed on as failed, with
    /// [`Object::fill_failed`] on the object it names. A fill of an object the file system
    /// has closed needs no answer (the close failed it) and is `Ok`.
    ///
    /// # Errors
    ///
    /// The answer's error; the pump then closes the queue.
    fn fail_fill(&mut self, fill: FailedFill) -> Result<(), ClientError>;

    /// Reports that [`Pump::forward`] failed with `error`, after the batch's `answered`
    /// fills were failed with `EIO`. The pump carries on with the next batch.
    fn forward_failed(&mut self, error: ClientError, answered: usize);
}

/// Starts a thread that waits on `queue` and hands each batch of records it reads (whole
/// records, at most 64) to `pump`, until the queue ends. A batch [`Pump::forward`] fails on
/// has every fill in it failed with `EIO` ([`crate::pump::fail_fills`] through
/// [`Pump::fail_fill`]), so no reader waits on a record that was taken off the queue. The
/// thread returns why it stopped.
///
/// # Errors
///
/// [`ClientError::Spawn`] when the thread cannot be started. The thread returns
/// [`ClientError::Unanswered`] after closing the queue when it could not fail a batch's
/// fills, and the error of a queue read that failed.
pub fn spawn_pump<P: Pump>(
    queue: Arc<Queue>,
    mut pump: P,
) -> Result<JoinHandle<Result<(), ClientError>>, ClientError> {
    thread::Builder::new()
        .name("pcache-pump".into())
        .spawn(move || {
            let mut buf = [0_u8; Record::BYTES * BATCH];
            loop {
                let records = match queue.read(&mut buf)? {
                    0 => return Ok(()),
                    n => &buf[..n],
                };
                let Err(error) = pump.forward(records) else {
                    continue;
                };
                match fail_fills(records, |fill| pump.fail_fill(fill)) {
                    Ok(answered) => pump.forward_failed(error, answered),
                    Err(err) => {
                        queue.close();
                        return Err(ClientError::Unanswered(alloc::boxed::Box::new(err)));
                    }
                }
            }
        })
        .map_err(ClientError::Spawn)
}
