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
//! ever written this boot, and the boot's sequence number (`0` for the first boot after a
//! cold one, one more than the previous boot's otherwise). The rest of the header is zero.
//! The data follows, a byte ring: byte `n` of the log (counting from 0) is at data offset
//! `n % data length`, packed eight to a word, low byte first.
//!
//! The written count is stored after the bytes it counts, so a reset in the middle of a
//! write loses at most that write. The kept text can start in the middle of a line, or of a
//! UTF-8 sequence, once older bytes have been overwritten.
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
use core::sync::atomic::{AtomicU64, Ordering};

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
        Ok(Self {
            boot: words[BOOT_WORD].load(Ordering::Relaxed),
            written: words[WRITTEN_WORD].load(Ordering::Acquire),
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
    /// length.
    pub fn kept(&self) -> usize {
        usize::try_from(self.written).map_or(self.data_bytes, |w| w.min(self.data_bytes))
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
        let mut written = self.written();
        // Only the last `data_bytes` of a long write can be kept.
        let skip = bytes.len().saturating_sub(self.data_bytes);
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
        // After the bytes it counts: a reset before this store loses only this write.
        self.words[WRITTEN_WORD].store(written, Ordering::Release);
    }

    /// The bytes appended since the region was armed.
    pub fn written(&self) -> u64 {
        self.words[WRITTEN_WORD].load(Ordering::Relaxed)
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
    fn a_written_count_past_usize_keeps_a_full_region() {
        let words = region(HEADER_WORDS + 1);
        Log::arm(&words, 0).expect("armed").append(b"abcdefgh");
        words[WRITTEN_WORD].store(u64::MAX, Ordering::Relaxed);
        let previous = Previous::find(&words).expect("a log");
        assert_eq!(previous.kept(), 8);
    }
}
