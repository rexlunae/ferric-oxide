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
//! | [`Request::Filled`] | an object | metadata `[verb \| count << 16 \| errno << 32, first page, generation]`, payload the pages |
//! | [`Request::WrittenBack`] | an object | metadata `[verb \| count << 16 \| errno << 32, first page, generation]` |
//! | [`Request::SetReadaheadMax`] | a queue | metadata `[verb, pages]` |
//! | read records | a queue | `read(queue, buf)`: whole [`Record::BYTES`]-byte records |
//!
//! Every call fits the three metadata words the kernel passes a kernel scheme
//! ([`Request::MAX_WORDS`]).
//!
//! A file system opens one queue and binds it to its own scheme with [`Request::Bind`],
//! passing its scheme socket, so the queue can only ever serve that scheme's descriptions.
//! Each object it creates belongs to that queue: the kernel puts the object's [`Record`]s
//! there, and the file system answers each record with [`Request::Filled`] or
//! [`Request::WrittenBack`] on the object's descriptor, which only it holds. The file system
//! picks each object's ID ([`ObjectId`], unique on its queue, for example the file's node
//! number) when it creates the object, so records name objects in its own terms and need no
//! lookup. Opening an ID the queue already has fails with `EEXIST`. An ID may be reused once
//! its object has closed, so each object also has a [`Generation`], which the kernel puts in
//! every record and the file system copies into its answer: an answer to a record of an
//! earlier object with the same ID is refused (`ESTALE`), and closing an object drops the
//! records of it still on the queue. Generations are 64 bits and never repeat on a queue: the
//! kernel refuses to open an object once they run out (`EOVERFLOW`), which at a billion opens
//! a second takes centuries.
//!
//! Only the process that opened the queue may use it and its objects, binding included: a
//! queue or object descriptor passed to another process is refused there (`EPERM`), so it
//! cannot bind the queue to its own scheme, or attach or fill another client's descriptions.
//!
//! [`Request::Attach`] ties a client's open file description to the object: `handle` is the
//! number the file system's scheme gave the client's description. The kernel then serves that
//! description's positioned `read` and `pread` from the object, until the file system
//! detaches it, the client closes it, or the object or its queue closes. Everything else
//! (unpositioned reads, `mmap`, writes, `fstat`) still goes to the file system's scheme;
//! serving shared mappings from objects is a later step of the design.
//!
//! [`Request::encode`] and [`Request::decode`] are the one place the metadata layout is written
//! down; [`Record::to_bytes`] and [`Record::from_bytes`] the one place the record layout is.

use core::fmt;
use core::num::NonZeroU16;

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
    /// object's frames; a failed fill has an empty payload.
    Filled {
        /// The pages the record named.
        pages: RecordPages,
        /// How the fill ended.
        outcome: Outcome,
        /// The record's [`Generation`].
        generation: Generation,
    },
    /// On the object a [`Record::Writeback`] named: the answer to it.
    WrittenBack {
        /// The pages the record named.
        pages: RecordPages,
        /// How the writeback ended.
        outcome: Outcome,
        /// The record's [`Generation`].
        generation: Generation,
    },
    /// On a queue: read ahead at most `pages` pages for this file system's objects.
    SetReadaheadMax {
        /// The largest readahead window, in pages.
        pages: ReadaheadMax,
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
            _ => return None,
        })
    }
}

impl Request {
    /// The most metadata words a request takes: what the kernel passes a kernel scheme.
    pub const MAX_WORDS: usize = 3;

    /// The metadata words, and how many of them are used.
    ///
    /// [`Self::Filled`] and [`Self::WrittenBack`] are
    /// `[verb | count << 16 | errno << 32, first page, generation]`: the verb in the low 16
    /// bits of the first word, the page count in the 16 above it, the outcome's errno (0 for
    /// [`Outcome::Done`]) in the 16 above that, the top 16 zero, and the record's whole
    /// [`Generation`] in the last word. Every other request has the verb alone in its first
    /// word.
    pub const fn encode(self) -> ([u64; Self::MAX_WORDS], usize) {
        match self {
            Self::Bind => ([Verb::Bind as u64, 0, 0], 1),
            Self::SetSize { size } => ([Verb::SetSize as u64, size, 0], 2),
            Self::Attach { handle } => ([Verb::Attach as u64, handle, 0], 2),
            Self::Detach { handle } => ([Verb::Detach as u64, handle, 0], 2),
            Self::Filled {
                pages,
                outcome,
                generation,
            } => (
                [
                    answer_word(Verb::Filled, pages, outcome),
                    pages.0.first,
                    generation.0,
                ],
                3,
            ),
            Self::WrittenBack {
                pages,
                outcome,
                generation,
            } => (
                [
                    answer_word(Verb::WrittenBack, pages, outcome),
                    pages.0.first,
                    generation.0,
                ],
                3,
            ),
            Self::SetReadaheadMax { pages } => {
                ([Verb::SetReadaheadMax as u64, pages.0 as u64, 0], 2)
            }
        }
    }

    /// Decodes the metadata words of a call.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, [`DecodeError::Length`] when the
    /// number of words does not match it, and [`DecodeError::Value`] for a page range that is
    /// empty, overflows or is longer than [`MAX_RECORD_PAGES`], a readahead maximum of 0 or
    /// above [`MAX_RECORD_PAGES`], or bits above the verb that the request does not have.
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
            (Verb::Filled, &[first, generation]) => {
                let (pages, outcome) = answer(packed, first)?;
                let generation = Generation(generation);
                Ok(Self::Filled {
                    pages,
                    outcome,
                    generation,
                })
            }
            (Verb::WrittenBack, &[first, generation]) => {
                let (pages, outcome) = answer(packed, first)?;
                let generation = Generation(generation);
                Ok(Self::WrittenBack {
                    pages,
                    outcome,
                    generation,
                })
            }
            (Verb::SetReadaheadMax, &[pages]) => u32::try_from(pages)
                .ok()
                .and_then(ReadaheadMax::new)
                .map(|pages| Self::SetReadaheadMax { pages })
                .ok_or(DecodeError::Value),
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
        /// The pages to fill.
        pages: RecordPages,
    },
    /// Write these dirty pages of the object to the file, then answer
    /// [`Request::WrittenBack`].
    Writeback {
        /// The object.
        object: ObjectId,
        /// The object instance's generation, for the answer.
        generation: Generation,
        /// The pages to write.
        pages: RecordPages,
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

impl Record {
    /// The size of a record on the queue, in bytes.
    pub const BYTES: usize = 32;

    const KIND_FILL: u16 = 1;
    const KIND_WRITEBACK: u16 = 2;
    const KIND_RELEASE: u16 = 3;
    const KIND_ACCESSED: u16 = 4;

    /// The record's bytes: `kind: u16`, `count: u16`, a reserved zero `u32`, `object: u64`,
    /// `first: u64`, `generation: u64`, all little-endian. Records without pages have `first`
    /// and `count` zero.
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
                pages: pages()?,
            }),
            Self::KIND_WRITEBACK => Ok(Self::Writeback {
                object,
                generation,
                pages: pages()?,
            }),
            Self::KIND_RELEASE => no_pages().map(|()| Self::Release { object, generation }),
            Self::KIND_ACCESSED => no_pages().map(|()| Self::Accessed { object, generation }),
            _ => Err(RecordError::Kind),
        }
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
        };
        let (words, len) = failed.encode();
        assert_eq!(len, 3);
        assert_eq!(words[0], 4 | 1 << 16 | 1 << 32);
        assert_eq!(Request::decode(&words[..len]), Ok(failed));
        assert_eq!(
            Request::decode(&[4 | 1 << 16, 0, 0]),
            Ok(Request::Filled {
                pages: record(0, 1),
                outcome: Outcome::Done,
                generation: Generation::default(),
            })
        );
        assert_eq!(
            Request::decode(&[4 | 3 << 16 | 0xffff << 32, 7, 0]),
            Ok(Request::Filled {
                pages: record(7, 3),
                outcome: Outcome::Failed(errno(0xffff)),
                generation: Generation::default(),
            })
        );
    }

    #[test]
    fn answers_carry_the_whole_generation() {
        let answer = Request::Filled {
            pages: record(2, 1),
            outcome: Outcome::Failed(errno(5)),
            generation: Generation::new(0xbeef_0000_0001),
        };
        let (words, len) = answer.encode();
        assert_eq!(words, [4 | 1 << 16 | 5 << 32, 2, 0xbeef_0000_0001]);
        assert_eq!(Request::decode(&words[..len]), Ok(answer));
        // The errno field stays 16 bits: the top 16 bits of the first word are refused.
        assert_eq!(
            Request::decode(&[5 | 1 << 16 | 1 << 48, 0, 0]),
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
        };
        let (words, len) = answer(first).encode();
        assert_eq!(Request::decode(&words[..len]), Ok(answer(first)));
        assert_ne!(Request::decode(&words[..len]), Ok(answer(later)));
        let fill = |generation| Record::Fill {
            object: ObjectId::new(7),
            generation,
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
        // The kernel hands a kernel scheme at most three metadata words.
        assert_eq!(Request::MAX_WORDS, 3);
        let widest = Request::WrittenBack {
            pages: record(u64::MAX - MAX_RECORD_PAGES as u64, MAX_RECORD_PAGES),
            outcome: Outcome::Failed(errno(u16::MAX)),
            generation: Generation::new(u64::MAX),
        };
        let (words, len) = widest.encode();
        assert_eq!(Request::decode(&words[..len]), Ok(widest));
        assert_eq!(Request::Bind.encode(), ([7, 0, 0], 1));
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
            },
            Request::WrittenBack {
                pages: record(0, MAX_RECORD_PAGES),
                outcome: Outcome::Failed(errno(5)),
                generation: Generation::default(),
            },
            Request::SetReadaheadMax {
                pages: ReadaheadMax::new(64).expect("in range"),
            },
            Request::SetReadaheadMax {
                pages: ReadaheadMax::new(MAX_RECORD_PAGES).expect("in range"),
            },
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
        assert_eq!(Request::decode(&[4, 1, 0, 1]), Err(DecodeError::Length));
        // An empty range, a too long one, a zero readahead maximum, bits above a plain verb.
        assert_eq!(Request::decode(&[4, 0, 0]), Err(DecodeError::Value));
        let long = u64::from(MAX_RECORD_PAGES) + 1;
        assert_eq!(
            Request::decode(&[4 | long << 16, 0, 0]),
            Err(DecodeError::Value)
        );
        assert_eq!(Request::decode(&[1 | 1 << 16, 0]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[7 | 1 << 40]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[6, 0]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[6, 1 << 32]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[6, long]), Err(DecodeError::Value));
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
                pages: record(1, 2),
            },
            Record::Writeback {
                object,
                generation,
                pages: record(u64::MAX - 8, 8),
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
            pages: record(4, 1),
        }
        .to_bytes();
        assert_eq!(Record::from_bytes(&fill[..31]), Err(RecordError::Length));
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
        release[16] = 1;
        assert_eq!(Record::from_bytes(&release), Err(RecordError::Reserved));
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
