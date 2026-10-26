//! The `kernel.pcache` scheme's paths, calls and fill-queue records.
//!
//! | Operation | On | Encoding |
//! | --- | --- | --- |
//! | open [`QUEUE_PATH`] | the scheme root (root only) | `open("/scheme/kernel.pcache/queue")` |
//! | [`Request::Bind`] | a queue | `SYS_CALL` with `FD \| WRITE`, the scheme socket, metadata `[verb]` |
//! | open an object path | a queue | `openat(queue, ObjectId::encode_path(..))`: a new, empty object |
//! | [`Request::SetSize`] | an object | `SYS_CALL`, metadata `[verb, size]` |
//! | [`Request::Attach`] | an object | metadata `[verb, handle]` |
//! | [`Request::Detach`] | an object | metadata `[verb, handle]` |
//! | [`Request::Filled`] | an object | metadata `[verb \| count << 16 \| errno << 32, first page, generation, fill]`, payload the pages |
//! | [`Request::WrittenBack`] | an object | metadata `[verb \| count << 16 \| errno << 32, first page]` |
//! | [`Request::SetReadaheadMax`] | a queue | metadata `[verb, pages]` |
//! | [`Request::SetWriteback`] | a queue | metadata `[verb, pages]` |
//! | [`Request::AttachAccess`] | an object | metadata `[verb, handle, access]` |
//! | [`Request::HoldWrites`] | an object | metadata `[verb, 0 or 1]` |
//! | [`Request::TakeDirty`] | an object | `SYS_CALL` with `READ`, metadata `[verb, first page]`, payload room for the pages; returns [`decode_taken`]'s word |
//! | read records | a queue | `read(queue, buf)`: whole [`Record::BYTES`]-byte records |
//!
//! Every call fits the four metadata words the kernel passes a kernel scheme
//! ([`Request::MAX_WORDS`]).
//!
//! A file system opens one queue and binds it to its own scheme with [`Request::Bind`],
//! passing its scheme socket, so the queue can only ever serve that scheme's descriptions.
//! Each object it creates belongs to that queue: the kernel puts the object's [`Record`]s
//! there, and the file system answers each fill record with [`Request::Filled`] on the
//! object's descriptor, which only it holds. The file system
//! picks each object's ID ([`ObjectId`], unique on its queue, for example the file's node
//! number) when it creates the object, so records name objects in its own terms and need no
//! lookup. Opening an ID the queue already has fails with `EEXIST`. An ID may be reused once
//! its object has closed, so each object also has a [`Generation`], which the kernel puts in
//! every record and the file system copies into its answer: an answer to a record of an
//! earlier object with the same ID is stale and ignored (see below), and closing an object
//! drops the records of it still on the queue. Generations are 64 bits and never repeat on a queue: the
//! kernel refuses to open an object once they run out (`EOVERFLOW`), which at a billion opens
//! a second takes centuries.
//!
//! Each fill record also has a [`FillId`], which the file system copies into its answer too.
//! The object numbers its fills from 1 up and never reuses a number (once they run out, a
//! read that needs a fill fails with `EOVERFLOW`), and each page being filled remembers the
//! fill it is waiting for. An answer acts only on the pages its fill
//! still owns: once a fill has failed, been invalidated or been answered, a newer fill of the
//! same pages is not touched by a late answer to the old one, however malformed. An answer
//! whose fill owns none of its pages any more, or whose generation is not the object's, is
//! stale and ignored: the call succeeds and takes nothing (it returns 0). Any other answer
//! returns how many of its pages it acted on. So a file system never has to tell a late
//! answer from a live one, and a stale answer never fails the queue or another fill.
//!
//! Only the process that opened the queue may use it and its objects, binding included: a
//! queue or object descriptor passed to another process is refused there (`EPERM`), so it
//! cannot bind the queue to its own scheme, or attach or fill another client's descriptions. Every
//! thread of that process may, and it keeps the queue across `exec`.
//!
//! [`Request::Attach`] ties a client's open file description to the object: `handle` is the
//! number the file system's scheme gave the client's description. The kernel then serves that
//! description's positioned `read` and `pread` from the object, until the file system
//! detaches it, the client closes it, or the object or its queue closes. Everything else
//! (unpositioned reads, `mmap`, writes unless they are cached (below), `fstat`) still goes to
//! the file system's scheme; serving shared mappings from objects is a later step of the
//! design.
//!
//! # Cached writes
//!
//! With the kernel's `page_cache_writeback` feature a file system may also have the kernel
//! serve writes. [`Request::SetWriteback`] on the queue turns it on (a kernel without the
//! feature refuses it with `EOPNOTSUPP`, and the file system carries on as before), and
//! [`Request::AttachAccess`] attaches a description for writing too. A positioned `write` or
//! `pwrite` of such a description that lies inside the file's size, without `O_APPEND` or
//! `O_FSYNC`, is then copied into the object's pages, which become dirty, and returns with no
//! round trip. Anything else (a write that extends the file, an append, a synchronous
//! description, an object whose writes are held) goes to the file system's scheme as
//! before.
//!
//! The file system writes dirty pages back by pulling them: [`Request::TakeDirty`] marks the
//! next run of dirty pages under writeback and copies their bytes to the caller, who writes
//! them to the file and answers [`Request::WrittenBack`]. One run of an object is under
//! writeback at a time. A page written to meanwhile stays dirty after the answer, and a
//! failed writeback leaves its pages dirty, so data the file system does not have is never
//! dropped, by reclaim or anything else, except by a truncation of the file. The kernel tells
//! the file system when to: a [`Record::Writeback`] when an object's first page becomes
//! dirty, and an urgent one when the queue holds more dirty pages than
//! [`Request::SetWriteback`]'s limit (at twice the limit, writes go to the file system's
//! scheme, which slows writers down to the speed of writeback). The file system flushes an
//! object before it reads or changes the file itself, and holds the object's cached writes
//! ([`Request::HoldWrites`]) from before that flush until it has finished, so a cached write
//! never races one it serves.
//!
//! [`Request::encode`] and [`Request::decode`] are the one place the metadata layout is written
//! down; [`Record::to_bytes`] and [`Record::from_bytes`] the one place the record layout is.

use core::fmt;
use core::num::{NonZeroU16, NonZeroU32};

/// The scheme's number among the kernel's global schemes (`GlobalSchemes` in the kernel's
/// `redox_syscall`), which bootstrap uses to find it among the kernel scheme capabilities it is
/// started with.
pub const SCHEME_ID: u8 = 16;

/// The scheme's name in the namespace: file systems open `/scheme/kernel.pcache`.
pub const SCHEME_NAME: &str = "kernel.pcache";

/// The path that, opened from the scheme root, gives a file system its fill queue.
pub const QUEUE_PATH: &str = "queue";

/// The first component of an object path, opened from a queue ([`ObjectId::encode_path`]).
pub const OBJECT_PREFIX: &str = "object";

/// The size of a cached page, in bytes.
pub const PAGE_SIZE: u64 = 4096;

/// The most pages one fill or writeback record may name (4 MiB), so one answer is bounded.
pub const MAX_RECORD_PAGES: u32 = 1024;

/// A cache object's identifier on its queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(u64);

impl ObjectId {
    /// Object `id`.
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The longest object path, in bytes: `object/` and 16 hex digits.
    pub const PATH_MAX: usize = OBJECT_PREFIX.len() + 1 + 16;

    /// The path that creates this object on a queue: `object/<hex id>`, written into `buf`.
    pub fn encode_path(self, buf: &mut [u8; Self::PATH_MAX]) -> &str {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let prefix = OBJECT_PREFIX.as_bytes();
        buf[..prefix.len()].copy_from_slice(prefix);
        buf[prefix.len()] = b'/';
        let digits = (16 - self.0.leading_zeros() as usize / 4).max(1);
        let start = prefix.len() + 1;
        for (i, slot) in buf[start..start + digits].iter_mut().enumerate() {
            let shift = 4 * (digits - 1 - i);
            *slot = HEX[((self.0 >> shift) & 0xf) as usize];
        }
        // Only ASCII was written.
        core::str::from_utf8(&buf[..start + digits]).unwrap_or(OBJECT_PREFIX)
    }

    /// Parses an object path.
    ///
    /// # Errors
    ///
    /// [`PathError`] when the path is not `object/` followed by 1 to 16 lower-case hex digits.
    pub fn parse_path(path: &[u8]) -> Result<Self, PathError> {
        let digits = path
            .strip_prefix(OBJECT_PREFIX.as_bytes())
            .and_then(|rest| rest.strip_prefix(b"/"))
            .ok_or(PathError)?;
        if digits.is_empty() || digits.len() > 16 {
            return Err(PathError);
        }
        let mut id = 0_u64;
        for &digit in digits {
            let value = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                _ => return Err(PathError),
            };
            id = (id << 4) | u64::from(value);
        }
        Ok(Self(id))
    }
}

/// A path is not a valid object path ([`ObjectId::parse_path`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathError;

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a pcache object path (object/<hex id>)")
    }
}

impl core::error::Error for PathError {}

/// A non-empty run of pages of a file: `count` pages from page `first`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageRange {
    first: u64,
    count: u32,
}

impl PageRange {
    /// `count` pages from `first`, or `None` when `count` is 0 or the range would pass the last
    /// page number.
    pub const fn new(first: u64, count: u32) -> Option<Self> {
        if count == 0 || first.checked_add(count as u64).is_none() {
            return None;
        }
        Some(Self { first, count })
    }

    /// The pages from `first` up to, not including, `end`, or `None` when that is empty or
    /// longer than `u32::MAX` pages.
    pub const fn from_bounds(first: u64, end: u64) -> Option<Self> {
        if end <= first || end - first > u32::MAX as u64 {
            return None;
        }
        Self::new(first, (end - first) as u32)
    }

    /// The first page.
    pub const fn first(self) -> u64 {
        self.first
    }

    /// The number of pages.
    pub const fn count(self) -> u32 {
        self.count
    }

    /// The page after the last one.
    pub const fn end(self) -> u64 {
        // `new` checked that this does not overflow.
        self.first + self.count as u64
    }

    /// Whether `page` is in the range.
    pub const fn contains(self, page: u64) -> bool {
        page >= self.first && page < self.end()
    }

    /// The pages both ranges hold, if any.
    pub fn intersect(self, other: Self) -> Option<Self> {
        Self::from_bounds(self.first.max(other.first), self.end().min(other.end()))
    }

    /// The number of pages that hold `bytes` bytes.
    pub const fn pages_for(bytes: u64) -> u64 {
        bytes.div_ceil(PAGE_SIZE)
    }

    /// The pages that hold bytes `start..end` of a file, in runs of at most `u32::MAX` pages
    /// (none when the range is empty). A file system invalidates these after it changed the
    /// bytes itself.
    pub fn covering(start: u64, end: u64) -> impl Iterator<Item = Self> {
        let mut first = start / PAGE_SIZE;
        let end = if end > start {
            Self::pages_for(end)
        } else {
            first
        };
        core::iter::from_fn(move || {
            let next = end.min(first.saturating_add(u64::from(u32::MAX)));
            let run = Self::from_bounds(first, next)?;
            first = next;
            Some(run)
        })
    }
}

/// A run of pages one fill or writeback record may name: 1 to [`MAX_RECORD_PAGES`] pages.
///
/// [`Record::Fill`], [`Record::Writeback`] and their answers carry this type rather than a
/// [`PageRange`], so a range the wire format would refuse cannot be built. Longer ranges go in
/// several records, [`RecordPages::split`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordPages(PageRange);

impl RecordPages {
    /// `pages`, or `None` when it is longer than [`MAX_RECORD_PAGES`].
    pub const fn new(pages: PageRange) -> Option<Self> {
        if pages.count > MAX_RECORD_PAGES {
            return None;
        }
        Some(Self(pages))
    }

    /// `count` pages from `first`, or `None` when that is empty, overflows or is longer than
    /// [`MAX_RECORD_PAGES`].
    pub const fn from_parts(first: u64, count: u32) -> Option<Self> {
        match PageRange::new(first, count) {
            Some(pages) => Self::new(pages),
            None => None,
        }
    }

    /// The pages.
    pub const fn pages(self) -> PageRange {
        self.0
    }

    /// `pages` in order, cut into runs of at most [`MAX_RECORD_PAGES`] pages.
    pub fn split(pages: PageRange) -> impl Iterator<Item = Self> {
        let max = u64::from(MAX_RECORD_PAGES);
        let mut first = pages.first;
        let end = pages.end();
        core::iter::from_fn(move || {
            let next = end.min(first.saturating_add(max));
            let run = PageRange::from_bounds(first, next).and_then(Self::new)?;
            first = next;
            Some(run)
        })
    }
}

impl From<RecordPages> for PageRange {
    fn from(pages: RecordPages) -> Self {
        pages.0
    }
}

/// A readahead maximum a file system may set: 1 to [`MAX_RECORD_PAGES`] pages, so a window fits
/// in one record ([`crate::readahead::ReadaheadConfig::new`] has the same bound).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReadaheadMax(u32);

impl ReadaheadMax {
    /// `pages`, or `None` when it is 0 or above [`MAX_RECORD_PAGES`].
    pub const fn new(pages: u32) -> Option<Self> {
        if pages == 0 || pages > MAX_RECORD_PAGES {
            return None;
        }
        Some(Self(pages))
    }

    /// The number of pages.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// What the kernel serves of an attached description ([`Request::AttachAccess`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// Reads only, as [`Request::Attach`].
    Read,
    /// Writes only (a description opened `O_WRONLY`).
    Write,
    /// Reads and writes.
    ReadWrite,
}

impl Access {
    /// Whether the kernel serves reads of the description.
    pub const fn reads(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    /// Whether the kernel serves writes of the description.
    pub const fn writes(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }

    const fn to_word(self) -> u64 {
        match self {
            Self::Read => 1,
            Self::Write => 2,
            Self::ReadWrite => 3,
        }
    }

    const fn from_word(word: u64) -> Option<Self> {
        Some(match word {
            1 => Self::Read,
            2 => Self::Write,
            3 => Self::ReadWrite,
            _ => return None,
        })
    }
}

/// The bits of [`Request::TakeDirty`]'s return value that hold the page count.
const TAKEN_COUNT_BITS: u32 = 11;
/// The first page a [`Request::TakeDirty`] return value can name: below 2^52, so the word
/// stays below 2^63 and is never taken for an error return.
const TAKEN_FIRST_LIMIT: u64 = 1 << 52;

/// The return value of a [`Request::TakeDirty`] that took `pages` (`None`: no dirty page was
/// left at or after the first page asked for), `first << 11 | count`, or 0. `None` when the
/// first page is 2^52 or more, which no file of 2^64 bytes has.
pub const fn encode_taken(pages: Option<RecordPages>) -> Option<u64> {
    match pages {
        None => Some(0),
        Some(RecordPages(range)) if range.first < TAKEN_FIRST_LIMIT => {
            Some(range.first << TAKEN_COUNT_BITS | range.count as u64)
        }
        Some(_) => None,
    }
}

/// The pages a [`Request::TakeDirty`] return value names, `None` for 0.
///
/// # Errors
///
/// [`DecodeError::Value`] for a word [`encode_taken`] does not make: a count of 0 with a page,
/// or above [`MAX_RECORD_PAGES`], or a first page of 2^52 or more.
pub fn decode_taken(word: u64) -> Result<Option<RecordPages>, DecodeError> {
    if word == 0 {
        return Ok(None);
    }
    let count = word & ((1 << TAKEN_COUNT_BITS) - 1);
    let first = word >> TAKEN_COUNT_BITS;
    if first >= TAKEN_FIRST_LIMIT {
        return Err(DecodeError::Value);
    }
    record_range(first, count)
        .map(Some)
        .ok_or(DecodeError::Value)
}

/// Which object instance a record is about. The kernel numbers each object it opens on a
/// queue, from 0 up, never reusing a number, so an answer meant for a closed object with the
/// same [`ObjectId`] is told apart from one for the object open now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Generation(u64);

impl Generation {
    /// The generation `raw`.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The generation after this one, or `None` after the last: generations do not wrap, so
    /// the kernel stops opening objects on the queue instead.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Which fill of an object a [`Record::Fill`] and its [`Request::Filled`] answer are about.
/// An object numbers its fills from 1 up and never reuses a number, so an answer to a fill
/// that has ended (failed, invalidated, answered) cannot be taken for a newer fill of the
/// same pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FillId(u64);

impl FillId {
    /// The fill ID `raw`.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The ID after this one, or `None` after the last. At a billion fills a second an
    /// object takes centuries to run out.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// How a file system finished a fill or a writeback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// The pages are filled, or written back.
    Done,
    /// It failed with this errno; the kernel fails waiting readers with `EIO`, whatever it is.
    /// Non-zero, since errno 0 on the wire means [`Self::Done`].
    Failed(NonZeroU16),
}

impl Outcome {
    const fn to_word(self) -> u64 {
        match self {
            Self::Done => 0,
            Self::Failed(errno) => errno.get() as u64,
        }
    }

    const fn from_word(word: u64) -> Option<Self> {
        if word > u16::MAX as u64 {
            return None;
        }
        match NonZeroU16::new(word as u16) {
            None => Some(Self::Done),
            Some(errno) => Some(Self::Failed(errno)),
        }
    }
}

/// A call on a queue or an object, as `SYS_CALL` metadata words.
///
/// Every request fits the three metadata words the kernel passes a kernel scheme. An answer
/// to a record is a call on the object the record named, so it need not repeat the object,
/// and its page count and outcome share the verb's word ([`Self::encode`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// On a queue, as a descriptor-passing call (`CallFlags::FD | CallFlags::WRITE`) carrying
    /// the file system's scheme socket: the queue serves that scheme's descriptions. Only once
    /// per queue, and before any [`Self::Attach`].
    Bind,
    /// On an object: the file's size is now `size` bytes. Pages past it are dropped and
    /// unmapped; a mapping that touches them afterwards gets `SIGBUS`.
    SetSize {
        /// The size in bytes.
        size: u64,
    },
    /// On an object: serve the file system's client description `handle` from this object.
    Attach {
        /// The handle number of the client's description on the file system's scheme.
        handle: u64,
    },
    /// On an object: stop serving description `handle` from it.
    Detach {
        /// The handle number given to [`Self::Attach`].
        handle: u64,
    },
    /// On the object a [`Record::Fill`] named: the answer to it. When the fill is
    /// [`Outcome::Done`], the call's payload is the pages' bytes, whole pages (`count` times
    /// [`PAGE_SIZE`], zero past the end of the file), which the kernel copies into the
    /// object's frames; a failed fill has an empty payload. An answer the kernel cannot take
    /// (no memory for the frames, a payload that faults or has the wrong length) fails the
    /// fill, as a failed answer would: the call returns the error and the record need not be
    /// answered again. The answer acts only on the pages its fill still owns, and returns how
    /// many that was; a stale one (see the module docs) returns 0 and changes nothing.
    Filled {
        /// The pages the record named.
        pages: RecordPages,
        /// How the fill ended.
        outcome: Outcome,
        /// The record's [`Generation`].
        generation: Generation,
        /// The record's [`FillId`].
        fill: FillId,
    },
    /// On an object: the answer to the [`Self::TakeDirty`] that took `pages`, once the file
    /// system has written them to the file ([`Outcome::Done`]) or could not
    /// ([`Outcome::Failed`], which leaves them dirty for a later writeback). A page written
    /// to while it was under writeback stays dirty either way. Returns the number of the
    /// object's pages still dirty, so the file system knows whether to come back. It needs
    /// no generation: it is a call on the object [`Self::TakeDirty`] was, not an answer to a
    /// record. An answer for pages that are not the object's writeback in flight is refused
    /// with `EINVAL`.
    WrittenBack {
        /// The pages [`Self::TakeDirty`] took.
        pages: RecordPages,
        /// How the writeback ended.
        outcome: Outcome,
    },
    /// On a queue: read ahead at most `pages` pages for this file system's objects.
    SetReadaheadMax {
        /// The largest readahead window, in pages.
        pages: ReadaheadMax,
    },
    /// On a queue: serve writes of descriptions attached with an [`Access`] that writes
    /// (see the module docs), with `max_dirty` pages as the queue's dirty limit: past it the
    /// kernel posts urgent [`Record::Writeback`]s, and at twice it sends writes to the file
    /// system's scheme. A kernel without the `page_cache_writeback` feature refuses it with
    /// `EOPNOTSUPP`. A later call changes the limit.
    SetWriteback {
        /// The dirty limit, in pages.
        max_dirty: NonZeroU32,
    },
    /// On an object: serve `handle` of the file system's scheme from this object for
    /// `access`. [`Access::Read`] is [`Self::Attach`]; the others need
    /// [`Self::SetWriteback`] first, and refuse with `EOPNOTSUPP` without it.
    AttachAccess {
        /// The handle number of the client's description on the file system's scheme.
        handle: u64,
        /// What the kernel serves of it.
        access: Access,
    },
    /// On an object: `hold` stops the kernel serving writes from it until a call with `hold`
    /// false (they go to the file system's scheme meanwhile); holds do not nest. A write the
    /// kernel was serving when the hold came has dirtied its pages by the time the call
    /// returns, so a [`Self::TakeDirty`] pass after it sees every cached write.
    HoldWrites {
        /// Hold (true) or serve again (false).
        hold: bool,
    },
    /// On an object: start writing back its next run of dirty pages at or after page `from`,
    /// in page order: at most as many as the call's payload holds whole pages, and at most
    /// [`MAX_RECORD_PAGES`]. Their bytes are copied to the payload (whole pages, zero past
    /// the end of the file), and the call returns the pages ([`encode_taken`]), or 0 when no
    /// page at or after `from` is dirty. The file system answers with
    /// [`Self::WrittenBack`]. `EBUSY` while a run of the object is under writeback already,
    /// and `EINVAL` for a payload smaller than a page.
    TakeDirty {
        /// The first page to look at.
        from: u64,
    },
    /// On an object: the file's bytes in `pages` changed outside the cache (the file system
    /// served a write of them itself). Their cached copies are dropped, and a fill of them in
    /// flight is stale: its answer is not taken, and the readers waiting for it fill again.
    Invalidate {
        /// The pages that changed. Unlike a record, any number of pages: a truncation drops
        /// everything past the new end at once.
        pages: PageRange,
    },
}

#[repr(u64)]
#[derive(Clone, Copy)]
enum Verb {
    SetSize = 1,
    Attach = 2,
    Detach = 3,
    Filled = 4,
    WrittenBack = 5,
    SetReadaheadMax = 6,
    Bind = 7,
    Invalidate = 8,
    SetWriteback = 9,
    AttachAccess = 10,
    HoldWrites = 11,
    TakeDirty = 12,
}

impl Verb {
    const fn from_raw(raw: u64) -> Option<Self> {
        Some(match raw {
            1 => Self::SetSize,
            2 => Self::Attach,
            3 => Self::Detach,
            4 => Self::Filled,
            5 => Self::WrittenBack,
            6 => Self::SetReadaheadMax,
            7 => Self::Bind,
            8 => Self::Invalidate,
            9 => Self::SetWriteback,
            10 => Self::AttachAccess,
            11 => Self::HoldWrites,
            12 => Self::TakeDirty,
            _ => return None,
        })
    }
}

impl Request {
    /// The most metadata words a request takes: what the kernel passes a kernel scheme.
    pub const MAX_WORDS: usize = 4;

    /// The metadata words, and how many of them are used.
    ///
    /// [`Self::Filled`] is `[verb | count << 16 | errno << 32, first page, generation, fill]`:
    /// the verb in the low 16 bits of the first word, the page count in the 16 above it, the
    /// outcome's errno (0 for [`Outcome::Done`]) in the 16 above that, the top 16 zero, the
    /// record's whole [`Generation`] in the third word and its [`FillId`] in the fourth.
    /// [`Self::WrittenBack`] has the same first two words and no more. Every other request
    /// has the verb alone in its first word.
    pub const fn encode(self) -> ([u64; Self::MAX_WORDS], usize) {
        match self {
            Self::Bind => ([Verb::Bind as u64, 0, 0, 0], 1),
            Self::SetSize { size } => ([Verb::SetSize as u64, size, 0, 0], 2),
            Self::Attach { handle } => ([Verb::Attach as u64, handle, 0, 0], 2),
            Self::Detach { handle } => ([Verb::Detach as u64, handle, 0, 0], 2),
            Self::Filled {
                pages,
                outcome,
                generation,
                fill,
            } => (
                [
                    answer_word(Verb::Filled, pages, outcome),
                    pages.0.first,
                    generation.0,
                    fill.0,
                ],
                4,
            ),
            Self::WrittenBack { pages, outcome } => (
                [
                    answer_word(Verb::WrittenBack, pages, outcome),
                    pages.0.first,
                    0,
                    0,
                ],
                2,
            ),
            Self::SetReadaheadMax { pages } => {
                ([Verb::SetReadaheadMax as u64, pages.0 as u64, 0, 0], 2)
            }
            Self::Invalidate { pages } => (
                [Verb::Invalidate as u64, pages.first, pages.count as u64, 0],
                3,
            ),
            Self::SetWriteback { max_dirty } => {
                ([Verb::SetWriteback as u64, max_dirty.get() as u64, 0, 0], 2)
            }
            Self::AttachAccess { handle, access } => {
                ([Verb::AttachAccess as u64, handle, access.to_word(), 0], 3)
            }
            Self::HoldWrites { hold } => ([Verb::HoldWrites as u64, hold as u64, 0, 0], 2),
            Self::TakeDirty { from } => ([Verb::TakeDirty as u64, from, 0, 0], 2),
        }
    }

    /// Decodes the metadata words of a call.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, [`DecodeError::Length`] when the
    /// number of words does not match it, and [`DecodeError::Value`] for a page range that is
    /// empty, overflows or is longer than [`MAX_RECORD_PAGES`], a readahead maximum of 0 or
    /// above [`MAX_RECORD_PAGES`], a dirty limit of 0 or above `u32::MAX`, an unknown
    /// [`Access`], a hold that is not 0 or 1, or bits above the verb that the request does
    /// not have.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let (&head, args) = words.split_first().ok_or(DecodeError::Verb)?;
        let verb = Verb::from_raw(head & VERB_MASK).ok_or(DecodeError::Verb)?;
        let packed = head >> 16;
        if packed != 0 && !matches!(verb, Verb::Filled | Verb::WrittenBack) {
            return Err(DecodeError::Value);
        }
        match (verb, args) {
            (Verb::Bind, &[]) => Ok(Self::Bind),
            (Verb::SetSize, &[size]) => Ok(Self::SetSize { size }),
            (Verb::Attach, &[handle]) => Ok(Self::Attach { handle }),
            (Verb::Detach, &[handle]) => Ok(Self::Detach { handle }),
            (Verb::Filled, &[first, generation, fill]) => {
                let (pages, outcome) = answer(packed, first)?;
                Ok(Self::Filled {
                    pages,
                    outcome,
                    generation: Generation(generation),
                    fill: FillId(fill),
                })
            }
            (Verb::WrittenBack, &[first]) => {
                let (pages, outcome) = answer(packed, first)?;
                Ok(Self::WrittenBack { pages, outcome })
            }
            (Verb::SetReadaheadMax, &[pages]) => u32::try_from(pages)
                .ok()
                .and_then(ReadaheadMax::new)
                .map(|pages| Self::SetReadaheadMax { pages })
                .ok_or(DecodeError::Value),
            (Verb::Invalidate, &[first, count]) => u32::try_from(count)
                .ok()
                .and_then(|count| PageRange::new(first, count))
                .map(|pages| Self::Invalidate { pages })
                .ok_or(DecodeError::Value),
            (Verb::SetWriteback, &[pages]) => u32::try_from(pages)
                .ok()
                .and_then(NonZeroU32::new)
                .map(|max_dirty| Self::SetWriteback { max_dirty })
                .ok_or(DecodeError::Value),
            (Verb::AttachAccess, &[handle, access]) => Access::from_word(access)
                .map(|access| Self::AttachAccess { handle, access })
                .ok_or(DecodeError::Value),
            (Verb::HoldWrites, &[hold]) => match hold {
                0 => Ok(Self::HoldWrites { hold: false }),
                1 => Ok(Self::HoldWrites { hold: true }),
                _ => Err(DecodeError::Value),
            },
            (Verb::TakeDirty, &[from]) => Ok(Self::TakeDirty { from }),
            _ => Err(DecodeError::Length),
        }
    }
}

/// The bits of a request's first word that hold its verb.
const VERB_MASK: u64 = 0xffff;

/// The first word of a [`Request::Filled`] or [`Request::WrittenBack`]: the verb, the count
/// above it and the errno above that. A count fits 16 bits, being at most
/// [`MAX_RECORD_PAGES`].
const fn answer_word(verb: Verb, pages: RecordPages, outcome: Outcome) -> u64 {
    verb as u64 | ((pages.0.count as u64) << 16) | (outcome.to_word() << 32)
}

/// The page range and outcome of a [`Request::Filled`] or [`Request::WrittenBack`], from the
/// bits of its first word above the verb (`packed`) and its first page.
fn answer(packed: u64, first: u64) -> Result<(RecordPages, Outcome), DecodeError> {
    let pages = record_range(first, packed & 0xffff).ok_or(DecodeError::Value)?;
    let outcome = Outcome::from_word(packed >> 16).ok_or(DecodeError::Value)?;
    Ok((pages, outcome))
}

/// A page range a record or an answer may carry.
fn record_range(first: u64, count: u64) -> Option<RecordPages> {
    RecordPages::from_parts(first, u32::try_from(count).ok()?)
}

/// Why metadata words are not a valid [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The verb is missing or unknown.
    Verb,
    /// The number of words does not match the verb.
    Length,
    /// A word has a value the call does not allow.
    Value,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Verb => "missing or unknown pcache verb",
            Self::Length => "wrong number of metadata words for the pcache verb",
            Self::Value => "a pcache call word is out of range",
        })
    }
}

impl core::error::Error for DecodeError {}

/// A record the kernel puts on a file system's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// Fill these pages of the object with the file's data, then answer [`Request::Filled`].
    Fill {
        /// The object.
        object: ObjectId,
        /// The object instance's generation, for the answer.
        generation: Generation,
        /// Which of the object's fills this is, for the answer.
        fill: FillId,
        /// The pages to fill.
        pages: RecordPages,
    },
    /// The object has dirty pages: write them back ([`Request::TakeDirty`] and
    /// [`Request::WrittenBack`]), within the file system's age limit, or at once when
    /// `urgent`. Posted when the object's first page becomes dirty, and (urgent) when a write
    /// takes the queue past its dirty limit ([`Request::SetWriteback`]).
    Writeback {
        /// The object.
        object: ObjectId,
        /// The object instance's generation.
        generation: Generation,
        /// The pages the write that posted it dirtied (at most a record's worth).
        pages: RecordPages,
        /// The queue is past its dirty limit.
        urgent: bool,
    },
    /// The kernel needs memory: close the object if it is only retained (no client has the
    /// file open).
    Release {
        /// The object.
        object: ObjectId,
        /// The object instance's generation, for the answer.
        generation: Generation,
    },
    /// The file was read since the last such record: update its access time.
    Accessed {
        /// The object.
        object: ObjectId,
        /// The object instance's generation, for the answer.
        generation: Generation,
    },
}

/// What an answer to a [`Record::Fill`] repeats from it: the object's generation, the fill's
/// ID and its pages ([`Record::fill`] takes them out of a record).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FillRef {
    /// The generation of the object the record was for.
    pub generation: Generation,
    /// The fill's ID.
    pub id: FillId,
    /// The pages the record names.
    pub pages: RecordPages,
}

impl FillRef {
    /// The [`Request::Filled`] answer to this fill with `outcome`.
    pub const fn answer(self, outcome: Outcome) -> Request {
        Request::Filled {
            pages: self.pages,
            outcome,
            generation: self.generation,
            fill: self.id,
        }
    }
}

impl Record {
    /// The size of a record on the queue, in bytes.
    pub const BYTES: usize = 40;

    /// The object a fill record is for, and what its answer repeats; `None` for other
    /// records.
    pub const fn fill(&self) -> Option<(ObjectId, FillRef)> {
        match *self {
            Self::Fill {
                object,
                generation,
                fill,
                pages,
            } => Some((
                object,
                FillRef {
                    generation,
                    id: fill,
                    pages,
                },
            )),
            _ => None,
        }
    }

    const KIND_FILL: u16 = 1;
    const KIND_WRITEBACK: u16 = 2;
    const KIND_RELEASE: u16 = 3;
    const KIND_ACCESSED: u16 = 4;

    /// The record's bytes: `kind: u16`, `count: u16`, a reserved zero `u32`, `object: u64`,
    /// `first: u64`, `generation: u64`, `fill: u64`, all little-endian. Records without pages
    /// have `first` and `count` zero; a writeback record has its `urgent` flag (0 or 1) in the
    /// `fill` word, and the other records other than fills have it zero.
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let pages = match self {
            Self::Fill { pages, .. } | Self::Writeback { pages, .. } => Some(pages),
            Self::Release { .. } | Self::Accessed { .. } => None,
        };
        let kind = match self {
            Self::Fill { .. } => Self::KIND_FILL,
            Self::Writeback { .. } => Self::KIND_WRITEBACK,
            Self::Release { .. } => Self::KIND_RELEASE,
            Self::Accessed { .. } => Self::KIND_ACCESSED,
        };
        let object = self.object();
        // A record's count is at most `MAX_RECORD_PAGES`, which fits 16 bits.
        let (first, count) = pages.map_or((0, 0), |p| (p.0.first, p.0.count as u16));
        let mut bytes = [0; Self::BYTES];
        bytes[0..2].copy_from_slice(&kind.to_le_bytes());
        bytes[2..4].copy_from_slice(&count.to_le_bytes());
        bytes[8..16].copy_from_slice(&object.0.to_le_bytes());
        bytes[16..24].copy_from_slice(&first.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.generation().0.to_le_bytes());
        let word = match self {
            Self::Fill { fill, .. } => fill.0,
            Self::Writeback { urgent, .. } => u64::from(urgent),
            Self::Release { .. } | Self::Accessed { .. } => 0,
        };
        bytes[32..40].copy_from_slice(&word.to_le_bytes());
        bytes
    }

    /// Decodes a record.
    ///
    /// # Errors
    ///
    /// [`RecordError::Length`] for a buffer that is not [`Self::BYTES`] long,
    /// [`RecordError::Kind`] for an unknown kind, [`RecordError::Reserved`] for non-zero
    /// reserved bytes (or pages on a record that has none), and [`RecordError::Pages`] for an
    /// empty, overflowing or too long page range.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RecordError> {
        let bytes: &[u8; Self::BYTES] = bytes.try_into().map_err(|_| RecordError::Length)?;
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let u64_at = |at: usize| {
            let mut word = [0; 8];
            word.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(word)
        };
        if bytes[4..8] != [0; 4] {
            return Err(RecordError::Reserved);
        }
        let object = ObjectId(u64_at(8));
        let (first, count) = (u64_at(16), u16_at(2));
        let generation = Generation(u64_at(24));
        let fill = u64_at(32);
        let no_fill = || {
            if fill == 0 {
                Ok(())
            } else {
                Err(RecordError::Reserved)
            }
        };
        let pages = || record_range(first, u64::from(count)).ok_or(RecordError::Pages);
        let no_pages = || {
            if first == 0 && count == 0 {
                Ok(())
            } else {
                Err(RecordError::Reserved)
            }
        };
        match u16_at(0) {
            Self::KIND_FILL => Ok(Self::Fill {
                object,
                generation,
                fill: FillId(fill),
                pages: pages()?,
            }),
            Self::KIND_WRITEBACK => match fill {
                0 | 1 => Ok(Self::Writeback {
                    object,
                    generation,
                    pages: pages()?,
                    urgent: fill == 1,
                }),
                _ => Err(RecordError::Reserved),
            },
            Self::KIND_RELEASE => no_pages()
                .and_then(|()| no_fill())
                .map(|()| Self::Release { object, generation }),
            Self::KIND_ACCESSED => no_pages()
                .and_then(|()| no_fill())
                .map(|()| Self::Accessed { object, generation }),
            _ => Err(RecordError::Kind),
        }
    }

    /// Decodes the records in `bytes`, as a queue read returns them: whole records one after
    /// the other.
    ///
    /// # Errors
    ///
    /// Each malformed record is an error ([`Record::from_bytes`]), and so is a tail shorter
    /// than a record ([`RecordError::Length`]).
    pub fn decode_all(bytes: &[u8]) -> impl Iterator<Item = Result<Self, RecordError>> + '_ {
        let tail = (!bytes.len().is_multiple_of(Self::BYTES)).then_some(Err(RecordError::Length));
        bytes
            .chunks_exact(Self::BYTES)
            .map(Self::from_bytes)
            .chain(tail)
    }

    /// The object the record is about.
    pub const fn object(self) -> ObjectId {
        match self {
            Self::Fill { object, .. }
            | Self::Writeback { object, .. }
            | Self::Release { object, .. }
            | Self::Accessed { object, .. } => object,
        }
    }

    /// The generation of the object instance the record is about.
    pub const fn generation(self) -> Generation {
        match self {
            Self::Fill { generation, .. }
            | Self::Writeback { generation, .. }
            | Self::Release { generation, .. }
            | Self::Accessed { generation, .. } => generation,
        }
    }
}

/// Why bytes are not a valid [`Record`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordError {
    /// The buffer is not [`Record::BYTES`] long.
    Length,
    /// The kind is unknown.
    Kind,
    /// A reserved field is not zero.
    Reserved,
    /// The page range is empty, overflows or is longer than [`MAX_RECORD_PAGES`].
    Pages,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Length => "a pcache record has the wrong length",
            Self::Kind => "unknown pcache record kind",
            Self::Reserved => "a reserved pcache record field is not zero",
            Self::Pages => "a pcache record's page range is invalid",
        })
    }
}

impl core::error::Error for RecordError {}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::ToString;
    use std::vec::Vec;

    fn range(first: u64, count: u32) -> PageRange {
        PageRange::new(first, count).expect("a valid range")
    }

    fn record(first: u64, count: u32) -> RecordPages {
        RecordPages::from_parts(first, count).expect("a valid record range")
    }

    fn errno(n: u16) -> NonZeroU16 {
        NonZeroU16::new(n).expect("non-zero")
    }

    #[test]
    fn a_fill_record_gives_what_its_answer_repeats() {
        let fill = Record::Fill {
            object: ObjectId::new(3),
            generation: Generation::new(9),
            fill: FillId::new(5),
            pages: record(8, 2),
        };
        let (object, answer) = fill.fill().expect("a fill record");
        assert_eq!(object, ObjectId::new(3));
        assert_eq!(
            answer.answer(Outcome::Done),
            Request::Filled {
                pages: record(8, 2),
                outcome: Outcome::Done,
                generation: Generation::new(9),
                fill: FillId::new(5),
            }
        );
        let release = Record::Release {
            object: ObjectId::new(3),
            generation: Generation::new(9),
        };
        assert_eq!(release.fill(), None);
    }

    #[test]
    fn decode_all_takes_whole_records() {
        let fill = Record::Fill {
            object: ObjectId::new(3),
            generation: Generation::new(1),
            fill: FillId::new(5),
            pages: record(8, 2),
        };
        let release = Record::Release {
            object: ObjectId::new(4),
            generation: Generation::new(2),
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&fill.to_bytes());
        bytes.extend_from_slice(&release.to_bytes());
        let got: Vec<_> = Record::decode_all(&bytes).collect();
        assert_eq!(got, [Ok(fill), Ok(release)]);
        assert_eq!(Record::decode_all(&[]).count(), 0);
        // A short tail and a bad record are errors; the good records around them still decode.
        bytes.extend_from_slice(&[1, 2, 3]);
        let got: Vec<_> = Record::decode_all(&bytes).collect();
        assert_eq!(got, [Ok(fill), Ok(release), Err(RecordError::Length)]);
        let mut bad = fill.to_bytes();
        bad[0] = 99;
        assert_eq!(
            Record::decode_all(&bad).collect::<Vec<_>>(),
            [Err(RecordError::Kind)]
        );
    }

    #[test]
    fn covering_holds_every_byte() {
        let pages = |start, end| PageRange::covering(start, end).collect::<Vec<_>>();
        assert_eq!(pages(0, 0), []);
        assert_eq!(pages(5000, 5000), []);
        assert_eq!(pages(0, 1), [PageRange::new(0, 1).unwrap()]);
        // A write of bytes 4095..4097 touches pages 0 and 1.
        assert_eq!(pages(4095, 4097), [PageRange::new(0, 2).unwrap()]);
        assert_eq!(pages(8192, 3 * 4096), [PageRange::new(2, 1).unwrap()]);
        // More pages than one range holds come in several.
        let huge = pages(0, (u64::from(u32::MAX) + 10) * 4096);
        assert_eq!(
            huge,
            [
                PageRange::new(0, u32::MAX).unwrap(),
                PageRange::new(u64::from(u32::MAX), 10).unwrap()
            ]
        );
    }

    #[test]
    fn record_pages_are_bounded_and_split() {
        assert_eq!(
            RecordPages::from_parts(0, MAX_RECORD_PAGES).map(RecordPages::pages),
            Some(range(0, MAX_RECORD_PAGES))
        );
        assert_eq!(RecordPages::from_parts(0, MAX_RECORD_PAGES + 1), None);
        assert_eq!(RecordPages::new(range(0, 1025)), None);
        assert_eq!(RecordPages::from_parts(0, 0), None);
        let runs: std::vec::Vec<_> = RecordPages::split(range(10, 2 * MAX_RECORD_PAGES + 1))
            .map(PageRange::from)
            .collect();
        assert_eq!(
            runs,
            [
                range(10, MAX_RECORD_PAGES),
                range(10 + 1024, MAX_RECORD_PAGES),
                range(10 + 2048, 1)
            ]
        );
        assert_eq!(RecordPages::split(range(u64::MAX - 3, 3)).count(), 1);
    }

    #[test]
    fn an_errno_is_never_done() {
        // Errno 0 on the wire is Done; a failure always encodes as a non-zero errno field.
        let failed = Request::Filled {
            pages: record(0, 1),
            outcome: Outcome::Failed(errno(1)),
            generation: Generation::default(),
            fill: FillId::new(1),
        };
        let (words, len) = failed.encode();
        assert_eq!(len, 4);
        assert_eq!(words[0], 4 | 1 << 16 | 1 << 32);
        assert_eq!(Request::decode(&words[..len]), Ok(failed));
        assert_eq!(
            Request::decode(&[4 | 1 << 16, 0, 0, 2]),
            Ok(Request::Filled {
                pages: record(0, 1),
                outcome: Outcome::Done,
                generation: Generation::default(),
                fill: FillId::new(2),
            })
        );
        assert_eq!(
            Request::decode(&[4 | 3 << 16 | 0xffff << 32, 7, 0, 3]),
            Ok(Request::Filled {
                pages: record(7, 3),
                outcome: Outcome::Failed(errno(0xffff)),
                generation: Generation::default(),
                fill: FillId::new(3),
            })
        );
    }

    #[test]
    fn answers_carry_the_whole_generation() {
        let answer = Request::Filled {
            pages: record(2, 1),
            outcome: Outcome::Failed(errno(5)),
            generation: Generation::new(0xbeef_0000_0001),
            fill: FillId::new(0x1234_5678_9abc),
        };
        let (words, len) = answer.encode();
        assert_eq!(
            words,
            [4 | 1 << 16 | 5 << 32, 2, 0xbeef_0000_0001, 0x1234_5678_9abc]
        );
        assert_eq!(Request::decode(&words[..len]), Ok(answer));
        // The errno field stays 16 bits: the top 16 bits of the first word are refused.
        assert_eq!(
            Request::decode(&[5 | 1 << 16 | 1 << 48, 0]),
            Err(DecodeError::Value)
        );
    }

    #[test]
    fn generations_do_not_repeat_at_the_old_16_bit_boundary() {
        // The first object past 65 536 opens: with 16-bit generations it matched object 0.
        let first = Generation::default();
        let mut later = first;
        for _ in 0..=u16::MAX {
            later = later.next().expect("far from the end");
        }
        assert_eq!(later.get(), 0x1_0000);
        assert_ne!(later, first);
        let answer = |generation| Request::Filled {
            pages: record(0, 1),
            outcome: Outcome::Done,
            generation,
            fill: FillId::new(1),
        };
        let (words, len) = answer(first).encode();
        assert_eq!(Request::decode(&words[..len]), Ok(answer(first)));
        assert_ne!(Request::decode(&words[..len]), Ok(answer(later)));
        let fill = |generation| Record::Fill {
            object: ObjectId::new(7),
            generation,
            fill: FillId::new(1),
            pages: record(0, 1),
        };
        assert_ne!(fill(first).to_bytes(), fill(later).to_bytes());
        assert_eq!(Record::from_bytes(&fill(later).to_bytes()), Ok(fill(later)));
    }

    #[test]
    fn generations_end_instead_of_wrapping() {
        assert_eq!(
            Generation::new(u64::MAX - 1).next(),
            Some(Generation::new(u64::MAX))
        );
        assert_eq!(Generation::new(u64::MAX).next(), None);
    }

    #[test]
    fn every_request_fits_a_kernel_call() {
        // The kernel hands a kernel scheme at most four metadata words.
        assert_eq!(Request::MAX_WORDS, 4);
        let widest = Request::Filled {
            pages: record(u64::MAX - MAX_RECORD_PAGES as u64, MAX_RECORD_PAGES),
            outcome: Outcome::Failed(errno(u16::MAX)),
            generation: Generation::new(u64::MAX),
            fill: FillId::new(u64::MAX),
        };
        let (words, len) = widest.encode();
        assert_eq!(len, Request::MAX_WORDS);
        assert_eq!(Request::decode(&words[..len]), Ok(widest));
        assert_eq!(Request::Bind.encode(), ([7, 0, 0, 0], 1));
    }

    #[test]
    fn page_ranges_refuse_empty_and_overflowing_runs() {
        assert_eq!(PageRange::new(5, 0), None);
        assert_eq!(PageRange::new(u64::MAX, 1), None);
        assert_eq!(
            PageRange::new(u64::MAX - 1, 1).map(PageRange::end),
            Some(u64::MAX)
        );
        assert_eq!(PageRange::from_bounds(4, 4), None);
        assert_eq!(PageRange::from_bounds(4, 9), Some(range(4, 5)));
        assert_eq!(PageRange::from_bounds(0, u64::from(u32::MAX) + 1), None);
    }

    #[test]
    fn page_ranges_intersect_and_contain() {
        let a = range(10, 10);
        assert!(a.contains(10) && a.contains(19) && !a.contains(20) && !a.contains(9));
        assert_eq!(a.intersect(range(15, 10)), Some(range(15, 5)));
        assert_eq!(a.intersect(range(20, 1)), None);
        assert_eq!(PageRange::pages_for(0), 0);
        assert_eq!(PageRange::pages_for(1), 1);
        assert_eq!(PageRange::pages_for(PAGE_SIZE + 1), 2);
    }

    #[test]
    fn object_paths_round_trip() {
        let mut buf = [0; ObjectId::PATH_MAX];
        for id in [0, 1, 0xabc, u64::MAX] {
            let object = ObjectId::new(id);
            let path = object.encode_path(&mut buf);
            assert_eq!(ObjectId::parse_path(path.as_bytes()), Ok(object));
        }
        assert_eq!(ObjectId::new(0x2a).encode_path(&mut buf), "object/2a");
        for bad in [
            &b"object/"[..],
            b"object",
            b"objects/1",
            b"object/ABC",
            b"object/12345678901234567",
        ] {
            assert_eq!(ObjectId::parse_path(bad), Err(PathError));
        }
    }

    #[test]
    fn requests_round_trip() {
        let all = [
            Request::Bind,
            Request::SetSize { size: 12_345 },
            Request::Attach { handle: 9 },
            Request::Detach { handle: 9 },
            Request::Filled {
                pages: record(8, 4),
                outcome: Outcome::Done,
                generation: Generation::new(3),
                fill: FillId::new(17),
            },
            Request::WrittenBack {
                pages: record(0, MAX_RECORD_PAGES),
                outcome: Outcome::Failed(errno(5)),
            },
            Request::WrittenBack {
                pages: record(u64::MAX - 1, 1),
                outcome: Outcome::Done,
            },
            Request::SetReadaheadMax {
                pages: ReadaheadMax::new(64).expect("in range"),
            },
            Request::SetReadaheadMax {
                pages: ReadaheadMax::new(MAX_RECORD_PAGES).expect("in range"),
            },
            Request::Invalidate {
                pages: PageRange::new(3, 1).expect("valid"),
            },
            Request::Invalidate {
                pages: PageRange::new(u64::MAX - u64::from(u32::MAX), u32::MAX).expect("valid"),
            },
            Request::SetWriteback {
                max_dirty: NonZeroU32::new(8192).expect("non-zero"),
            },
            Request::SetWriteback {
                max_dirty: NonZeroU32::MAX,
            },
            Request::AttachAccess {
                handle: 3,
                access: Access::Read,
            },
            Request::AttachAccess {
                handle: u64::MAX,
                access: Access::Write,
            },
            Request::AttachAccess {
                handle: 0,
                access: Access::ReadWrite,
            },
            Request::HoldWrites { hold: true },
            Request::HoldWrites { hold: false },
            Request::TakeDirty { from: 0 },
            Request::TakeDirty { from: u64::MAX },
        ];
        for request in all {
            let (words, len) = request.encode();
            assert_eq!(Request::decode(&words[..len]), Ok(request));
        }
    }

    #[test]
    fn bad_requests_are_refused() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[99]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[1]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[1, 2, 3]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[7, 0]), Err(DecodeError::Length));
        // A fill answer without its fill ID, and a writeback answer with a generation.
        assert_eq!(
            Request::decode(&[4 | 1 << 16, 1, 0]),
            Err(DecodeError::Length)
        );
        assert_eq!(
            Request::decode(&[5 | 1 << 16, 1, 0]),
            Err(DecodeError::Length)
        );
        // An empty range, a too long one, a zero readahead maximum, bits above a plain verb.
        assert_eq!(Request::decode(&[4, 0, 0, 1]), Err(DecodeError::Value));
        let long = u64::from(MAX_RECORD_PAGES) + 1;
        assert_eq!(
            Request::decode(&[4 | long << 16, 0, 0, 1]),
            Err(DecodeError::Value)
        );
        assert_eq!(Request::decode(&[1 | 1 << 16, 0]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[7 | 1 << 40]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[6, 0]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[6, 1 << 32]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[6, long]), Err(DecodeError::Value));
        // An invalidation of no pages, of too many, or of a range past the last page.
        assert_eq!(Request::decode(&[8, 0, 0]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[8, 0, 1 << 32]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[8, u64::MAX, 2]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[8, 0]), Err(DecodeError::Length));
        // A dirty limit of 0 or past 32 bits, an unknown access, a hold that is not a bool.
        assert_eq!(Request::decode(&[9, 0]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[9, 1 << 32]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[10, 1, 0]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[10, 1, 4]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[10, 1]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[11, 2]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[12]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[12 | 1 << 16, 0]), Err(DecodeError::Value));
        // So the encoder cannot produce what the decoder refuses: 1024 is the largest maximum.
        assert_eq!(
            ReadaheadMax::new(MAX_RECORD_PAGES).map(ReadaheadMax::get),
            Some(1024)
        );
        assert_eq!(ReadaheadMax::new(MAX_RECORD_PAGES + 1), None);
        assert_eq!(ReadaheadMax::new(0), None);
    }

    #[test]
    fn records_round_trip() {
        let object = ObjectId::new(0xdead_beef);
        let generation = Generation::new(0x0102_0304_0506_0708);
        for record in [
            Record::Fill {
                object,
                generation,
                fill: FillId::new(0x1122_3344),
                pages: record(1, 2),
            },
            Record::Writeback {
                object,
                generation,
                pages: record(u64::MAX - 8, 8),
                urgent: false,
            },
            Record::Writeback {
                object,
                generation,
                pages: record(0, MAX_RECORD_PAGES),
                urgent: true,
            },
            Record::Release { object, generation },
            Record::Accessed { object, generation },
        ] {
            assert_eq!(Record::from_bytes(&record.to_bytes()), Ok(record));
            assert_eq!(record.object(), object);
            assert_eq!(record.generation(), generation);
            assert_eq!(record.to_bytes()[24..32], [8, 7, 6, 5, 4, 3, 2, 1]);
        }
    }

    #[test]
    fn bad_records_are_refused() {
        let fill = Record::Fill {
            object: ObjectId::new(1),
            generation: Generation::new(9),
            fill: FillId::new(1),
            pages: record(4, 1),
        }
        .to_bytes();
        assert_eq!(Record::from_bytes(&fill[..39]), Err(RecordError::Length));
        let mut bad = fill;
        bad[0] = 9;
        assert_eq!(Record::from_bytes(&bad), Err(RecordError::Kind));
        let mut bad = fill;
        bad[6] = 1;
        assert_eq!(Record::from_bytes(&bad), Err(RecordError::Reserved));
        let mut bad = fill;
        bad[2..4].copy_from_slice(&0_u16.to_le_bytes());
        assert_eq!(Record::from_bytes(&bad), Err(RecordError::Pages));
        let mut bad = fill;
        bad[2..4].copy_from_slice(&(MAX_RECORD_PAGES as u16 + 1).to_le_bytes());
        assert_eq!(Record::from_bytes(&bad), Err(RecordError::Pages));
        let mut release = Record::Release {
            object: ObjectId::new(1),
            generation: Generation::default(),
        }
        .to_bytes();
        let mut with_fill = release;
        release[16] = 1;
        assert_eq!(Record::from_bytes(&release), Err(RecordError::Reserved));
        // Only a fill has a fill ID.
        with_fill[32] = 1;
        assert_eq!(Record::from_bytes(&with_fill), Err(RecordError::Reserved));
        // A writeback record's urgent flag is 0 or 1.
        let mut writeback = Record::Writeback {
            object: ObjectId::new(1),
            generation: Generation::default(),
            pages: record(4, 1),
            urgent: true,
        }
        .to_bytes();
        assert_eq!(writeback[32..40], [1, 0, 0, 0, 0, 0, 0, 0]);
        writeback[32] = 2;
        assert_eq!(Record::from_bytes(&writeback), Err(RecordError::Reserved));
    }

    #[test]
    fn taken_pages_round_trip_below_the_error_returns() {
        assert_eq!(encode_taken(None), Some(0));
        assert_eq!(decode_taken(0), Ok(None));
        let last = TAKEN_FIRST_LIMIT - 1;
        for pages in [
            record(0, 1),
            record(7, 3),
            record(0, MAX_RECORD_PAGES),
            record(last, 1),
        ] {
            let word = encode_taken(Some(pages)).expect("encodable");
            // A kernel call's error returns are the top 4095 values; these stay below 2^63.
            assert!(word < 1 << 63, "{word:#x}");
            assert_eq!(decode_taken(word), Ok(Some(pages)));
        }
        assert_eq!(encode_taken(Some(record(TAKEN_FIRST_LIMIT, 1))), None);
        // No count, too many pages, a page too far.
        assert_eq!(decode_taken(5 << 11), Err(DecodeError::Value));
        assert_eq!(decode_taken(1025), Err(DecodeError::Value));
        assert_eq!(
            decode_taken(TAKEN_FIRST_LIMIT << 11 | 1),
            Err(DecodeError::Value)
        );
    }

    #[test]
    fn access_says_what_is_served() {
        assert!(Access::Read.reads() && !Access::Read.writes());
        assert!(!Access::Write.reads() && Access::Write.writes());
        assert!(Access::ReadWrite.reads() && Access::ReadWrite.writes());
    }

    #[test]
    fn errors_describe_themselves() {
        let messages = [
            PathError.to_string(),
            DecodeError::Verb.to_string(),
            DecodeError::Length.to_string(),
            DecodeError::Value.to_string(),
            RecordError::Length.to_string(),
            RecordError::Kind.to_string(),
            RecordError::Reserved.to_string(),
            RecordError::Pages.to_string(),
        ];
        for (i, message) in messages.iter().enumerate() {
            assert!(!message.is_empty());
            assert!(!messages[..i].contains(message), "{message} is used twice");
        }
    }
}
