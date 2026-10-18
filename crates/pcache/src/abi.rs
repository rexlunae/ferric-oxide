//! The `kernel.pcache` scheme's paths, calls and fill-queue records.
//!
//! | Operation | On | Encoding |
//! | --- | --- | --- |
//! | open [`QUEUE_PATH`] | the scheme root (root only) | `open("/scheme/kernel.pcache/queue")` |
//! | open an object path | a queue | `openat(queue, ObjectId::encode_path(..))`: a new, empty object |
//! | [`Request::SetSize`] | an object | `SYS_CALL`, metadata `[verb, size]` |
//! | [`Request::Attach`] | an object | metadata `[verb, handle]` |
//! | [`Request::Detach`] | an object | metadata `[verb, handle]` |
//! | [`Request::Filled`] | a queue | metadata `[verb, object, first page, count, errno]` |
//! | [`Request::WrittenBack`] | a queue | metadata `[verb, object, first page, count, errno]` |
//! | [`Request::SetReadaheadMax`] | a queue | metadata `[verb, pages]` |
//! | read records | a queue | `read(queue, buf)`: whole [`Record::BYTES`]-byte records |
//! | map an object | the file system | `mmap(object, ..., MAP_SHARED)`: only pages being filled or written back are writable |
//!
//! A file system opens one queue. Each object it creates belongs to that queue: the kernel puts
//! the object's [`Record`]s there, and only that queue's [`Request::Filled`] and
//! [`Request::WrittenBack`] answers are accepted for it. The file system picks each object's
//! ID ([`ObjectId`], unique on its queue, for example the file's node number) when it creates
//! the object, so records name objects in its own terms and need no lookup. Opening an ID the
//! queue already has fails with `EEXIST`.
//!
//! [`Request::Attach`] ties a client's open file description to the object: `handle` is the
//! number the file system's scheme gave the client's description. The kernel then serves that
//! description's `read`, `pread` and shared `mmap` from the object.
//!
//! [`Request::encode`] and [`Request::decode`] are the one place the metadata layout is written
//! down; [`Record::to_bytes`] and [`Record::from_bytes`] the one place the record layout is.

use core::fmt;

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

/// How a file system finished a fill or a writeback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// The pages are filled, or written back.
    Done,
    /// It failed with this errno; the kernel fails waiting readers with `EIO`, whatever it is.
    Failed(u16),
}

impl Outcome {
    const fn to_word(self) -> u64 {
        match self {
            Self::Done => 0,
            Self::Failed(errno) => errno as u64,
        }
    }

    const fn from_word(word: u64) -> Option<Self> {
        match word {
            0 => Some(Self::Done),
            1..=0xffff => Some(Self::Failed(word as u16)),
            _ => None,
        }
    }
}

/// A call on a queue or an object, as `SYS_CALL` metadata words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
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
    /// On a queue: the answer to a [`Record::Fill`].
    Filled {
        /// The object.
        object: ObjectId,
        /// The pages the record named.
        pages: PageRange,
        /// How the fill ended.
        outcome: Outcome,
    },
    /// On a queue: the answer to a [`Record::Writeback`].
    WrittenBack {
        /// The object.
        object: ObjectId,
        /// The pages the record named.
        pages: PageRange,
        /// How the writeback ended.
        outcome: Outcome,
    },
    /// On a queue: read ahead at most `pages` pages for this file system's objects.
    SetReadaheadMax {
        /// The largest readahead window, in pages.
        pages: u32,
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
            _ => return None,
        })
    }
}

impl Request {
    /// The most metadata words a request takes.
    pub const MAX_WORDS: usize = 5;

    /// The metadata words, and how many of them are used.
    pub const fn encode(self) -> ([u64; Self::MAX_WORDS], usize) {
        match self {
            Self::SetSize { size } => ([Verb::SetSize as u64, size, 0, 0, 0], 2),
            Self::Attach { handle } => ([Verb::Attach as u64, handle, 0, 0, 0], 2),
            Self::Detach { handle } => ([Verb::Detach as u64, handle, 0, 0, 0], 2),
            Self::Filled {
                object,
                pages,
                outcome,
            } => (
                [
                    Verb::Filled as u64,
                    object.0,
                    pages.first,
                    pages.count as u64,
                    outcome.to_word(),
                ],
                5,
            ),
            Self::WrittenBack {
                object,
                pages,
                outcome,
            } => (
                [
                    Verb::WrittenBack as u64,
                    object.0,
                    pages.first,
                    pages.count as u64,
                    outcome.to_word(),
                ],
                5,
            ),
            Self::SetReadaheadMax { pages } => {
                ([Verb::SetReadaheadMax as u64, pages as u64, 0, 0, 0], 2)
            }
        }
    }

    /// Decodes the metadata words of a call.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, [`DecodeError::Length`] when the
    /// number of words does not match it, and [`DecodeError::Value`] for a page range that is
    /// empty, overflows or is longer than [`MAX_RECORD_PAGES`], an errno above 16 bits, or a
    /// readahead maximum of 0.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let (&verb, args) = words.split_first().ok_or(DecodeError::Verb)?;
        match (Verb::from_raw(verb).ok_or(DecodeError::Verb)?, args) {
            (Verb::SetSize, &[size]) => Ok(Self::SetSize { size }),
            (Verb::Attach, &[handle]) => Ok(Self::Attach { handle }),
            (Verb::Detach, &[handle]) => Ok(Self::Detach { handle }),
            (Verb::Filled, &[object, first, count, outcome]) => {
                let (pages, outcome) = answer(first, count, outcome)?;
                Ok(Self::Filled {
                    object: ObjectId(object),
                    pages,
                    outcome,
                })
            }
            (Verb::WrittenBack, &[object, first, count, outcome]) => {
                let (pages, outcome) = answer(first, count, outcome)?;
                Ok(Self::WrittenBack {
                    object: ObjectId(object),
                    pages,
                    outcome,
                })
            }
            (Verb::SetReadaheadMax, &[pages]) => match u32::try_from(pages) {
                Ok(pages) if pages > 0 => Ok(Self::SetReadaheadMax { pages }),
                _ => Err(DecodeError::Value),
            },
            _ => Err(DecodeError::Length),
        }
    }
}

/// The page range and outcome of a [`Request::Filled`] or [`Request::WrittenBack`].
fn answer(first: u64, count: u64, outcome: u64) -> Result<(PageRange, Outcome), DecodeError> {
    let pages = record_range(first, count).ok_or(DecodeError::Value)?;
    let outcome = Outcome::from_word(outcome).ok_or(DecodeError::Value)?;
    Ok((pages, outcome))
}

/// A page range a record or an answer may carry.
fn record_range(first: u64, count: u64) -> Option<PageRange> {
    let count = u32::try_from(count).ok()?;
    if count > MAX_RECORD_PAGES {
        return None;
    }
    PageRange::new(first, count)
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
        /// The pages to fill.
        pages: PageRange,
    },
    /// Write these dirty pages of the object to the file, then answer
    /// [`Request::WrittenBack`].
    Writeback {
        /// The object.
        object: ObjectId,
        /// The pages to write.
        pages: PageRange,
    },
    /// The kernel needs memory: close the object if it is only retained (no client has the
    /// file open).
    Release {
        /// The object.
        object: ObjectId,
    },
    /// The file was read since the last such record: update its access time.
    Accessed {
        /// The object.
        object: ObjectId,
    },
}

impl Record {
    /// The size of a record on the queue, in bytes.
    pub const BYTES: usize = 32;

    const KIND_FILL: u32 = 1;
    const KIND_WRITEBACK: u32 = 2;
    const KIND_RELEASE: u32 = 3;
    const KIND_ACCESSED: u32 = 4;

    /// The record's bytes: `kind: u32`, a reserved zero `u32`, `object: u64`, `first: u64`,
    /// `count: u32`, a reserved zero `u32`, all little-endian. Records without pages have
    /// `first` and `count` zero.
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let (kind, object, pages) = match self {
            Self::Fill { object, pages } => (Self::KIND_FILL, object, Some(pages)),
            Self::Writeback { object, pages } => (Self::KIND_WRITEBACK, object, Some(pages)),
            Self::Release { object } => (Self::KIND_RELEASE, object, None),
            Self::Accessed { object } => (Self::KIND_ACCESSED, object, None),
        };
        let (first, count) = pages.map_or((0, 0), |p| (p.first, p.count));
        let mut bytes = [0; Self::BYTES];
        bytes[0..4].copy_from_slice(&kind.to_le_bytes());
        bytes[8..16].copy_from_slice(&object.0.to_le_bytes());
        bytes[16..24].copy_from_slice(&first.to_le_bytes());
        bytes[24..28].copy_from_slice(&count.to_le_bytes());
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
        let u32_at = |at: usize| {
            let mut word = [0; 4];
            word.copy_from_slice(&bytes[at..at + 4]);
            u32::from_le_bytes(word)
        };
        let u64_at = |at: usize| {
            let mut word = [0; 8];
            word.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(word)
        };
        if u32_at(4) != 0 || u32_at(28) != 0 {
            return Err(RecordError::Reserved);
        }
        let object = ObjectId(u64_at(8));
        let (first, count) = (u64_at(16), u32_at(24));
        let pages = || record_range(first, u64::from(count)).ok_or(RecordError::Pages);
        let no_pages = || {
            if first == 0 && count == 0 {
                Ok(())
            } else {
                Err(RecordError::Reserved)
            }
        };
        match u32_at(0) {
            Self::KIND_FILL => Ok(Self::Fill {
                object,
                pages: pages()?,
            }),
            Self::KIND_WRITEBACK => Ok(Self::Writeback {
                object,
                pages: pages()?,
            }),
            Self::KIND_RELEASE => no_pages().map(|()| Self::Release { object }),
            Self::KIND_ACCESSED => no_pages().map(|()| Self::Accessed { object }),
            _ => Err(RecordError::Kind),
        }
    }

    /// The object the record is about.
    pub const fn object(self) -> ObjectId {
        match self {
            Self::Fill { object, .. }
            | Self::Writeback { object, .. }
            | Self::Release { object }
            | Self::Accessed { object } => object,
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
            Request::SetSize { size: 12_345 },
            Request::Attach { handle: 9 },
            Request::Detach { handle: 9 },
            Request::Filled {
                object: ObjectId::new(3),
                pages: range(8, 4),
                outcome: Outcome::Done,
            },
            Request::WrittenBack {
                object: ObjectId::new(3),
                pages: range(0, MAX_RECORD_PAGES),
                outcome: Outcome::Failed(5),
            },
            Request::SetReadaheadMax { pages: 64 },
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
        // An empty range, a too long one, an errno above 16 bits, a zero readahead maximum.
        assert_eq!(Request::decode(&[4, 1, 0, 0, 0]), Err(DecodeError::Value));
        let long = u64::from(MAX_RECORD_PAGES) + 1;
        assert_eq!(
            Request::decode(&[4, 1, 0, long, 0]),
            Err(DecodeError::Value)
        );
        assert_eq!(
            Request::decode(&[5, 1, 0, 1, 0x1_0000]),
            Err(DecodeError::Value)
        );
        assert_eq!(Request::decode(&[6, 0]), Err(DecodeError::Value));
        assert_eq!(Request::decode(&[6, 1 << 32]), Err(DecodeError::Value));
    }

    #[test]
    fn records_round_trip() {
        let object = ObjectId::new(0xdead_beef);
        for record in [
            Record::Fill {
                object,
                pages: range(1, 2),
            },
            Record::Writeback {
                object,
                pages: range(u64::MAX - 8, 8),
            },
            Record::Release { object },
            Record::Accessed { object },
        ] {
            assert_eq!(Record::from_bytes(&record.to_bytes()), Ok(record));
            assert_eq!(record.object(), object);
        }
    }

    #[test]
    fn bad_records_are_refused() {
        let fill = Record::Fill {
            object: ObjectId::new(1),
            pages: range(4, 1),
        }
        .to_bytes();
        assert_eq!(Record::from_bytes(&fill[..31]), Err(RecordError::Length));
        let mut bad = fill;
        bad[0] = 9;
        assert_eq!(Record::from_bytes(&bad), Err(RecordError::Kind));
        let mut bad = fill;
        bad[5] = 1;
        assert_eq!(Record::from_bytes(&bad), Err(RecordError::Reserved));
        let mut bad = fill;
        bad[24..28].copy_from_slice(&0_u32.to_le_bytes());
        assert_eq!(Record::from_bytes(&bad), Err(RecordError::Pages));
        let mut release = Record::Release {
            object: ObjectId::new(1),
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
