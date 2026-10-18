//! The `regbuf` scheme's name, paths and call encoding.
//!
//! | Operation | On | Encoding |
//! | --- | --- | --- |
//! | open [`REGION_PATH`] | the scheme root | `open("/scheme/kernel.regbuf/region")` |
//! | [`Request::Register`] | a region | `SYS_CALL`, metadata `[verb, addr, len]` |
//! | open a grant | a region | `openat(region, GrantSpec::encode_path(..))` |
//! | [`Request::Revoke`] | a region | metadata `[verb, grant ID]` |
//! | [`Request::Info`] | a grant | metadata `[verb]`, payload [`GrantInfo::BYTES`] |
//! | [`Request::Unmap`] | a grant | metadata `[verb, addr]` |
//! | [`Request::Usage`] | the scheme root | metadata `[verb]`, payload [`Usage::BYTES`] |
//! | map a grant | a grant | `mmap(grant, offset, len, PROT_READ[\|PROT_WRITE], MAP_SHARED)` |
//!
//! Grants are byte-granular: a [`GrantSpec`] names any byte range of the region. A holder maps
//! its *window*, the pages the range touches ([`GrantSpec::window_offset`],
//! [`GrantSpec::window_len`]); `mmap` offsets count from the window's first page, and the
//! granted bytes start [`GrantSpec::lead`] bytes into it. Pages wholly inside the range are the
//! region's own pages. A first or last page the range covers only in part is a bounce page:
//! the kernel copies the granted bytes into a fresh page when the window is mapped (the rest of
//! that page reads as zero), and, for a writable mapping, copies them back into the region when
//! the mapping is taken down (`Unmap`, closing the grant, `Revoke`, closing the region), so the
//! application sees the scheme's writes to those bytes once it has revoked the grant. Bytes of
//! the region outside the range never reach the holder.
//!
//! [`Request::encode`] and [`Request::decode`] are the one place the metadata layout is written
//! down. The kernel reads at most three metadata words.
//!
//! Errors the kernel returns: `EINVAL` for a bad range or alignment, `EEXIST` when a region is
//! already registered, `EPERM` for memory that is not anonymous read-write memory (private or
//! `MAP_SHARED`) or that the caller only borrows from another process (a grant mapping or a
//! scheme request's buffer),
//! `EACCES` for a write mapping of a read-only grant, `ECANCELED` once a grant is revoked,
//! `ENOENT` for an unknown grant ID or mapping, and `EDQUOT` when pinning would exceed the
//! caller's limit.

use core::fmt;

/// The scheme's number among the kernel's global schemes (`GlobalSchemes` in the kernel's
/// `redox_syscall`), which bootstrap uses to find it among the kernel scheme capabilities it is
/// started with.
pub const SCHEME_ID: u8 = 13;

/// The scheme's name in the namespace: clients open `/scheme/kernel.regbuf`.
pub const SCHEME_NAME: &str = "kernel.regbuf";

/// The path that, opened from the scheme root, creates an empty region.
pub const REGION_PATH: &str = "region";

/// The granularity of regions and mappings, in bytes. Grants are byte-granular.
pub const PAGE_SIZE: u64 = 4096;

/// What a grant lets its holder do with the pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// Read the pages: the data of an application's `write`.
    Read,
    /// Read and write the pages: the buffer of an application's `read`.
    ReadWrite,
}

impl Access {
    /// Whether the holder may write the pages.
    pub const fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }

    /// Whether this access includes everything `other` allows.
    pub const fn covers(self, other: Self) -> bool {
        self.allows_write() || !other.allows_write()
    }

    /// The access's name in a grant path: `r` or `rw`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "r",
            Self::ReadWrite => "rw",
        }
    }

    const fn to_word(self) -> u64 {
        match self {
            Self::Read => 1,
            Self::ReadWrite => 3,
        }
    }

    const fn from_word(word: u64) -> Option<Self> {
        match word {
            1 => Some(Self::Read),
            3 => Some(Self::ReadWrite),
            _ => None,
        }
    }
}

/// A grant's identifier, unique within its region and never reused there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GrantId(u64);

impl GrantId {
    /// The ID with number `raw`.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The ID's number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for GrantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "grant {}", self.0)
    }
}

/// Why a range is not a valid grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpecError {
    /// The length is zero.
    Empty,
    /// The range, rounded out to whole pages, ends beyond `u64::MAX`.
    Overflow,
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "grant length is zero",
            Self::Overflow => "grant range overflows",
        })
    }
}

impl core::error::Error for SpecError {}

/// Why a path is not a grant path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathError {
    /// The path does not have the form `grant/<access>/<offset>/<len>`.
    Form,
    /// The access is neither `r` nor `rw`.
    Access,
    /// The offset or length is not a hexadecimal number that fits in 64 bits.
    Number,
    /// The numbers do not make a valid range.
    Spec(SpecError),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Form => f.write_str("not a grant path (grant/<r|rw>/<offset>/<len>)"),
            Self::Access => f.write_str("grant access is neither r nor rw"),
            Self::Number => f.write_str("grant offset or length is not a 64-bit hex number"),
            Self::Spec(_) => f.write_str("grant path names an invalid range"),
        }
    }
}

impl core::error::Error for PathError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Spec(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SpecError> for PathError {
    fn from(err: SpecError) -> Self {
        Self::Spec(err)
    }
}

/// The access and byte range, relative to the start of the region, of a grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GrantSpec {
    access: Access,
    offset: u64,
    len: u64,
}

impl GrantSpec {
    /// The longest path [`GrantSpec::encode_path`] writes.
    pub const PATH_MAX: usize = 48;

    /// `len` bytes from `offset` with `access`. Neither needs to be page-aligned.
    ///
    /// # Errors
    ///
    /// [`SpecError::Empty`] for a zero length and [`SpecError::Overflow`] when the range,
    /// rounded out to whole pages, ends beyond `u64::MAX`.
    pub const fn new(access: Access, offset: u64, len: u64) -> Result<Self, SpecError> {
        if len == 0 {
            return Err(SpecError::Empty);
        }
        let Some(end) = offset.checked_add(len) else {
            return Err(SpecError::Overflow);
        };
        if end.checked_next_multiple_of(PAGE_SIZE).is_none() {
            return Err(SpecError::Overflow);
        }
        Ok(Self {
            access,
            offset,
            len,
        })
    }

    /// What the grant allows.
    pub const fn access(&self) -> Access {
        self.access
    }

    /// The first byte, from the start of the region.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The length in bytes.
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Always false: a grant spans at least one byte.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The byte after the last, from the start of the region. [`GrantSpec::new`] checked it
    /// does not overflow.
    pub const fn end(&self) -> u64 {
        self.offset.saturating_add(self.len)
    }

    /// Whether the range lies inside a region of `region_len` bytes.
    pub const fn fits(&self, region_len: u64) -> bool {
        self.end() <= region_len
    }

    /// Whether the range starts and ends on page boundaries, so its window has no bounce
    /// pages.
    pub const fn is_page_aligned(&self) -> bool {
        self.offset.is_multiple_of(PAGE_SIZE) && self.len.is_multiple_of(PAGE_SIZE)
    }

    /// The first byte of the grant's window (its first page), from the start of the region.
    pub const fn window_offset(&self) -> u64 {
        self.offset - self.offset % PAGE_SIZE
    }

    /// The length of the grant's window: the whole pages the range touches.
    /// [`GrantSpec::new`] checked rounding up does not overflow.
    pub const fn window_len(&self) -> u64 {
        let end = match self.end().checked_next_multiple_of(PAGE_SIZE) {
            Some(end) => end,
            None => u64::MAX - u64::MAX % PAGE_SIZE,
        };
        end - self.window_offset()
    }

    /// How far into the window the granted bytes start: the offset within its first page.
    pub const fn lead(&self) -> u64 {
        self.offset % PAGE_SIZE
    }

    /// Writes the path that opens this grant from a region into `buf` and returns it:
    /// `grant/<r|rw>/<offset>/<len>`, numbers in lowercase hexadecimal.
    pub fn encode_path<'a>(&self, buf: &'a mut [u8; Self::PATH_MAX]) -> &'a str {
        let mut out = Cursor { buf, len: 0 };
        // The longest path is 9 + 16 + 1 + 16 bytes, well inside PATH_MAX, so no write is cut.
        let _ = fmt::write(
            &mut out,
            format_args!(
                "grant/{}/{:x}/{:x}",
                self.access.as_str(),
                self.offset,
                self.len
            ),
        );
        let len = out.len;
        let bytes = buf.get(..len).unwrap_or_default();
        core::str::from_utf8(bytes).unwrap_or_default()
    }

    /// Parses a path written by [`GrantSpec::encode_path`]. A leading `/` is ignored.
    ///
    /// # Errors
    ///
    /// [`PathError::Form`] when the path is not `grant/<access>/<offset>/<len>`,
    /// [`PathError::Access`] for an access other than `r` and `rw`, [`PathError::Number`] for an
    /// offset or length that is not hexadecimal or does not fit in 64 bits, and
    /// [`PathError::Spec`] when they do not make a valid range.
    pub fn parse_path(path: &[u8]) -> Result<Self, PathError> {
        let path = path.strip_prefix(b"/").unwrap_or(path);
        let rest = path.strip_prefix(b"grant/").ok_or(PathError::Form)?;
        let mut parts = rest.split(|&b| b == b'/');
        let (Some(access), Some(offset), Some(len), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(PathError::Form);
        };
        let access = match access {
            b"r" => Access::Read,
            b"rw" => Access::ReadWrite,
            _ => return Err(PathError::Access),
        };
        Ok(Self::new(access, parse_hex(offset)?, parse_hex(len)?)?)
    }
}

/// A `fmt::Write` sink into a fixed buffer.
struct Cursor<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl fmt::Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        let dst = self.buf.get_mut(self.len..end).ok_or(fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// A hexadecimal number of 1 to 16 digits.
fn parse_hex(digits: &[u8]) -> Result<u64, PathError> {
    if digits.is_empty() || digits.len() > 16 {
        return Err(PathError::Number);
    }
    digits.iter().try_fold(0_u64, |acc, &d| {
        let value = match d {
            b'0'..=b'9' => d - b'0',
            b'a'..=b'f' => d - b'a' + 10,
            b'A'..=b'F' => d - b'A' + 10,
            _ => return Err(PathError::Number),
        };
        Ok((acc << 4) | u64::from(value))
    })
}

/// A call's verb, the first metadata word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
#[non_exhaustive]
pub enum Verb {
    /// Pin memory into an empty region.
    Register = 1,
    /// Revoke a grant of the region.
    Revoke = 2,
    /// Describe a grant.
    Info = 3,
    /// Drop one of the caller's mappings of a grant.
    Unmap = 4,
    /// Report pinned-page accounting.
    Usage = 5,
}

impl Verb {
    const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            1 => Some(Self::Register),
            2 => Some(Self::Revoke),
            3 => Some(Self::Info),
            4 => Some(Self::Unmap),
            5 => Some(Self::Usage),
            _ => None,
        }
    }
}

/// A call on a regbuf descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// On an empty region: pin `len` bytes of the caller's memory from virtual address `addr`
    /// (both page-aligned) into it. The memory must be anonymous read-write memory, private or
    /// `MAP_SHARED`, of the caller's own: pages it only borrows from another process (a grant
    /// mapping, a scheme request's buffer) are refused with `EPERM`.
    Register {
        /// The first byte.
        addr: u64,
        /// The length in bytes.
        len: u64,
    },
    /// On a region: revoke a grant. Before the call returns every mapping of it is gone, and
    /// mapping it again fails with `ECANCELED`. Revoking a revoked grant does nothing.
    Revoke(GrantId),
    /// On a grant: write its [`GrantInfo`] into the payload.
    Info,
    /// On a grant: drop the caller's mapping of it that starts at `addr`. Mappings of a grant
    /// cannot be removed with `munmap` (`EBUSY`).
    Unmap {
        /// The mapping's first byte, as `mmap` returned it.
        addr: u64,
    },
    /// On the scheme root: write the caller's [`Usage`] into the payload.
    Usage,
}

impl Request {
    /// The call's metadata words, and how many of them are used.
    pub const fn encode(self) -> ([u64; 3], usize) {
        match self {
            Self::Register { addr, len } => ([Verb::Register as u64, addr, len], 3),
            Self::Revoke(id) => ([Verb::Revoke as u64, id.0, 0], 2),
            Self::Info => ([Verb::Info as u64, 0, 0], 1),
            Self::Unmap { addr } => ([Verb::Unmap as u64, addr, 0], 2),
            Self::Usage => ([Verb::Usage as u64, 0, 0], 1),
        }
    }

    /// The request in call metadata words.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb and [`DecodeError::Length`] when the
    /// number of words does not match the verb.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let (&verb, args) = words.split_first().ok_or(DecodeError::Verb)?;
        match (Verb::from_raw(verb).ok_or(DecodeError::Verb)?, args) {
            (Verb::Register, &[addr, len]) => Ok(Self::Register { addr, len }),
            (Verb::Revoke, &[id]) => Ok(Self::Revoke(GrantId(id))),
            (Verb::Info, &[]) => Ok(Self::Info),
            (Verb::Unmap, &[addr]) => Ok(Self::Unmap { addr }),
            (Verb::Usage, &[]) => Ok(Self::Usage),
            _ => Err(DecodeError::Length),
        }
    }
}

/// Why call metadata is not a valid [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The verb is missing or unknown.
    Verb,
    /// The number of words does not match the verb.
    Length,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Verb => "missing or unknown regbuf verb",
            Self::Length => "wrong number of metadata words for the regbuf verb",
        })
    }
}

impl core::error::Error for DecodeError {}

/// Why a payload is not a valid [`GrantInfo`] or [`Usage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BytesError {
    /// The payload has the wrong length.
    Length,
    /// A field has a value no kernel writes.
    Value,
}

impl fmt::Display for BytesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Length => "regbuf payload has the wrong length",
            Self::Value => "regbuf payload has an invalid field",
        })
    }
}

impl core::error::Error for BytesError {}

/// Reads little-endian word `index` of `bytes`, which has the right length.
fn word(bytes: &[u8], index: usize) -> u64 {
    let start = index.saturating_mul(8);
    let mut le = [0; 8];
    if let Some(src) = bytes.get(start..start.saturating_add(8)) {
        le.copy_from_slice(src);
    }
    u64::from_le_bytes(le)
}

/// Packs little-endian words.
fn words_to_bytes<const N: usize, const B: usize>(words: [u64; N]) -> [u8; B] {
    let mut out = [0; B];
    for (chunk, w) in out.chunks_exact_mut(8).zip(words) {
        chunk.copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// What [`Request::Info`] reports about a grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantInfo {
    /// The grant's ID, which the region's owner revokes it by.
    pub id: GrantId,
    /// Its access and range in the region.
    pub spec: GrantSpec,
    /// Whether it has been revoked.
    pub revoked: bool,
}

impl GrantInfo {
    /// The payload length: five little-endian 64-bit words (ID, access, offset, length,
    /// revoked).
    pub const BYTES: usize = 40;

    /// The payload bytes.
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        words_to_bytes([
            self.id.0,
            self.spec.access.to_word(),
            self.spec.offset,
            self.spec.len,
            u64::from(self.revoked),
        ])
    }

    /// Reads a payload written by [`GrantInfo::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`BytesError::Length`] when `bytes` is not [`GrantInfo::BYTES`] long and
    /// [`BytesError::Value`] for an unknown access, an invalid range or a revoked flag other
    /// than 0 or 1.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BytesError> {
        if bytes.len() != Self::BYTES {
            return Err(BytesError::Length);
        }
        let access = Access::from_word(word(bytes, 1)).ok_or(BytesError::Value)?;
        let spec = GrantSpec::new(access, word(bytes, 2), word(bytes, 3))
            .map_err(|_| BytesError::Value)?;
        let revoked = match word(bytes, 4) {
            0 => false,
            1 => true,
            _ => return Err(BytesError::Value),
        };
        Ok(Self {
            id: GrantId(word(bytes, 0)),
            spec,
            revoked,
        })
    }
}

/// What [`Request::Usage`] reports: pinned pages, counted in [`PAGE_SIZE`] pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// Pages the caller's user ID has pinned.
    pub pinned: u64,
    /// How many it may pin, or `None` without a per-user limit (root).
    pub limit: Option<u64>,
    /// Pages pinned by everyone.
    pub total: u64,
    /// How many everyone together may pin.
    pub total_limit: u64,
}

impl Usage {
    /// The payload length: four little-endian 64-bit words (pinned, limit or `u64::MAX`,
    /// total, total limit).
    pub const BYTES: usize = 32;

    /// The payload bytes.
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        words_to_bytes([
            self.pinned,
            self.limit.unwrap_or(u64::MAX),
            self.total,
            self.total_limit,
        ])
    }

    /// Reads a payload written by [`Usage::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`BytesError::Length`] when `bytes` is not [`Usage::BYTES`] long.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BytesError> {
        if bytes.len() != Self::BYTES {
            return Err(BytesError::Length);
        }
        let limit = word(bytes, 1);
        Ok(Self {
            pinned: word(bytes, 0),
            limit: (limit != u64::MAX).then_some(limit),
            total: word(bytes, 2),
            total_limit: word(bytes, 3),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(access: Access, offset: u64, len: u64) -> GrantSpec {
        GrantSpec::new(access, offset, len).expect("valid test range")
    }

    #[test]
    fn spec_checks_ranges() {
        assert_eq!(GrantSpec::new(Access::Read, 0, 0), Err(SpecError::Empty));
        assert_eq!(
            GrantSpec::new(Access::Read, u64::MAX - 4095, PAGE_SIZE * 2),
            Err(SpecError::Overflow)
        );
        // Ends inside the last page of the address range, which cannot be rounded up.
        assert_eq!(
            GrantSpec::new(Access::Read, u64::MAX - 10, 5),
            Err(SpecError::Overflow)
        );
        let s = spec(Access::ReadWrite, PAGE_SIZE, 2 * PAGE_SIZE);
        assert_eq!(s.end(), 3 * PAGE_SIZE);
        assert!(s.fits(3 * PAGE_SIZE));
        assert!(!s.fits(2 * PAGE_SIZE));
        assert!(!s.is_empty());
        assert!(s.is_page_aligned());
        assert_eq!(
            (s.window_offset(), s.window_len(), s.lead()),
            (PAGE_SIZE, 2 * PAGE_SIZE, 0)
        );
    }

    #[test]
    fn byte_granular_windows() {
        // Unaligned at both ends: three pages, starting 0x100 into the first.
        let s = spec(Access::ReadWrite, PAGE_SIZE + 0x100, 2 * PAGE_SIZE);
        assert!(!s.is_page_aligned());
        assert_eq!(s.window_offset(), PAGE_SIZE);
        assert_eq!(s.window_len(), 3 * PAGE_SIZE);
        assert_eq!(s.lead(), 0x100);
        // A few bytes inside one page.
        let s = spec(Access::Read, 0x10, 0x20);
        assert_eq!(
            (s.window_offset(), s.window_len(), s.lead()),
            (0, PAGE_SIZE, 0x10)
        );
        // Ends exactly on a page boundary.
        let s = spec(Access::Read, 0x800, 0x800);
        assert_eq!((s.window_offset(), s.window_len()), (0, PAGE_SIZE));
        // Near the top of the address range: the window ends at its last page.
        let s = spec(Access::Read, u64::MAX - 12287, PAGE_SIZE + 1);
        assert_eq!(s.window_len(), 2 * PAGE_SIZE);
    }

    #[test]
    fn access_cover() {
        assert!(Access::ReadWrite.covers(Access::Read));
        assert!(Access::ReadWrite.covers(Access::ReadWrite));
        assert!(Access::Read.covers(Access::Read));
        assert!(!Access::Read.covers(Access::ReadWrite));
    }

    #[test]
    fn path_round_trip() {
        let mut buf = [0; GrantSpec::PATH_MAX];
        for s in [
            spec(Access::Read, 0, PAGE_SIZE),
            spec(Access::ReadWrite, 0xabc000, 0x10000),
            spec(Access::ReadWrite, 0, u64::MAX - 4095),
            spec(Access::Read, u64::MAX - 8191, PAGE_SIZE),
            spec(Access::ReadWrite, 0x123, 0x4567),
        ] {
            let path = s.encode_path(&mut buf);
            assert!(path.starts_with("grant/"));
            assert_eq!(GrantSpec::parse_path(path.as_bytes()), Ok(s), "{path}");
        }
        assert_eq!(
            GrantSpec::parse_path(b"/grant/r/1000/2000"),
            Ok(spec(Access::Read, 0x1000, 0x2000))
        );
        assert_eq!(
            GrantSpec::parse_path(b"grant/RW/0/1000"),
            Err(PathError::Access)
        );
    }

    #[test]
    fn path_rejects_garbage() {
        for (path, err) in [
            (&b"region"[..], PathError::Form),
            (b"grant/r/0", PathError::Form),
            (b"grant/r/0/1000/", PathError::Form),
            (b"grant/r/0/1000/x", PathError::Form),
            (b"grant/w/0/1000", PathError::Access),
            (b"grant/r//1000", PathError::Number),
            (b"grant/r/0/10000000000000000", PathError::Number),
            (b"grant/r/0x0/1000", PathError::Number),
            (b"grant/r/0/0", PathError::Spec(SpecError::Empty)),
            (
                b"grant/r/fffffffffffffff0/8",
                PathError::Spec(SpecError::Overflow),
            ),
        ] {
            assert_eq!(GrantSpec::parse_path(path), Err(err), "{path:?}");
        }
    }

    #[test]
    fn request_round_trip() {
        for request in [
            Request::Register {
                addr: 0x7000_0000,
                len: 0x4000,
            },
            Request::Revoke(GrantId::new(42)),
            Request::Info,
            Request::Unmap { addr: 0x1000 },
            Request::Usage,
        ] {
            let (words, len) = request.encode();
            assert_eq!(Request::decode(&words[..len]), Ok(request));
        }
    }

    #[test]
    fn request_rejects_bad_metadata() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[0]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[99, 1]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[1, 0x1000]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[3, 0]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[2]), Err(DecodeError::Length));
    }

    #[test]
    fn info_round_trip() {
        let info = GrantInfo {
            id: GrantId::new(5),
            spec: spec(Access::ReadWrite, PAGE_SIZE, PAGE_SIZE),
            revoked: true,
        };
        assert_eq!(GrantInfo::from_bytes(&info.to_bytes()), Ok(info));
        assert_eq!(GrantInfo::from_bytes(&[0; 8]), Err(BytesError::Length));
        let mut bad = info.to_bytes();
        bad[8] = 2;
        assert_eq!(GrantInfo::from_bytes(&bad), Err(BytesError::Value));
        let mut bad = info.to_bytes();
        bad[32] = 7;
        assert_eq!(GrantInfo::from_bytes(&bad), Err(BytesError::Value));
    }

    #[test]
    fn usage_round_trip() {
        for usage in [
            Usage {
                pinned: 3,
                limit: Some(4096),
                total: 10,
                total_limit: 1 << 20,
            },
            Usage {
                pinned: 0,
                limit: None,
                total: 0,
                total_limit: 0,
            },
        ] {
            assert_eq!(Usage::from_bytes(&usage.to_bytes()), Ok(usage));
        }
        assert_eq!(Usage::from_bytes(&[0; 31]), Err(BytesError::Length));
    }
}
