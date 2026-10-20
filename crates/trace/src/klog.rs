//! The persistent kernel log: a region of reserved memory that keeps the newest kernel log
//! output across a warm reboot (`kernel.log_persist`).
//!
//! The kernel reserves [`REGION_BYTES`] of RAM at a fixed physical address, as it does for the
//! crash dump ([`crate::crash`], whose [`region_is_free`](crate::crash::region_is_free) check
//! it shares). At boot it reads what the previous boot left with [`Previous::find`], copies
//! it out, and [arms](Log::arm) the region for this boot. From then on every byte the kernel
//! logs is also [appended](Log::append) to the region, the oldest bytes giving way once it is
//! full, so after a panic, a hang the watchdog resets or a reboot, the next boot can serve
//! the end of the previous boot's log. Firmware and QEMU leave RAM alone across a warm reset;
//! a cold boot finds nothing.
//!
//! # Layout
//!
//! Little-endian 64-bit words. A header of [`HEADER_WORDS`]: [`MAGIC`], [`VERSION`], the
//! data length in bytes ([`DATA_BYTES`] for a region of [`REGION_BYTES`]), the number of bytes
//! ever written this boot, the boot's sequence number (`0` for the first boot after a
//! cold one, one more than the previous boot's otherwise), and the count the write in
//! progress will end at (no more than the written count when none is). The rest of the
//! header is zero.
//! The data follows, a byte ring: byte `n` of the log (counting from 0) is at data offset
//! `n % data length`, packed eight to a word, low byte first.
//!
//! A write publishes the count it will end at before it stores a byte, and the written count
//! after the bytes it counts. A reset in the middle of a write therefore loses that write,
//! and the reader also drops the oldest bytes the write may already have overwritten, so the
//! text it returns never mixes the two. The kept text can start in the middle of a line, or
//! of a UTF-8 sequence, once older bytes have been overwritten.
//!
//! # Examples
//!
//! ```
//! use core::sync::atomic::AtomicU64;
//! use ferric_trace::klog::{Log, Previous, REGION_WORDS};
//!
//! let words: Vec<AtomicU64> = (0..REGION_WORDS).map(|_| AtomicU64::new(0)).collect();
//! // A cold boot: nothing to read, so this is boot 0.
//! assert!(Previous::find(&words).is_err());
//! let log = Log::arm(&words, 0).expect("a whole region");
//! log.append(b"kernel: booting\n");
//!
//! // The next boot reads it back before arming the region again.
//! let previous = Previous::find(&words).expect("armed");
//! let mut text = vec![0; previous.kept()];
//! previous.copy(&words, &mut text);
//! assert_eq!(text, b"kernel: booting\n");
//! assert_eq!(previous.boot(), 0);
//! Log::arm(&words, previous.boot() + 1).expect("a whole region");
//! ```

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering, fence};

use crate::record::WORD_BYTES;

/// Where user space reads the previous boot's log: the `sys` scheme's `log_previous` file,
/// empty when there is none.
pub const PREVIOUS_PATH: &str = "/scheme/sys/log_previous";

/// The first word of a region: `FOKLOG01`.
pub const MAGIC: u64 = u64::from_le_bytes(*b"FOKLOG01");

/// The layout's version.
pub const VERSION: u64 = 1;

/// The size of the reserved region, a whole number of pages.
pub const REGION_BYTES: usize = 64 * 1024;

/// [`REGION_BYTES`] in words.
pub const REGION_WORDS: usize = REGION_BYTES / WORD_BYTES;

/// The header's length in words.
pub const HEADER_WORDS: usize = 8;

/// The bytes of log a region of [`REGION_BYTES`] keeps.
pub const DATA_BYTES: usize = REGION_BYTES - HEADER_WORDS * WORD_BYTES;

const MAGIC_WORD: usize = 0;
const VERSION_WORD: usize = 1;
const DATA_WORD: usize = 2;
const WRITTEN_WORD: usize = 3;
const BOOT_WORD: usize = 4;
const PENDING_WORD: usize = 5;

/// Why a region holds no log, or cannot be armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KlogError {
    /// The region has no room for the header and a word of data.
    RegionTooSmall,
    /// The first word is not [`MAGIC`]: the region was never armed, or not since a cold boot.
    NoMagic,
    /// A version this crate does not read.
    Version(u64),
    /// The header's data length does not fit the region.
    BadLength {
        /// The data length the header gives, in bytes.
        data_bytes: u64,
        /// The data bytes the region has room for.
        room: usize,
    },
}

impl fmt::Display for KlogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KlogError::RegionTooSmall => f.write_str("the persistent log region is too small"),
            KlogError::NoMagic => f.write_str("no persistent log magic"),
            KlogError::Version(v) => write!(f, "persistent log version {v} is not {VERSION}"),
            KlogError::BadLength { data_bytes, room } => write!(
                f,
                "a persistent log of {data_bytes} bytes in a region with room for {room}"
            ),
        }
    }
}

impl core::error::Error for KlogError {}

/// The data bytes `words` has room for after the header.
fn room(words: &[AtomicU64]) -> Result<usize, KlogError> {
    match words.len().checked_sub(HEADER_WORDS) {
        Some(data) if data > 0 => Ok(data * WORD_BYTES),
        _ => Err(KlogError::RegionTooSmall),
    }
}

/// Byte `pos` of the data ring in `words`.
fn byte(words: &[AtomicU64], pos: usize) -> u8 {
    let word = words[HEADER_WORDS + pos / WORD_BYTES].load(Ordering::Relaxed);
    word.to_le_bytes()[pos % WORD_BYTES]
}

/// The log a previous boot left in a region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Previous {
    boot: u64,
    written: u64,
    /// Where the write a reset interrupted would have ended, or `written` when none was.
    pending: u64,
    data_bytes: usize,
}

impl Previous {
    /// Reads the header of the region in `words`.
    ///
    /// # Errors
    ///
    /// [`KlogError`] when the region holds no log this crate reads: never armed (a cold
    /// boot's RAM), another version, or a header that does not fit the region.
    pub fn find(words: &[AtomicU64]) -> Result<Self, KlogError> {
        let room = room(words)?;
        if words[MAGIC_WORD].load(Ordering::Acquire) != MAGIC {
            return Err(KlogError::NoMagic);
        }
        let version = words[VERSION_WORD].load(Ordering::Relaxed);
        if version != VERSION {
            return Err(KlogError::Version(version));
        }
        let data_bytes = words[DATA_WORD].load(Ordering::Relaxed);
        let data_bytes = usize::try_from(data_bytes)
            .ok()
            .filter(|&bytes| bytes > 0 && bytes <= room)
            .ok_or(KlogError::BadLength { data_bytes, room })?;
        let written = words[WRITTEN_WORD].load(Ordering::Acquire);
        // Below the written count means no write was in progress (an armed region starts at 0).
        let pending = words[PENDING_WORD].load(Ordering::Relaxed).max(written);
        Ok(Self {
            boot: words[BOOT_WORD].load(Ordering::Relaxed),
            written,
            pending,
            data_bytes,
        })
    }

    /// The boot's sequence number: `0` for the first boot after a cold one.
    pub fn boot(&self) -> u64 {
        self.boot
    }

    /// The bytes that boot logged, including the ones overwritten since.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// The bytes the region still holds: the newest [`Previous::written`], up to its data
    /// length, less any that a write a reset interrupted may have overwritten.
    pub fn kept(&self) -> usize {
        let full =
            usize::try_from(self.written).map_or(self.data_bytes, |w| w.min(self.data_bytes));
        // The interrupted write stored at most `pending - written` bytes over the oldest ones.
        let torn = usize::try_from(self.pending - self.written).unwrap_or(usize::MAX);
        let lost = torn.saturating_sub(self.data_bytes - full);
        full.saturating_sub(lost)
    }

    /// The bytes logged before the kept ones, which the region no longer holds.
    pub fn overwritten(&self) -> u64 {
        self.written - self.kept() as u64
    }

    /// Copies the kept bytes, oldest first, to the start of `out`, as many as fit, and returns
    /// how many it copied. `words` is the region [`Previous::find`] read.
    pub fn copy(&self, words: &[AtomicU64], out: &mut [u8]) -> usize {
        let kept = self.kept();
        let len = kept.min(out.len());
        let start = self.overwritten();
        for (n, slot) in (start..).zip(&mut out[..len]) {
            // `n % data_bytes` is below `data_bytes`, which fits in a usize.
            *slot = byte(words, (n % self.data_bytes as u64) as usize);
        }
        len
    }
}

/// An armed region that this boot's log is appended to.
///
/// Appending is not synchronized: one writer at a time (the kernel appends under the lock
/// of its in-memory log). The region's words are atomics, so a reader on another CPU sees no
/// undefined behaviour, only a log that may be mid-write.
#[derive(Debug)]
pub struct Log<'a> {
    words: &'a [AtomicU64],
    data_bytes: usize,
}

impl<'a> Log<'a> {
    /// Arms the region in `words` for boot number `boot`, emptying it.
    ///
    /// # Errors
    ///
    /// [`KlogError::RegionTooSmall`] when `words` has no room for the header and a word of
    /// data. Nothing is written then.
    pub fn arm(words: &'a [AtomicU64], boot: u64) -> Result<Self, KlogError> {
        let data_bytes = room(words)?;
        // Invalidate first, so a reset while arming does not leave a header over stale counts.
        words[MAGIC_WORD].store(0, Ordering::Release);
        words[VERSION_WORD].store(VERSION, Ordering::Relaxed);
        words[DATA_WORD].store(data_bytes as u64, Ordering::Relaxed);
        words[WRITTEN_WORD].store(0, Ordering::Relaxed);
        words[BOOT_WORD].store(boot, Ordering::Relaxed);
        for word in &words[BOOT_WORD + 1..HEADER_WORDS] {
            word.store(0, Ordering::Relaxed);
        }
        words[MAGIC_WORD].store(MAGIC, Ordering::Release);
        Ok(Self { words, data_bytes })
    }

    /// Appends `bytes` to the log, overwriting the oldest bytes once the region is full.
    pub fn append(&self, bytes: &[u8]) {
        let written = self.store_bytes(bytes);
        // After the bytes it counts: a reset before this store loses only this write.
        self.words[WRITTEN_WORD].store(written, Ordering::Release);
    }

    /// The first half of [`Log::append`]: publishes where the write will end, then stores the
    /// bytes, and returns the new written count without publishing it.
    fn store_bytes(&self, bytes: &[u8]) -> u64 {
        let mut written = self.written();
        // Only the last `data_bytes` of a long write can be kept.
        let skip = bytes.len().saturating_sub(self.data_bytes);
        let end = written.wrapping_add(bytes.len() as u64);
        // Before any byte: a reader after a reset here knows which old bytes may be gone.
        self.words[PENDING_WORD].store(end, Ordering::Relaxed);
        // Orders the pending count before the byte stores below.
        fence(Ordering::Release);
        written = written.wrapping_add(skip as u64);
        for &b in &bytes[skip..] {
            // `written % data_bytes` is below `data_bytes`, which fits in a usize.
            let pos = (written % self.data_bytes as u64) as usize;
            let word = &self.words[HEADER_WORDS + pos / WORD_BYTES];
            let shift = (pos % WORD_BYTES) * 8;
            let old = word.load(Ordering::Relaxed);
            word.store(
                old & !(0xff << shift) | u64::from(b) << shift,
                Ordering::Relaxed,
            );
            written = written.wrapping_add(1);
        }
        written
    }

    /// The bytes appended since the region was armed.
    pub fn written(&self) -> u64 {
        self.words[WRITTEN_WORD].load(Ordering::Relaxed)
    }
}

/// A fixed-size byte ring for the log the kernel writes before it has a heap, and so before
/// its in-memory log exists. It needs no allocation, so it can be a `static`; once the
/// in-memory log exists the kernel moves the kept bytes into it, and from there into the
/// persistent log.
///
/// Like [`Log`], it keeps the newest `N` bytes, and it is not synchronized: one writer at a
/// time (the kernel uses it under the lock of its in-memory log).
///
/// # Examples
///
/// ```
/// use ferric_trace::klog::EarlyLog;
///
/// let mut early = EarlyLog::<8>::new();
/// early.push(b"0123456789");
/// let (older, newer) = early.slices();
/// assert_eq!([older, newer].concat(), b"23456789");
/// assert_eq!(early.dropped(), 2);
/// ```
#[derive(Debug)]
pub struct EarlyLog<const N: usize> {
    bytes: [u8; N],
    written: usize,
}

impl<const N: usize> EarlyLog<N> {
    /// An empty buffer.
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            written: 0,
        }
    }

    /// Appends `bytes`, overwriting the oldest ones once `N` are kept.
    pub fn push(&mut self, bytes: &[u8]) {
        if N == 0 {
            self.written = self.written.saturating_add(bytes.len());
            return;
        }
        let skip = bytes.len().saturating_sub(N);
        self.written = self.written.saturating_add(skip);
        for &b in &bytes[skip..] {
            self.bytes[self.written % N] = b;
            self.written = self.written.saturating_add(1);
        }
    }

    /// The kept bytes, oldest first, as two slices to be read one after the other.
    pub fn slices(&self) -> (&[u8], &[u8]) {
        if N == 0 || self.written <= N {
            return (&self.bytes[..self.written.min(N)], &[]);
        }
        let start = self.written % N;
        (&self.bytes[start..], &self.bytes[..start])
    }

    /// The bytes pushed before the kept ones, which the buffer no longer holds.
    pub fn dropped(&self) -> usize {
        self.written.saturating_sub(N)
    }

    /// Empties the buffer.
    pub fn clear(&mut self) {
        self.written = 0;
    }
}

impl<const N: usize> Default for EarlyLog<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn region(words: usize) -> Vec<AtomicU64> {
        (0..words).map(|_| AtomicU64::new(0)).collect()
    }

    fn read(words: &[AtomicU64]) -> (Previous, Vec<u8>) {
        let previous = Previous::find(words).expect("a log");
        let mut out = vec![0; previous.kept()];
        assert_eq!(previous.copy(words, &mut out), out.len());
        (previous, out)
    }

    #[test]
    fn a_cold_region_holds_no_log() {
        assert_eq!(
            Previous::find(&region(REGION_WORDS)),
            Err(KlogError::NoMagic)
        );
        let garbage = region(REGION_WORDS);
        garbage[0].store(0x1234_5678, Ordering::Relaxed);
        assert_eq!(Previous::find(&garbage), Err(KlogError::NoMagic));
    }

    #[test]
    fn keeps_what_was_appended_in_order() {
        let words = region(REGION_WORDS);
        let log = Log::arm(&words, 3).expect("armed");
        log.append(b"hello ");
        log.append(b"world\n");
        let (previous, text) = read(&words);
        assert_eq!(text, b"hello world\n");
        assert_eq!(
            (previous.boot(), previous.written(), previous.overwritten()),
            (3, 12, 0)
        );
        assert_eq!(previous.kept(), 12);
    }

    #[test]
    fn a_full_region_keeps_the_newest_bytes() {
        // Two data words: 16 bytes.
        let words = region(HEADER_WORDS + 2);
        let log = Log::arm(&words, 0).expect("armed");
        log.append(b"0123456789");
        log.append(b"abcdefghij");
        let (previous, text) = read(&words);
        assert_eq!(text, b"456789abcdefghij");
        assert_eq!((previous.written(), previous.overwritten()), (20, 4));
        // A write longer than the region keeps its end.
        log.append(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ");
        let (previous, text) = read(&words);
        assert_eq!(text, b"KLMNOPQRSTUVWXYZ");
        assert_eq!(previous.written(), 46);
    }

    #[test]
    fn arming_again_starts_an_empty_log() {
        let words = region(REGION_WORDS);
        Log::arm(&words, 0).expect("armed").append(b"first boot");
        let previous = Previous::find(&words).expect("a log");
        let log = Log::arm(&words, previous.boot() + 1).expect("armed");
        let (again, text) = read(&words);
        assert_eq!((again.boot(), text.len()), (1, 0));
        log.append(b"x");
        assert_eq!(read(&words).1, b"x");
    }

    #[test]
    fn copies_as_much_as_fits() {
        let words = region(REGION_WORDS);
        Log::arm(&words, 0).expect("armed").append(b"abcdef");
        let previous = Previous::find(&words).expect("a log");
        let mut out = [0u8; 4];
        assert_eq!(previous.copy(&words, &mut out), 4);
        assert_eq!(&out, b"abcd");
    }

    #[test]
    fn rejects_headers_that_do_not_fit() {
        assert_eq!(
            Log::arm(&region(HEADER_WORDS), 0).map(|_| ()),
            Err(KlogError::RegionTooSmall)
        );
        let words = region(REGION_WORDS);
        Log::arm(&words, 0).expect("armed");
        words[DATA_WORD].store(REGION_BYTES as u64, Ordering::Relaxed);
        assert_eq!(
            Previous::find(&words),
            Err(KlogError::BadLength {
                data_bytes: REGION_BYTES as u64,
                room: DATA_BYTES
            })
        );
        words[DATA_WORD].store(DATA_BYTES as u64, Ordering::Relaxed);
        words[VERSION_WORD].store(9, Ordering::Relaxed);
        assert_eq!(Previous::find(&words), Err(KlogError::Version(9)));
    }

    #[test]
    fn a_reset_mid_wrap_keeps_only_untouched_bytes() {
        // The example from review: a full 16-byte ring, then `WXYZ` lands on slots 0-3 and
        // the machine resets before the count is published.
        let words = region(HEADER_WORDS + 2);
        let log = Log::arm(&words, 0).expect("armed");
        log.append(b"0123456789abcdef");
        log.store_bytes(b"WXYZ");
        let (previous, text) = read(&words);
        assert_eq!(text, b"456789abcdef");
        assert_eq!((previous.written(), previous.overwritten()), (16, 4));
    }

    #[test]
    fn a_reset_before_the_ring_is_full_loses_nothing_old() {
        let words = region(HEADER_WORDS + 2);
        let log = Log::arm(&words, 0).expect("armed");
        log.append(b"0123456789");
        // Six free slots take the first six bytes; the last two overwrite `01`.
        log.store_bytes(b"ABCDEFGH");
        assert_eq!(read(&words).1, b"23456789");
    }

    #[test]
    fn a_reset_in_a_write_longer_than_the_ring_keeps_nothing() {
        let words = region(HEADER_WORDS + 1);
        let log = Log::arm(&words, 0).expect("armed");
        log.append(b"abcdefgh");
        log.store_bytes(b"0123456789");
        let (previous, text) = read(&words);
        assert_eq!((previous.written(), text.len()), (8, 0));
    }

    #[test]
    fn a_published_write_clears_the_pending_count() {
        let words = region(HEADER_WORDS + 2);
        let log = Log::arm(&words, 0).expect("armed");
        log.append(b"0123456789abcdef");
        log.append(b"WXYZ");
        assert_eq!(read(&words).1, b"456789abcdefWXYZ");
    }

    fn early_text<const N: usize>(early: &EarlyLog<N>) -> Vec<u8> {
        let (older, newer) = early.slices();
        [older, newer].concat()
    }

    #[test]
    fn the_early_log_keeps_the_newest_bytes_in_order() {
        let mut early = EarlyLog::<8>::new();
        assert_eq!(early_text(&early), b"");
        early.push(b"abc");
        early.push(b"def");
        assert_eq!(
            (early_text(&early), early.dropped()),
            (b"abcdef".to_vec(), 0)
        );
        early.push(b"ghij");
        assert_eq!(
            (early_text(&early), early.dropped()),
            (b"cdefghij".to_vec(), 2)
        );
        early.push(b"0123456789XY");
        assert_eq!(
            (early_text(&early), early.dropped()),
            (b"456789XY".to_vec(), 14)
        );
        early.clear();
        assert_eq!(early_text(&early), b"");
    }

    #[test]
    fn an_empty_early_log_keeps_nothing() {
        let mut early = EarlyLog::<0>::new();
        early.push(b"abc");
        assert_eq!((early_text(&early), early.dropped()), (Vec::new(), 3));
    }

    #[test]
    fn a_written_count_past_usize_keeps_a_full_region() {
        let words = region(HEADER_WORDS + 1);
        Log::arm(&words, 0).expect("armed").append(b"abcdefgh");
        words[WRITTEN_WORD].store(u64::MAX, Ordering::Relaxed);
        let previous = Previous::find(&words).expect("a log");
        assert_eq!(previous.kept(), 8);
    }
}
