//! Host-visible trace rings (`kernel.trace_host`): the region a host reads a guest's rings
//! from, and the directory page that describes it.
//!
//! A kernel built with `trace_host` and booted with `FERRIC_TRACE_HOST=<KiB per CPU>` puts
//! every CPU's ring into one physically contiguous region of RAM, laid out by
//! [`RegionLayout`]:
//!
//! | Bytes from the region's start | Content |
//! | --- | --- |
//! | 0 | the directory, one page of [`DIRECTORY_BYTES`] |
//! | [`DIRECTORY_BYTES`] | CPU 0's ring: a [shared-layout](crate::ring#storage) header page, then the data |
//! | after it | CPU 1's ring, and so on, back to back |
//!
//! Each ring overwrites its oldest records when it is full
//! ([`WhenFull::OverwriteOldest`](crate::ring::WhenFull::OverwriteOldest)), so the kernel
//! never waits for a reader and a reader can never hold the kernel up. A host that sees the
//! guest's RAM (QEMU's `memory-backend-file` with `share=on`) finds the directory, maps it
//! read-only and follows each ring with an [`Observer`](crate::ring::Observer), which keeps
//! its own position and stores nothing. The host is these rings' only reader: the kernel
//! refuses reads and commits of them through `kernel.trace/cpu/<n>` (`EBUSY`), because a
//! guest consumer would move the `tail` an overtaken observer takes as its oldest whole
//! record.
//!
//! # The directory
//!
//! The directory is a same-machine, in-memory interface: [`DirectoryHeader`] and then one
//! [`RingEntry`] per CPU, `#[repr(C)]` 64-bit words in the CPU's own byte order:
//!
//! | Word | Field |
//! | --- | --- |
//! | 0 | [`DIRECTORY_MAGIC`] |
//! | 1 | [`DIRECTORY_VERSION`] |
//! | 2 | the directory's own physical address |
//! | 3 | the boot generation: a value that differs from one boot to the next |
//! | 4 | the number of rings, one per CPU, at most [`MAX_RINGS`] |
//! | 5 | the trace clock, [`Clock::raw`] |
//! | 6 | the clock's tick length in femtoseconds, 0 when unknown |
//! | 7 | reserved, 0 |
//! | 8 + 2n | ring n's offset in bytes from the directory's start |
//! | 9 + 2n | ring n's length in bytes, its header page included |
//!
//! The clock fields hold what the capture directory's `clock` file holds
//! ([`ClockLine`]). The kernel writes the directory once, after the clock is chosen and
//! every ring's header is written ([`publish`]): every word but the magic first, then the
//! magic with release ordering, so a reader that sees the magic and then fences sees the
//! rest. A page that holds the magic is a directory only if its address field is the
//! address it was found at: a copy of the page elsewhere (or a stale one from an earlier
//! boot, which the generation tells apart) does not pass. [`Directory::read`] checks the
//! rest.
//!
//! # Examples
//!
//! Lay out a region for two CPUs, publish its directory and read it back:
//!
//! ```
//! use core::sync::atomic::AtomicU64;
//! use ferric_trace::clock::{Clock, ClockLine};
//! use ferric_trace::host::{self, Directory, Published, RegionLayout, RingSize};
//!
//! let layout = RegionLayout::new(2, RingSize::from_kib(64).expect("a power of two"))
//!     .expect("fits");
//! let page: Vec<AtomicU64> = (0..host::DIRECTORY_WORDS).map(|_| AtomicU64::new(0)).collect();
//! let clock = ClockLine { clock: Clock::KvmClock, period_fs: Some(1_000_000) };
//! host::publish(&page[..], &Published { self_phys: 0x10_0000, generation: 7, clock }, layout.entries())
//!     .expect("two rings fit");
//! let directory = Directory::read(&page[..]).expect("valid");
//! assert_eq!(directory.ring_phys(1), Some((0x10_0000 + 4096 + 69632, 69632)));
//! ```

use core::fmt;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{Ordering, fence};

use crate::abi::LineError;
use crate::clock::{Clock, ClockLine};
use crate::record::WORD_BYTES;
use crate::ring::{SHARED_HEADER_WORDS, Words};

/// The directory's first word: `b"FOTRDIR1"` read as a little-endian word, so the page
/// starts with those bytes on the little-endian machines the kernel runs on.
pub const DIRECTORY_MAGIC: u64 = u64::from_le_bytes(*b"FOTRDIR1");

/// The directory format's version, at word 1.
pub const DIRECTORY_VERSION: u64 = 1;

/// The directory's size: one 4 KiB page, which the rings follow.
pub const DIRECTORY_BYTES: usize = 4096;

/// The directory's size in words.
pub const DIRECTORY_WORDS: usize = DIRECTORY_BYTES / WORD_BYTES;

/// The words of [`DirectoryHeader`].
pub const HEADER_WORDS: usize = size_of::<DirectoryHeader>() / WORD_BYTES;

/// The words of one [`RingEntry`].
pub const ENTRY_WORDS: usize = size_of::<RingEntry>() / WORD_BYTES;

/// The most rings a directory lists: as many entries as fit in the page after the header.
pub const MAX_RINGS: usize = (DIRECTORY_WORDS - HEADER_WORDS) / ENTRY_WORDS;

/// The bytes of a ring's shared-layout header page, ahead of its data.
pub const RING_HEADER_BYTES: usize = SHARED_HEADER_WORDS * WORD_BYTES;

/// The smallest ring [`RingSize`] accepts, in KiB of data: one page.
pub const MIN_RING_KIB: u32 = 4;

/// The largest ring [`RingSize`] accepts, in KiB of data: 1 GiB per CPU.
pub const MAX_RING_KIB: u32 = 1 << 20;

/// The fixed words at the start of the directory; see the [module docs](self#the-directory).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectoryHeader {
    /// [`DIRECTORY_MAGIC`] once the directory is complete, 0 before.
    pub magic: u64,
    /// [`DIRECTORY_VERSION`].
    pub version: u64,
    /// The directory's own physical address.
    pub self_phys: u64,
    /// A value that differs from one boot to the next.
    pub generation: u64,
    /// The number of [`RingEntry`]s that follow, one per CPU.
    pub cpus: u64,
    /// The trace clock ([`Clock::raw`]).
    pub clock: u64,
    /// The clock's tick length in femtoseconds, 0 when unknown.
    pub period_fs: u64,
    /// Reserved, 0.
    pub reserved: u64,
}

/// Where one CPU's ring is, relative to the directory.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingEntry {
    /// The ring's offset in bytes from the directory's start.
    pub offset: u64,
    /// The ring's length in bytes, its header page included.
    pub bytes: u64,
}

const _: () = assert!(size_of::<DirectoryHeader>() == 8 * WORD_BYTES);
const _: () = assert!(offset_of!(DirectoryHeader, self_phys) == 2 * WORD_BYTES);
const _: () = assert!(offset_of!(DirectoryHeader, period_fs) == 6 * WORD_BYTES);
const _: () = assert!(size_of::<RingEntry>() == 2 * WORD_BYTES);
const _: () = assert!(MAX_RINGS == 252);

impl DirectoryHeader {
    /// The header as its words, in order.
    pub const fn to_words(&self) -> [u64; HEADER_WORDS] {
        [
            self.magic,
            self.version,
            self.self_phys,
            self.generation,
            self.cpus,
            self.clock,
            self.period_fs,
            self.reserved,
        ]
    }

    /// The header made of its words.
    pub const fn from_words(words: [u64; HEADER_WORDS]) -> Self {
        let [
            magic,
            version,
            self_phys,
            generation,
            cpus,
            clock,
            period_fs,
            reserved,
        ] = words;
        Self {
            magic,
            version,
            self_phys,
            generation,
            cpus,
            clock,
            period_fs,
            reserved,
        }
    }
}

/// A ring's data size per CPU, as `FERRIC_TRACE_HOST=<KiB>` gives it: a power of two from
/// [`MIN_RING_KIB`] to [`MAX_RING_KIB`] KiB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingSize {
    kib: u32,
}

/// Why a ring size was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RingSizeError {
    /// The value is not a decimal number.
    NotNumber,
    /// The size is not a power of two.
    NotPowerOfTwo {
        /// The size in KiB.
        kib: u32,
    },
    /// The size is below [`MIN_RING_KIB`].
    TooSmall {
        /// The size in KiB.
        kib: u32,
    },
    /// The size is above [`MAX_RING_KIB`].
    TooLarge {
        /// The size in KiB.
        kib: u32,
    },
}

impl fmt::Display for RingSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RingSizeError::NotNumber => f.write_str("not a number of KiB"),
            RingSizeError::NotPowerOfTwo { kib } => {
                write!(f, "{kib} KiB is not a power of two")
            }
            RingSizeError::TooSmall { kib } => {
                write!(
                    f,
                    "{kib} KiB is below the smallest ring, {MIN_RING_KIB} KiB"
                )
            }
            RingSizeError::TooLarge { kib } => {
                write!(f, "{kib} KiB is above the largest ring, {MAX_RING_KIB} KiB")
            }
        }
    }
}

impl core::error::Error for RingSizeError {}

impl RingSize {
    /// A ring of `kib` KiB of data.
    ///
    /// # Errors
    ///
    /// [`RingSizeError`] when `kib` is not a power of two from [`MIN_RING_KIB`] to
    /// [`MAX_RING_KIB`].
    pub const fn from_kib(kib: u32) -> Result<Self, RingSizeError> {
        if kib < MIN_RING_KIB {
            Err(RingSizeError::TooSmall { kib })
        } else if kib > MAX_RING_KIB {
            Err(RingSizeError::TooLarge { kib })
        } else if !kib.is_power_of_two() {
            Err(RingSizeError::NotPowerOfTwo { kib })
        } else {
            Ok(Self { kib })
        }
    }

    /// Parses a boot environment value: a decimal number of KiB, surrounding spaces ignored.
    ///
    /// # Errors
    ///
    /// [`RingSizeError::NotNumber`], or as [`RingSize::from_kib`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_trace::host::{RingSize, RingSizeError};
    ///
    /// assert_eq!(RingSize::parse("4096").map(|s| s.kib()), Ok(4096));
    /// assert_eq!(RingSize::parse("3000"), Err(RingSizeError::NotPowerOfTwo { kib: 3000 }));
    /// assert_eq!(RingSize::parse("4M"), Err(RingSizeError::NotNumber));
    /// ```
    pub fn parse(text: &str) -> Result<Self, RingSizeError> {
        let kib = text
            .trim()
            .parse::<u32>()
            .map_err(|_| RingSizeError::NotNumber)?;
        Self::from_kib(kib)
    }

    /// The size in KiB.
    pub const fn kib(self) -> u32 {
        self.kib
    }

    /// The data's size in bytes.
    pub const fn data_bytes(self) -> usize {
        self.kib as usize * 1024
    }

    /// The data's size in words.
    pub const fn data_words(self) -> usize {
        self.data_bytes() / WORD_BYTES
    }

    /// A whole ring's size in bytes: its header page and the data.
    pub const fn ring_bytes(self) -> usize {
        RING_HEADER_BYTES + self.data_bytes()
    }
}

/// The layout of a region: the directory, then `cpus` rings of one [`RingSize`] each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionLayout {
    cpus: usize,
    size: RingSize,
}

/// Why a region cannot be laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutError {
    /// No CPUs, or more than [`MAX_RINGS`].
    Cpus {
        /// The number of CPUs asked for.
        cpus: usize,
    },
    /// The region's size does not fit in an address.
    TooLarge,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::Cpus { cpus } => {
                write!(f, "{cpus} CPUs: a directory lists 1 to {MAX_RINGS} rings")
            }
            LayoutError::TooLarge => f.write_str("the region does not fit in the address space"),
        }
    }
}

impl core::error::Error for LayoutError {}

impl RegionLayout {
    /// The layout of `cpus` rings of `size`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Cpus`] for no CPUs or more than [`MAX_RINGS`], and
    /// [`LayoutError::TooLarge`] when the region's size does not fit in a `usize`.
    pub fn new(cpus: usize, size: RingSize) -> Result<Self, LayoutError> {
        if cpus == 0 || cpus > MAX_RINGS {
            return Err(LayoutError::Cpus { cpus });
        }
        size.ring_bytes()
            .checked_mul(cpus)
            .and_then(|rings| rings.checked_add(DIRECTORY_BYTES))
            .ok_or(LayoutError::TooLarge)?;
        Ok(Self { cpus, size })
    }

    /// The number of rings.
    pub const fn cpus(&self) -> usize {
        self.cpus
    }

    /// Each ring's size.
    pub const fn ring_size(&self) -> RingSize {
        self.size
    }

    /// The region's size in bytes: the directory and every ring.
    pub const fn bytes(&self) -> usize {
        // `new` checked that this does not overflow.
        DIRECTORY_BYTES + self.cpus * self.size.ring_bytes()
    }

    /// CPU `cpu`'s ring, or `None` past the last.
    pub const fn entry(&self, cpu: usize) -> Option<RingEntry> {
        if cpu >= self.cpus {
            return None;
        }
        Some(RingEntry {
            offset: (DIRECTORY_BYTES + cpu * self.size.ring_bytes()) as u64,
            bytes: self.size.ring_bytes() as u64,
        })
    }

    /// Every CPU's ring, in CPU order.
    pub fn entries(&self) -> impl Iterator<Item = RingEntry> + Clone + use<> {
        let layout = *self;
        (0..layout.cpus).filter_map(move |cpu| layout.entry(cpu))
    }
}

/// What [`publish`] writes besides the rings: where the directory is, the boot and the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    /// The directory's physical address.
    pub self_phys: u64,
    /// A value that differs from one boot to the next.
    pub generation: u64,
    /// The trace clock.
    pub clock: ClockLine,
}

/// Why a directory could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublishError {
    /// The words are fewer than [`DIRECTORY_WORDS`].
    Short {
        /// The number of words.
        len: usize,
    },
    /// More rings than [`MAX_RINGS`], or none.
    Rings {
        /// The number of rings, counted up to one past the limit.
        count: usize,
    },
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::Short { len } => {
                write!(
                    f,
                    "{len} words cannot hold a {DIRECTORY_WORDS}-word directory"
                )
            }
            PublishError::Rings { count } => {
                write!(f, "{count} rings: a directory lists 1 to {MAX_RINGS}")
            }
        }
    }
}

impl core::error::Error for PublishError {}

/// Writes the directory for `rings` into `words`, the directory's page: the entries and
/// header first, the magic last, with release ordering, so a reader that sees the magic sees
/// a complete directory. Call it once the rings' headers are written.
///
/// # Errors
///
/// [`PublishError::Short`] when `words` has fewer than [`DIRECTORY_WORDS`] words, and
/// [`PublishError::Rings`] for no rings or more than [`MAX_RINGS`]; the magic is not written
/// then.
pub fn publish<W: Words>(
    words: W,
    published: &Published,
    rings: impl IntoIterator<Item = RingEntry>,
) -> Result<(), PublishError> {
    if words.len() < DIRECTORY_WORDS {
        return Err(PublishError::Short { len: words.len() });
    }
    let mut count = 0;
    for entry in rings {
        if count == MAX_RINGS {
            return Err(PublishError::Rings {
                count: MAX_RINGS + 1,
            });
        }
        let at = HEADER_WORDS + count * ENTRY_WORDS;
        words.word(at).store(entry.offset, Ordering::Relaxed);
        words.word(at + 1).store(entry.bytes, Ordering::Relaxed);
        count += 1;
    }
    if count == 0 {
        return Err(PublishError::Rings { count });
    }
    let header = DirectoryHeader {
        magic: DIRECTORY_MAGIC,
        version: DIRECTORY_VERSION,
        self_phys: published.self_phys,
        generation: published.generation,
        cpus: count as u64,
        clock: u64::from(published.clock.clock.raw()),
        period_fs: published.clock.period_fs.unwrap_or(0),
        reserved: 0,
    };
    for (index, value) in header.to_words().into_iter().enumerate().skip(1) {
        words.word(index).store(value, Ordering::Relaxed);
    }
    words.word(0).store(header.magic, Ordering::Release);
    Ok(())
}

/// Whether `words` start with [`DIRECTORY_MAGIC`]: the cheap first test when scanning memory
/// for a directory. [`Directory::read`] decides.
pub fn has_magic<W: Words>(words: W) -> bool {
    !words.is_empty() && words.word(0).load(Ordering::Relaxed) == DIRECTORY_MAGIC
}

/// A directory, as [`Directory::read`] found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Directory {
    header: DirectoryHeader,
    clock: ClockLine,
    rings: [RingEntry; MAX_RINGS],
}

/// Why words are not a directory this reader understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DirectoryError {
    /// The words are fewer than [`DIRECTORY_WORDS`].
    Short {
        /// The number of words.
        len: usize,
    },
    /// The first word is not [`DIRECTORY_MAGIC`]: no directory, or not one yet.
    Magic,
    /// The format version is not [`DIRECTORY_VERSION`].
    Version {
        /// The version found.
        found: u64,
    },
    /// The ring count is 0 or above [`MAX_RINGS`].
    Cpus {
        /// The count found.
        found: u64,
    },
    /// The clock is not one this reader knows.
    Clock {
        /// The clock number found.
        raw: u64,
    },
    /// A ring's place overlaps the directory, is not whole words, or does not fit in an
    /// address.
    Entry {
        /// The CPU.
        cpu: usize,
        /// The entry.
        entry: RingEntry,
    },
}

impl fmt::Display for DirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirectoryError::Short { len } => {
                write!(
                    f,
                    "{len} words cannot hold a {DIRECTORY_WORDS}-word directory"
                )
            }
            DirectoryError::Magic => f.write_str("not a trace ring directory (bad magic)"),
            DirectoryError::Version { found } => write!(
                f,
                "trace ring directory version {found}, this reader knows {DIRECTORY_VERSION}"
            ),
            DirectoryError::Cpus { found } => {
                write!(f, "{found} rings: a directory lists 1 to {MAX_RINGS}")
            }
            DirectoryError::Clock { raw } => write!(f, "unknown trace clock {raw}"),
            DirectoryError::Entry { cpu, entry } => write!(
                f,
                "CPU {cpu}'s ring at offset {} of {} bytes is not a ring's place",
                entry.offset, entry.bytes
            ),
        }
    }
}

impl core::error::Error for DirectoryError {}

impl Directory {
    /// Reads the directory in `words`, which may be changing under the reader (guest memory
    /// the guest still writes): every word is loaded once, relaxed (which is allowed on
    /// memory mapped read-only), with an acquire fence after the magic.
    ///
    /// It checks the format, not where the words were found: compare
    /// [`Directory::self_phys`] with the address the page was read at.
    ///
    /// # Errors
    ///
    /// [`DirectoryError`] for words that are not a complete directory of this version.
    pub fn read<W: Words>(words: W) -> Result<Self, DirectoryError> {
        if words.len() < DIRECTORY_WORDS {
            return Err(DirectoryError::Short { len: words.len() });
        }
        if words.word(0).load(Ordering::Relaxed) != DIRECTORY_MAGIC {
            return Err(DirectoryError::Magic);
        }
        // Pairs with the release store of the magic in `publish`.
        fence(Ordering::Acquire);
        let mut fields = [0; HEADER_WORDS];
        for (index, field) in fields.iter_mut().enumerate() {
            *field = words.word(index).load(Ordering::Relaxed);
        }
        let header = DirectoryHeader::from_words(fields);
        if header.version != DIRECTORY_VERSION {
            return Err(DirectoryError::Version {
                found: header.version,
            });
        }
        let cpus = usize::try_from(header.cpus)
            .ok()
            .filter(|&cpus| (1..=MAX_RINGS).contains(&cpus))
            .ok_or(DirectoryError::Cpus { found: header.cpus })?;
        let clock = u8::try_from(header.clock)
            .ok()
            .and_then(Clock::from_raw)
            .ok_or(DirectoryError::Clock { raw: header.clock })?;
        let mut rings = [RingEntry::default(); MAX_RINGS];
        for (cpu, ring) in rings.iter_mut().enumerate().take(cpus) {
            let at = HEADER_WORDS + cpu * ENTRY_WORDS;
            *ring = RingEntry {
                offset: words.word(at).load(Ordering::Relaxed),
                bytes: words.word(at + 1).load(Ordering::Relaxed),
            };
            if !is_place(header.self_phys, *ring) {
                return Err(DirectoryError::Entry { cpu, entry: *ring });
            }
        }
        Ok(Self {
            header,
            clock: ClockLine {
                clock,
                period_fs: (header.period_fs != 0).then_some(header.period_fs),
            },
            rings,
        })
    }

    /// The header as read.
    pub fn header(&self) -> &DirectoryHeader {
        &self.header
    }

    /// The physical address the directory says it is at.
    pub fn self_phys(&self) -> u64 {
        self.header.self_phys
    }

    /// The boot generation.
    pub fn generation(&self) -> u64 {
        self.header.generation
    }

    /// The trace clock: what a capture's `clock` file holds.
    pub fn clock(&self) -> ClockLine {
        self.clock
    }

    /// The rings, one per CPU in CPU order.
    pub fn rings(&self) -> &[RingEntry] {
        &self.rings[..self.cpu_count()]
    }

    /// The number of rings.
    pub fn cpu_count(&self) -> usize {
        // `read` checked the count against MAX_RINGS.
        self.header.cpus as usize
    }

    /// CPU `cpu`'s ring as its physical address and length in bytes, or `None` past the
    /// last CPU.
    pub fn ring_phys(&self, cpu: usize) -> Option<(u64, u64)> {
        let entry = self.rings().get(cpu)?;
        // `read` checked that the sum does not overflow.
        Some((self.header.self_phys + entry.offset, entry.bytes))
    }
}

/// Whether `entry` is a ring's place in a directory at `self_phys`: after the directory,
/// whole words, holding more than a header, and inside the address space.
fn is_place(self_phys: u64, entry: RingEntry) -> bool {
    let words = WORD_BYTES as u64;
    entry.offset >= DIRECTORY_BYTES as u64
        && entry.offset.is_multiple_of(words)
        && entry.bytes.is_multiple_of(words)
        && entry.bytes > RING_HEADER_BYTES as u64
        && self_phys
            .checked_add(entry.offset)
            .and_then(|start| start.checked_add(entry.bytes))
            .is_some()
}

/// One line of a host follower's `host-clock` file, a sample that ties the host's clock to
/// the guest's trace clock: `host_ns <ns> cpu <n> head <position> timestamp <ticks>`.
///
/// The host read CPU `cpu`'s ring up to position `head` at `host_ns` (its
/// `CLOCK_MONOTONIC`), and the record that ends there has timestamp `timestamp`. The record
/// was written between the host's previous read of that ring and `host_ns`, so a pair of
/// samples bounds the offset between the two clocks to within one poll interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSampleLine {
    /// The host's `CLOCK_MONOTONIC`, in nanoseconds.
    pub host_ns: u64,
    /// The CPU whose ring was read.
    pub cpu: u32,
    /// The ring position the host had read to.
    pub head: u64,
    /// The timestamp of the record that ends at `head`, in the trace clock's ticks.
    pub timestamp: u64,
}

impl ClockSampleLine {
    /// Parses a line, with or without its `\n`.
    ///
    /// # Errors
    ///
    /// [`LineError::Fields`] or [`LineError::Number`].
    pub fn parse(line: &str) -> Result<Self, LineError> {
        let [
            host_tag,
            host_ns,
            cpu_tag,
            cpu,
            head_tag,
            head,
            ts_tag,
            timestamp,
        ] = crate::abi::fields(line)?;
        if (host_tag, cpu_tag, head_tag, ts_tag) != ("host_ns", "cpu", "head", "timestamp") {
            return Err(LineError::Fields);
        }
        Ok(Self {
            host_ns: crate::abi::number(host_ns)?,
            cpu: crate::abi::number(cpu)?,
            head: crate::abi::number(head)?,
            timestamp: crate::abi::number(timestamp)?,
        })
    }
}

impl fmt::Display for ClockSampleLine {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "host_ns {} cpu {} head {} timestamp {}",
            self.host_ns, self.cpu, self.head, self.timestamp
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU64;
    use std::string::ToString;
    use std::vec::Vec;

    fn page() -> Vec<AtomicU64> {
        (0..DIRECTORY_WORDS).map(|_| AtomicU64::new(0)).collect()
    }

    fn published() -> Published {
        Published {
            self_phys: 0x0123_4000,
            generation: 0x0bad_cafe,
            clock: ClockLine {
                clock: Clock::Hpet,
                period_fs: Some(10_000_000),
            },
        }
    }

    fn bytes(words: &[AtomicU64]) -> Vec<u8> {
        words
            .iter()
            .flat_map(|w| w.load(Ordering::Relaxed).to_ne_bytes())
            .collect()
    }

    /// The directory of two 4 KiB rings, byte for byte as a little-endian machine holds it.
    #[cfg(target_endian = "little")]
    #[test]
    fn directory_bytes_are_fixed() {
        let words = page();
        let layout = RegionLayout::new(2, RingSize::from_kib(4).expect("valid")).expect("fits");
        publish(&words[..], &published(), layout.entries()).expect("fits");
        let bytes = bytes(&words);
        #[rustfmt::skip]
        let expected: [u8; 96] = [
            b'F', b'O', b'T', b'R', b'D', b'I', b'R', b'1', // magic
            0x01, 0, 0, 0, 0, 0, 0, 0,                       // version
            0x00, 0x40, 0x23, 0x01, 0, 0, 0, 0,              // self_phys 0x0123_4000
            0xfe, 0xca, 0xad, 0x0b, 0, 0, 0, 0,              // generation 0x0bad_cafe
            0x02, 0, 0, 0, 0, 0, 0, 0,                       // cpus
            0x02, 0, 0, 0, 0, 0, 0, 0,                       // clock: hpet
            0x80, 0x96, 0x98, 0x00, 0, 0, 0, 0,              // period_fs 10_000_000
            0, 0, 0, 0, 0, 0, 0, 0,                          // reserved
            0x00, 0x10, 0, 0, 0, 0, 0, 0,                    // ring 0 offset 4096
            0x00, 0x20, 0, 0, 0, 0, 0, 0,                    // ring 0 bytes 8192
            0x00, 0x30, 0, 0, 0, 0, 0, 0,                    // ring 1 offset 12288
            0x00, 0x20, 0, 0, 0, 0, 0, 0,                    // ring 1 bytes 8192
        ];
        assert_eq!(bytes[..96], expected);
        assert!(bytes[96..].iter().all(|&b| b == 0), "the rest is zero");
    }

    #[test]
    fn a_published_directory_reads_back() {
        let words = page();
        let layout = RegionLayout::new(3, RingSize::from_kib(16).expect("valid")).expect("fits");
        publish(&words[..], &published(), layout.entries()).expect("fits");
        assert!(has_magic(&words[..]));
        let directory = Directory::read(&words[..]).expect("valid");
        assert_eq!(directory.self_phys(), 0x0123_4000);
        assert_eq!(directory.generation(), 0x0bad_cafe);
        assert_eq!(directory.clock(), published().clock);
        assert_eq!(directory.cpu_count(), 3);
        assert_eq!(directory.rings().len(), 3);
        let ring = 4096 + 16 * 1024;
        assert_eq!(
            directory.ring_phys(2),
            Some((0x0123_4000 + 4096 + 2 * ring, ring))
        );
        assert_eq!(directory.ring_phys(3), None);
        assert_eq!(layout.bytes(), 4096 + 3 * ring as usize);
    }

    #[test]
    fn an_unknown_period_reads_as_none() {
        let words = page();
        let mut published = published();
        published.clock = ClockLine {
            clock: Clock::Tsc,
            period_fs: None,
        };
        publish(
            &words[..],
            &published,
            [RingEntry {
                offset: 4096,
                bytes: 8192,
            }],
        )
        .expect("fits");
        assert_eq!(words[6].load(Ordering::Relaxed), 0);
        let directory = Directory::read(&words[..]).expect("valid");
        assert_eq!(directory.clock(), published.clock);
    }

    #[test]
    fn bad_directories_are_refused() {
        let words = page();
        assert_eq!(Directory::read(&words[..]), Err(DirectoryError::Magic));
        assert_eq!(
            Directory::read(&words[..8]),
            Err(DirectoryError::Short { len: 8 })
        );
        let layout = RegionLayout::new(1, RingSize::from_kib(4).expect("valid")).expect("fits");
        let set = |index: usize, value: u64| {
            let words = page();
            publish(&words[..], &published(), layout.entries()).expect("fits");
            words[index].store(value, Ordering::Relaxed);
            Directory::read(&words[..])
        };
        assert_eq!(set(1, 2), Err(DirectoryError::Version { found: 2 }));
        assert_eq!(set(4, 0), Err(DirectoryError::Cpus { found: 0 }));
        assert_eq!(set(4, 253), Err(DirectoryError::Cpus { found: 253 }));
        assert_eq!(set(5, 9), Err(DirectoryError::Clock { raw: 9 }));
        let entry = |offset, bytes| DirectoryError::Entry {
            cpu: 0,
            entry: RingEntry { offset, bytes },
        };
        assert_eq!(set(8, 0), Err(entry(0, 8192)), "inside the directory");
        assert_eq!(set(8, 4097), Err(entry(4097, 8192)), "not whole words");
        assert_eq!(set(9, 4096), Err(entry(4096, 4096)), "a header alone");
        assert_eq!(
            set(9, u64::MAX - 7),
            Err(entry(4096, u64::MAX - 7)),
            "past the address space"
        );
    }

    #[test]
    fn publish_refuses_what_does_not_fit() {
        let words = page();
        let none: [RingEntry; 0] = [];
        assert_eq!(
            publish(&words[..], &published(), none),
            Err(PublishError::Rings { count: 0 })
        );
        let entry = RingEntry {
            offset: 4096,
            bytes: 8192,
        };
        assert_eq!(
            publish(&words[..], &published(), core::iter::repeat_n(entry, 253)),
            Err(PublishError::Rings { count: 253 })
        );
        assert_eq!(words[0].load(Ordering::Relaxed), 0, "no magic");
        assert_eq!(
            publish(&words[..100], &published(), [entry]),
            Err(PublishError::Short { len: 100 })
        );
        assert!(publish(&words[..], &published(), core::iter::repeat_n(entry, 252)).is_ok());
    }

    #[test]
    fn ring_sizes_are_checked() {
        assert_eq!(RingSize::parse(" 64 ").map(RingSize::kib), Ok(64));
        assert_eq!(RingSize::parse(""), Err(RingSizeError::NotNumber));
        assert_eq!(RingSize::parse("-4"), Err(RingSizeError::NotNumber));
        assert_eq!(
            RingSize::parse("2"),
            Err(RingSizeError::TooSmall { kib: 2 })
        );
        assert_eq!(
            RingSize::parse("2097152"),
            Err(RingSizeError::TooLarge { kib: 2_097_152 })
        );
        assert_eq!(
            RingSize::parse("96"),
            Err(RingSizeError::NotPowerOfTwo { kib: 96 })
        );
        let size = RingSize::from_kib(4096).expect("valid");
        assert_eq!(size.data_words(), 4096 * 128);
        assert_eq!(size.ring_bytes(), 4096 + 4096 * 1024);
        assert!(
            RingSizeError::TooSmall { kib: 2 }
                .to_string()
                .contains("4 KiB")
        );
    }

    #[test]
    fn layouts_are_checked() {
        let size = RingSize::from_kib(MAX_RING_KIB).expect("valid");
        assert_eq!(
            RegionLayout::new(0, size),
            Err(LayoutError::Cpus { cpus: 0 })
        );
        assert_eq!(
            RegionLayout::new(MAX_RINGS + 1, size),
            Err(LayoutError::Cpus { cpus: 253 })
        );
        let layout = RegionLayout::new(4, RingSize::from_kib(4096).expect("valid")).expect("fits");
        let offsets: Vec<u64> = layout.entries().map(|e| e.offset).collect();
        let ring = 4096 + 4096 * 1024;
        assert_eq!(
            offsets,
            [4096, 4096 + ring, 4096 + 2 * ring, 4096 + 3 * ring]
        );
        assert_eq!(layout.entry(4), None);
    }

    #[test]
    fn clock_sample_lines_round_trip() {
        let line = ClockSampleLine {
            host_ns: 123_456_789,
            cpu: 2,
            head: 4096,
            timestamp: 987_654,
        };
        let text = line.to_string();
        assert_eq!(text, "host_ns 123456789 cpu 2 head 4096 timestamp 987654\n");
        assert_eq!(ClockSampleLine::parse(&text), Ok(line));
        assert_eq!(
            ClockSampleLine::parse("host_ns 1 cpu 2 head 3"),
            Err(LineError::Fields)
        );
        assert_eq!(
            ClockSampleLine::parse("host_ns x cpu 2 head 3 timestamp 4"),
            Err(LineError::Number)
        );
    }
}
