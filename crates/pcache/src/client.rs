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
//!    to the file system, which decodes them with [`Record::decode_all`]. A file system with one request loop forwards them to itself as a
//!    call on its own scheme ([`FORWARDED_RECORDS`]), so the records are answered from the
//!    same thread that owns the file system, in order with the writes it serves.
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
use std::thread::{self, JoinHandle};

use libredox::Fd;
use libredox::flag::{O_CLOEXEC, O_RDWR};
use syscall::CallFlags;
use syscall::error::{ENODEV, ENOENT};

use crate::abi::{
    ObjectId, Outcome, PageRange, QUEUE_PATH, ReadaheadMax, Record, RecordError, RecordPages,
    Request, SCHEME_NAME,
};

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
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Call { call, .. } => write!(f, "{call} failed"),
            Self::Record(_) => f.write_str("the pcache queue gave a malformed record"),
            Self::Spawn(_) => f.write_str("the pcache pump thread could not be started"),
        }
    }
}

impl core::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Call { source, .. } => Some(source),
            Self::Record(source) => Some(source),
            Self::Spawn(source) => Some(source),
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
    fd: Fd,
}

impl fmt::Debug for Queue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Queue").field("fd", &self.fd.raw()).finish()
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
            Ok(fd) => Ok(Some(Self { fd })),
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
        tag(
            Call::Bind,
            self.fd.call_wo(
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
    /// [`ClientError::Call`] when the kernel refuses.
    pub fn set_readahead_max(&self, pages: ReadaheadMax) -> Result<(), ClientError> {
        send(
            &self.fd,
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
    /// there is no memory).
    pub fn create_object(&self, id: ObjectId) -> Result<Object, ClientError> {
        let mut path = [0; ObjectId::PATH_MAX];
        let fd = tag(
            Call::CreateObject,
            self.fd
                .openat(id.encode_path(&mut path), O_RDWR | O_CLOEXEC, 0),
        )?;
        Ok(Object { fd })
    }

    /// Waits for records and copies whole ones to `buf`; returns the bytes copied, 0 once the
    /// queue has ended.
    fn read(&self, buf: &mut [u8]) -> Result<usize, ClientError> {
        tag(Call::ReadQueue, self.fd.read(buf))
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

    /// Answers the fill of `pages` with their bytes, `pages.count()` whole pages (zero past
    /// the end of the file).
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses (the pages are not being filled, the
    /// payload has the wrong length, or there is no memory for it).
    pub fn filled(&self, pages: RecordPages, bytes: &[u8]) -> Result<(), ClientError> {
        let outcome = Outcome::Done;
        send(
            &self.fd,
            Call::Filled,
            Request::Filled { pages, outcome },
            bytes,
        )
    }

    /// Answers the fill of `pages` as failed with `errno`: the readers waiting for them get
    /// `EIO`, and the next read fills again.
    ///
    /// # Errors
    ///
    /// [`ClientError::Call`] when the kernel refuses (the pages are not being filled).
    pub fn fill_failed(&self, pages: RecordPages, errno: NonZeroU16) -> Result<(), ClientError> {
        let outcome = Outcome::Failed(errno);
        send(
            &self.fd,
            Call::Filled,
            Request::Filled { pages, outcome },
            &[],
        )
    }
}

/// Starts a thread that waits on `queue` and calls `forward` with each batch of records it
/// reads (whole records, at most 64), until the queue ends or `forward` fails. The thread
/// returns why it stopped.
///
/// # Errors
///
/// [`ClientError::Spawn`] when the thread cannot be started.
pub fn spawn_pump<F>(
    queue: Arc<Queue>,
    mut forward: F,
) -> Result<JoinHandle<Result<(), ClientError>>, ClientError>
where
    F: FnMut(&[u8]) -> Result<(), ClientError> + Send + 'static,
{
    thread::Builder::new()
        .name("pcache-pump".into())
        .spawn(move || {
            let mut buf = [0_u8; Record::BYTES * BATCH];
            loop {
                match queue.read(&mut buf)? {
                    0 => return Ok(()),
                    n => forward(&buf[..n])?,
                }
            }
        })
        .map_err(ClientError::Spawn)
}
