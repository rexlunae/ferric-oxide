//! The crash dump: a region of reserved memory that survives a warm reboot, where a kernel
//! panic leaves its message and the newest records of every CPU's ring (`kernel.trace_crash`).
//!
//! The kernel reserves [`REGION_BYTES`] of RAM at a fixed physical address, [arms](Writer::arm)
//! it at boot, and on a panic [writes](Writer) a dump into it. Firmware and QEMU leave RAM
//! alone across a warm reset, so the next boot finds the dump, checks it with [`Dump::len_in`],
//! copies it out and arms the region again. User space reads the copy from the trace scheme's
//! `crash` file ([`abi::CRASH_PATH`](crate::abi::CRASH_PATH)) and [`Dump::parse`]s it; the
//! `ferric-trace crash` tool turns it into a capture of `cpu<n>` files.
//!
//! # Layout
//!
//! Little-endian 64-bit words. A header of [`HEADER_WORDS`]: [`MAGIC`], [`VERSION`], the
//! state ([`STATE_EMPTY`], [`STATE_WRITING`] or [`STATE_COMPLETE`]), the payload length in
//! words, a checksum (64-bit FNV-1a, one word at a time, over every other header word and
//! the payload), and the clock the records' timestamps come from ([`crate::clock`]): one word
//! holding `1 + `[`Clock::raw`] and one holding the tick length in femtoseconds, `0` when it is
//! unknown. A clock word of `0` means the dump does not say, as in dumps from kernels before
//! the clock was recorded, whose timestamps are TSC cycles. The rest of the header is zero.
//!
//! The payload is a run of sections, each a word `kind | cpu << 8 | bytes << 32` and then
//! `bytes` of data padded to whole words:
//!
//! - [`SECTION_MESSAGE`]: the panic message, UTF-8, at most [`MESSAGE_MAX`] bytes (`cpu` is the
//!   CPU that panicked).
//! - [`SECTION_CPU`]: a CPU's ring: one word with its `lost` count, then whole records
//!   ([`crate::record`]), oldest first, the newest that fitted.
//!
//! A region is complete only when its state says so and the checksum matches, so a cold
//! boot's random RAM, or a panic cut short by a second one, never reads as a dump.
//!
//! # Placement
//!
//! The kernel writes the region in its panic handler, so it must be plain RAM that nothing
//! else owns: [`region_is_free`] checks the region against the boot memory map before the
//! kernel reserves it.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::clock::{Clock, ClockLine};
use crate::record::WORD_BYTES;
use crate::ring::{Ring, Storage};

/// The first word of a region: `FOCRASH1`.
pub const MAGIC: u64 = u64::from_le_bytes(*b"FOCRASH1");

/// The layout's version.
pub const VERSION: u64 = 1;

/// The size of the reserved region, a whole number of pages.
pub const REGION_BYTES: usize = 64 * 1024;

/// [`REGION_BYTES`] in words.
pub const REGION_WORDS: usize = REGION_BYTES / WORD_BYTES;

/// The header's length in words.
pub const HEADER_WORDS: usize = 8;

/// The longest panic message kept, in bytes.
pub const MESSAGE_MAX: usize = 512;

/// The state of an armed region with no dump.
pub const STATE_EMPTY: u64 = 0;
/// The state while a dump is written.
pub const STATE_WRITING: u64 = 1;
/// The state of a complete dump.
pub const STATE_COMPLETE: u64 = 2;

/// The section kind of the panic message.
pub const SECTION_MESSAGE: u8 = 1;
/// The section kind of a CPU's records.
pub const SECTION_CPU: u8 = 2;

const MAGIC_WORD: usize = 0;
const VERSION_WORD: usize = 1;
const STATE_WORD: usize = 2;
const USED_WORD: usize = 3;
const CHECKSUM_WORD: usize = 4;
const CLOCK_WORD: usize = 5;
const PERIOD_WORD: usize = 6;

/// The largest CPU number a section holds.
const MAX_CPU: u32 = (1 << 24) - 1;

/// Why a region or a copy of one is not a dump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CrashError {
    /// Fewer words than the header, or than the payload length says.
    Truncated,
    /// The first word is not [`MAGIC`]: the region was never armed, or not since a cold boot.
    NoMagic,
    /// A version this crate does not read.
    Version(u64),
    /// The region holds no complete dump (it is armed, or a write was cut short).
    NotComplete,
    /// The checksum does not match.
    Checksum,
    /// A section runs past the payload or is of an unknown kind.
    Section {
        /// The section's offset in the payload, in words.
        at: usize,
    },
    /// A region smaller than the header and one section.
    RegionTooSmall,
}

impl fmt::Display for CrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrashError::Truncated => f.write_str("the crash dump is cut short"),
            CrashError::NoMagic => f.write_str("no crash dump magic"),
            CrashError::Version(v) => write!(f, "crash dump version {v} is not {VERSION}"),
            CrashError::NotComplete => f.write_str("the crash dump is not complete"),
            CrashError::Checksum => f.write_str("the crash dump checksum does not match"),
            CrashError::Section { at } => write!(f, "bad crash dump section at word {at}"),
            CrashError::RegionTooSmall => f.write_str("the crash region is too small"),
        }
    }
}

impl core::error::Error for CrashError {}

/// 64-bit FNV-1a, a word at a time.
struct Checksum(u64);

impl Checksum {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn add(&mut self, word: u64) {
        self.0 = (self.0 ^ word).wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// A section's header word.
const fn section_word(kind: u8, cpu: u32, bytes: usize) -> u64 {
    kind as u64 | ((cpu & MAX_CPU) as u64) << 8 | (bytes as u64) << 32
}

/// The words `bytes` of data take.
const fn data_words(bytes: usize) -> usize {
    bytes.div_ceil(WORD_BYTES)
}

/// Checks the dump in the `len` words that `word` reads and returns its length in words,
/// header included.
fn check(len: usize, word: impl Fn(usize) -> u64) -> Result<usize, CrashError> {
    if len < HEADER_WORDS {
        return Err(CrashError::Truncated);
    }
    if word(MAGIC_WORD) != MAGIC {
        return Err(CrashError::NoMagic);
    }
    match word(VERSION_WORD) {
        VERSION => {}
        other => return Err(CrashError::Version(other)),
    }
    if word(STATE_WORD) != STATE_COMPLETE {
        return Err(CrashError::NotComplete);
    }
    let used = usize::try_from(word(USED_WORD)).map_err(|_| CrashError::Truncated)?;
    let total = HEADER_WORDS
        .checked_add(used)
        .filter(|&total| total <= len)
        .ok_or(CrashError::Truncated)?;
    let mut sum = Checksum::new();
    for i in (0..total).filter(|&i| i != CHECKSUM_WORD) {
        sum.add(word(i));
    }
    if sum.0 != word(CHECKSUM_WORD) {
        return Err(CrashError::Checksum);
    }
    // Every section must fit and be of a known kind.
    let mut at = 0;
    while at < used {
        let header = word(HEADER_WORDS + at);
        let words = 1 + data_words((header >> 32) as usize);
        let known = matches!(header as u8, SECTION_MESSAGE | SECTION_CPU);
        if !known || words > used - at {
            return Err(CrashError::Section { at });
        }
        at += words;
    }
    Ok(total)
}

/// The clock a header's clock and period words describe; `None` for a clock word of `0` (not
/// recorded) or of a clock this crate does not know.
fn clock_line(clock: u64, period_fs: u64) -> Option<ClockLine> {
    let raw = u8::try_from(clock.checked_sub(1)?).ok()?;
    Some(ClockLine {
        clock: Clock::from_raw(raw)?,
        period_fs: (period_fs != 0).then_some(period_fs),
    })
}

/// An area of the boot memory map, as [`region_is_free`] needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapArea {
    /// The first byte's physical address.
    pub start: usize,
    /// The physical address after the last byte.
    pub end: usize,
    /// Whether the crash region may use the area.
    pub availability: Availability,
}

/// Whether a [`MapArea`] is RAM the crash region may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Availability {
    /// Free RAM.
    Free,
    /// Anything else: reserved memory, ACPI tables, device memory, the kernel, the boot
    /// loader's data.
    Unavailable,
}

/// Whether the region `start..end` can hold a crash dump: it is not empty, lies wholly inside
/// one free area of `map`, and overlaps no area that is not free. A memory map can list a
/// reserved or device area inside a free one, and the dump must not write over it.
///
/// # Examples
///
/// ```
/// use ferric_trace::crash::{Availability, MapArea, region_is_free};
///
/// let ram = MapArea { start: 0, end: 0x1000_0000, availability: Availability::Free };
/// let acpi = MapArea {
///     start: 0x400_8000,
///     end: 0x400_9000,
///     availability: Availability::Unavailable,
/// };
/// assert!(region_is_free(0x400_0000, 0x401_0000, [ram]));
/// assert!(!region_is_free(0x400_0000, 0x401_0000, [ram, acpi]));
/// ```
pub fn region_is_free(start: usize, end: usize, map: impl IntoIterator<Item = MapArea>) -> bool {
    let mut covered = false;
    for area in map {
        match area.availability {
            Availability::Free => covered |= area.start <= start && end <= area.end,
            Availability::Unavailable if area.start < end && start < area.end => return false,
            Availability::Unavailable => {}
        }
    }
    start < end && covered
}

/// Writes a dump into a region. The kernel makes one in its panic handler.
#[derive(Debug)]
pub struct Writer<'a> {
    words: &'a [AtomicU64],
    pos: usize,
}

impl<'a> Writer<'a> {
    /// Arms `words` with no dump: the header of an empty region.
    ///
    /// # Errors
    ///
    /// [`CrashError::RegionTooSmall`] when `words` cannot hold the header and a section.
    pub fn arm(words: &[AtomicU64]) -> Result<(), CrashError> {
        let header = words
            .get(..HEADER_WORDS + 2)
            .ok_or(CrashError::RegionTooSmall)?;
        for word in header.iter().skip(1) {
            word.store(0, Ordering::Relaxed);
        }
        if let [magic, version, ..] = header {
            magic.store(MAGIC, Ordering::Relaxed);
            version.store(VERSION, Ordering::Release);
        }
        Ok(())
    }

    /// Starts a dump in `words`, discarding any dump there.
    ///
    /// # Errors
    ///
    /// [`CrashError::RegionTooSmall`] as for [`Writer::arm`].
    pub fn begin(words: &'a [AtomicU64]) -> Result<Self, CrashError> {
        Self::arm(words)?;
        words[STATE_WORD].store(STATE_WRITING, Ordering::Release);
        Ok(Self {
            words,
            pos: HEADER_WORDS,
        })
    }

    /// The words left for sections, their headers included.
    pub fn remaining(&self) -> usize {
        self.words.len() - self.pos
    }

    /// Appends a section of `kind` for `cpu` whose data `fill` writes, given at most `max`
    /// data words and a store for each word, and which returns its length in bytes.
    fn section(
        &mut self,
        kind: u8,
        cpu: u32,
        max: usize,
        fill: impl FnOnce(usize, &mut dyn FnMut(u64)) -> usize,
    ) -> usize {
        let Some(max) = self.remaining().checked_sub(1).map(|room| room.min(max)) else {
            return 0;
        };
        let header = self.pos;
        let mut next = header + 1;
        let end = next + max;
        let words = self.words;
        let bytes = fill(max, &mut |word| {
            if let Some(slot) = words.get(next).filter(|_| next < end) {
                slot.store(word, Ordering::Relaxed);
                next += 1;
            }
        });
        let bytes = bytes.min((next - header - 1) * WORD_BYTES);
        self.words[header].store(section_word(kind, cpu, bytes), Ordering::Relaxed);
        self.pos = header + 1 + data_words(bytes);
        bytes
    }

    /// Appends the panic message of `cpu`, cut to [`MESSAGE_MAX`] bytes and to the room left,
    /// and returns the bytes kept.
    pub fn message(&mut self, cpu: u32, message: &[u8]) -> usize {
        let message = message.get(..MESSAGE_MAX).unwrap_or(message);
        self.section(
            SECTION_MESSAGE,
            cpu,
            data_words(message.len()),
            |max, store| {
                let kept = message.get(..max * WORD_BYTES).unwrap_or(message);
                for chunk in kept.chunks(WORD_BYTES) {
                    let mut word = [0u8; WORD_BYTES];
                    word[..chunk.len()].copy_from_slice(chunk);
                    store(u64::from_le_bytes(word));
                }
                kept.len()
            },
        )
    }

    /// Appends the newest records of `cpu`'s `ring` that fit in `max` words (the section's two
    /// words of header and `lost` included), and returns the record words copied.
    pub fn cpu<S: Storage>(&mut self, cpu: u32, ring: &Ring<S>, max: usize) -> usize {
        let Some(max) = max.checked_sub(1) else {
            return 0;
        };
        let bytes = self.section(SECTION_CPU, cpu, max, |max, store| {
            let Some(records) = max.checked_sub(1) else {
                return 0;
            };
            store(ring.lost());
            (1 + ring.copy_latest(records, &mut *store)) * WORD_BYTES
        });
        data_words(bytes).saturating_sub(1)
    }

    /// Records the clock the dump's timestamps come from, in the header.
    pub fn clock(&mut self, line: ClockLine) {
        self.words[CLOCK_WORD].store(1 + u64::from(line.clock.raw()), Ordering::Relaxed);
        self.words[PERIOD_WORD].store(line.period_fs.unwrap_or(0), Ordering::Relaxed);
    }

    /// Completes the dump: stores its length and checksum, then marks it complete.
    pub fn finish(self) {
        let used = (self.pos - HEADER_WORDS) as u64;
        let mut sum = Checksum::new();
        for (i, word) in self.words[..self.pos].iter().enumerate() {
            sum.add(match i {
                STATE_WORD => STATE_COMPLETE,
                USED_WORD => used,
                CHECKSUM_WORD => continue,
                _ => word.load(Ordering::Relaxed),
            });
        }
        self.words[USED_WORD].store(used, Ordering::Relaxed);
        self.words[CHECKSUM_WORD].store(sum.0, Ordering::Relaxed);
        self.words[STATE_WORD].store(STATE_COMPLETE, Ordering::Release);
    }
}

/// A panic message being formatted into a fixed buffer, cut at [`MESSAGE_MAX`] bytes on a
/// character boundary. The kernel formats its panic message into one on the stack.
#[derive(Debug)]
pub struct MessageBuf {
    buf: [u8; MESSAGE_MAX],
    len: usize,
}

impl MessageBuf {
    /// An empty message.
    pub const fn new() -> Self {
        Self {
            buf: [0; MESSAGE_MAX],
            len: 0,
        }
    }

    /// The bytes written.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl Default for MessageBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Write for MessageBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = MESSAGE_MAX - self.len;
        let mut take = s.len().min(room);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// A CPU's section of a dump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuDump<'a> {
    /// The CPU.
    pub cpu: u32,
    /// Its ring's `lost` count when the dump was taken.
    pub lost: u64,
    /// Its newest records, whole records oldest first.
    pub records: &'a [u8],
}

/// A complete dump, read from its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dump<'a> {
    payload: &'a [u8],
    clock: Option<ClockLine>,
}

impl<'a> Dump<'a> {
    /// The length in bytes of the complete dump in `words` (header included), for copying it
    /// out of the region.
    ///
    /// # Errors
    ///
    /// As [`Dump::parse`].
    pub fn len_in(words: &[AtomicU64]) -> Result<usize, CrashError> {
        check(words.len(), |i| words[i].load(Ordering::Acquire)).map(|words| words * WORD_BYTES)
    }

    /// Reads the dump at the start of `bytes`.
    ///
    /// # Errors
    ///
    /// [`CrashError::Truncated`], [`CrashError::NoMagic`], [`CrashError::Version`],
    /// [`CrashError::NotComplete`], [`CrashError::Checksum`] or [`CrashError::Section`] when
    /// `bytes` does not start with a complete, intact dump.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, CrashError> {
        let word = |i: usize| {
            let at = i * WORD_BYTES;
            let mut w = [0u8; WORD_BYTES];
            w.copy_from_slice(&bytes[at..at + WORD_BYTES]);
            u64::from_le_bytes(w)
        };
        let total = check(bytes.len() / WORD_BYTES, word)?;
        Ok(Self {
            payload: &bytes[HEADER_WORDS * WORD_BYTES..total * WORD_BYTES],
            clock: clock_line(word(CLOCK_WORD), word(PERIOD_WORD)),
        })
    }

    /// The clock the dump's timestamps come from, when the dump records it. A dump without
    /// one comes from a kernel whose timestamps are TSC cycles.
    pub fn clock(&self) -> Option<ClockLine> {
        self.clock
    }

    /// The sections, as (kind, cpu, data).
    fn sections(&self) -> impl Iterator<Item = (u8, u32, &'a [u8])> + 'a {
        let mut rest = self.payload;
        core::iter::from_fn(move || {
            let (header, after) = rest.split_first_chunk::<WORD_BYTES>()?;
            let header = u64::from_le_bytes(*header);
            let bytes = (header >> 32) as usize;
            // `parse` checked that every section fits.
            let data = after.get(..bytes)?;
            rest = after.get(data_words(bytes) * WORD_BYTES..)?;
            Some((header as u8, (header >> 8) as u32 & MAX_CPU, data))
        })
    }

    /// The panic message and the CPU that panicked, if the dump has one.
    pub fn message(&self) -> Option<(u32, &'a [u8])> {
        self.sections()
            .find(|&(kind, ..)| kind == SECTION_MESSAGE)
            .map(|(_, cpu, data)| (cpu, data))
    }

    /// Each CPU's section, in the order they were written.
    pub fn cpus(&self) -> impl Iterator<Item = CpuDump<'a>> + 'a {
        self.sections()
            .filter(|&(kind, ..)| kind == SECTION_CPU)
            .filter_map(|(_, cpu, data)| {
                let (lost, records) = data.split_first_chunk::<WORD_BYTES>()?;
                Some(CpuDump {
                    cpu,
                    lost: u64::from_le_bytes(*lost),
                    records,
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventId;
    use crate::record::Records;
    use core::fmt::Write as _;
    use std::vec::Vec;

    #[test]
    fn a_region_must_be_free_ram_that_nothing_else_overlaps() {
        let free = |start, end| MapArea {
            start,
            end,
            availability: Availability::Free,
        };
        let taken = |start, end| MapArea {
            start,
            end,
            availability: Availability::Unavailable,
        };
        let (start, end) = (0x4000, 0x8000);
        assert!(region_is_free(start, end, [free(0, 0x10000)]));
        assert!(region_is_free(start, end, [free(start, end)]));
        // Not covered: no map, outside, partly outside, or split over two free areas.
        assert!(!region_is_free(start, end, []));
        assert!(!region_is_free(start, end, [free(0x10000, 0x20000)]));
        assert!(!region_is_free(start, end, [free(0, 0x6000)]));
        assert!(!region_is_free(
            start,
            end,
            [free(0, 0x6000), free(0x6000, 0x10000)]
        ));
        // A non-free area inside, across either end, or around it, before or after the free
        // one in the map.
        for area in [
            taken(0x5000, 0x6000),
            taken(0x3000, 0x4001),
            taken(0x7fff, 0x9000),
            taken(0, 0x10000),
        ] {
            assert!(
                !region_is_free(start, end, [free(0, 0x10000), area]),
                "{area:?}"
            );
            assert!(
                !region_is_free(start, end, [area, free(0, 0x10000)]),
                "{area:?}"
            );
        }
        // Touching is not overlapping.
        assert!(region_is_free(
            start,
            end,
            [taken(0, start), free(0, 0x10000), taken(end, 0x9000)]
        ));
        // An empty region is never free.
        assert!(!region_is_free(start, start, [free(0, 0x10000)]));
    }

    fn region(words: usize) -> Vec<AtomicU64> {
        (0..words).map(|_| AtomicU64::new(0)).collect()
    }

    fn bytes(words: &[AtomicU64]) -> Vec<u8> {
        words
            .iter()
            .flat_map(|w| w.load(Ordering::Relaxed).to_le_bytes())
            .collect()
    }

    fn ring_with(timestamps: core::ops::Range<u64>) -> Ring<crate::ring::Inline<Vec<AtomicU64>>> {
        let ring = Ring::new(region(64)).expect("valid");
        for t in timestamps {
            ring.push(EventId::new(2), t, &[t, t]).expect("room");
        }
        ring
    }

    #[test]
    fn writes_and_reads_a_dump() {
        let words = region(REGION_WORDS);
        Writer::arm(&words).expect("big enough");
        assert_eq!(Dump::len_in(&words), Err(CrashError::NotComplete));

        let ring0 = ring_with(0..5);
        ring0.count_lost();
        let ring1 = ring_with(10..12);
        let mut writer = Writer::begin(&words).expect("big enough");
        assert_eq!(writer.message(1, b"panicked at src/x.rs: oops"), 26);
        assert_eq!(writer.cpu(0, &ring0, 100), 20);
        assert_eq!(writer.cpu(1, &ring1, 100), 8);
        let kvm = ClockLine {
            clock: Clock::KvmClock,
            period_fs: Some(1_000_000),
        };
        writer.clock(kvm);
        writer.finish();

        let len = Dump::len_in(&words).expect("complete");
        let all = bytes(&words);
        let dump = Dump::parse(&all[..len]).expect("complete");
        assert_eq!(dump.clock(), Some(kvm));
        assert_eq!(
            dump.message(),
            Some((1, &b"panicked at src/x.rs: oops"[..]))
        );
        let cpus: Vec<_> = dump.cpus().collect();
        assert_eq!(cpus.len(), 2);
        assert_eq!((cpus[0].cpu, cpus[0].lost), (0, 1));
        let stamps: Vec<_> = Records::new(cpus[0].records)
            .map(|r| r.expect("whole").timestamp())
            .collect();
        assert_eq!(stamps, [0, 1, 2, 3, 4]);
        assert_eq!(Records::new(cpus[1].records).count(), 2);
        // The records stay in the rings.
        assert_eq!(ring0.pending(), 20);
    }

    #[test]
    fn keeps_the_newest_records_that_fit() {
        let words = region(REGION_WORDS);
        let ring = ring_with(0..10);
        let mut writer = Writer::begin(&words).expect("big enough");
        // Two words of section header and `lost`, then two four-word records.
        assert_eq!(writer.cpu(3, &ring, 2 + 9), 8);
        writer.finish();
        let all = bytes(&words);
        let dump = Dump::parse(&all).expect("complete");
        let cpu = dump.cpus().next().expect("one CPU");
        let stamps: Vec<_> = Records::new(cpu.records)
            .map(|r| r.expect("whole").timestamp())
            .collect();
        assert_eq!(stamps, [8, 9]);
        assert_eq!(dump.message(), None);
        // A dump that does not record its clock, like one from an older kernel.
        assert_eq!(dump.clock(), None);
    }

    #[test]
    fn reads_the_clock_words() {
        assert_eq!(clock_line(0, 0), None);
        assert_eq!(clock_line(0, 1_000_000), None);
        assert_eq!(clock_line(1 + 200, 0), None);
        assert_eq!(clock_line(1 + (1 << 8), 0), None);
        for (clock, period_fs) in [
            (Clock::Tsc, None),
            (Clock::KvmClock, Some(1_000_000)),
            (Clock::Hpet, Some(10_000_000)),
            (Clock::Hpet, None),
        ] {
            let words = region(REGION_WORDS);
            let mut writer = Writer::begin(&words).expect("big enough");
            writer.clock(ClockLine { clock, period_fs });
            writer.finish();
            let all = bytes(&words);
            let dump = Dump::parse(&all).expect("complete");
            assert_eq!(dump.clock(), Some(ClockLine { clock, period_fs }));
        }
    }

    #[test]
    fn a_small_region_cuts_the_dump() {
        let words = region(HEADER_WORDS + 4);
        let ring = ring_with(0..10);
        let mut writer = Writer::begin(&words).expect("big enough");
        // Three data words of room: 24 bytes of message.
        assert_eq!(writer.message(0, &[b'x'; 100]), 24);
        assert_eq!(writer.remaining(), 0);
        assert_eq!(writer.cpu(0, &ring, 100), 0);
        writer.finish();
        let all = bytes(&words);
        let dump = Dump::parse(&all).expect("complete");
        assert_eq!(dump.message().map(|(_, m)| m.len()), Some(24));
        assert_eq!(dump.cpus().count(), 0);
        assert_eq!(
            Writer::arm(&region(HEADER_WORDS + 1)),
            Err(CrashError::RegionTooSmall)
        );
    }

    #[test]
    fn refuses_what_is_not_a_complete_dump() {
        let words = region(REGION_WORDS);
        assert_eq!(Dump::len_in(&words), Err(CrashError::NoMagic));
        assert_eq!(Dump::parse(&[0; 16]), Err(CrashError::Truncated));

        // A dump cut short by a second panic is still being written.
        let mut writer = Writer::begin(&words).expect("big enough");
        writer.message(0, b"first");
        assert_eq!(Dump::len_in(&words), Err(CrashError::NotComplete));
        writer.finish();
        let good = bytes(&words);
        let len = Dump::len_in(&words).expect("complete");

        // Every flipped bit of the dump is caught.
        for bit in 0..len * 8 {
            let mut bad = good.clone();
            bad[bit / 8] ^= 1 << (bit % 8);
            assert!(Dump::parse(&bad[..len]).is_err(), "bit {bit}");
        }
        assert_eq!(Dump::parse(&good[..len - 8]), Err(CrashError::Truncated));
        let mut other = good.clone();
        other[8] = 2;
        assert_eq!(Dump::parse(&other), Err(CrashError::Version(2)));
    }

    #[test]
    fn rejects_bad_sections_even_with_a_good_checksum() {
        // A section longer than the payload, with the checksum fixed up to match.
        let words = region(64);
        let writer = Writer::begin(&words).expect("big enough");
        words[HEADER_WORDS].store(section_word(SECTION_CPU, 0, 800), Ordering::Relaxed);
        let mut writer = writer;
        writer.pos += 2;
        writer.finish();
        assert_eq!(Dump::len_in(&words), Err(CrashError::Section { at: 0 }));
    }

    #[test]
    fn message_buf_cuts_on_a_char_boundary() {
        let mut buf = MessageBuf::new();
        let long = "é".repeat(MESSAGE_MAX);
        write!(buf, "{long}").expect("never fails");
        assert_eq!(buf.as_bytes().len(), MESSAGE_MAX);
        assert!(core::str::from_utf8(buf.as_bytes()).is_ok());
        let mut buf = MessageBuf::new();
        write!(buf, "a{}", "é".repeat(MESSAGE_MAX)).expect("never fails");
        assert_eq!(buf.as_bytes().len(), MESSAGE_MAX - 1);
    }
}
