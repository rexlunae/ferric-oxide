//! How the kernel copies cached pages to a reader's buffer without 4K aliasing.
//!
//! Cached pages start on page boundaries. A reader's buffer usually does not: memory from
//! `malloc` typically starts 16 bytes into a page, so each page is copied to a destination a
//! few bytes past the source modulo 4096. A store followed shortly by a load whose address
//! matches it in the low 12 bits ("4K aliasing") then looks like a dependency to the core,
//! and the copy stalls on it. CI's AMD EPYC 7763 (Zen 3) runners showed this. Warm 1 MiB
//! reads from the cache, one `rep movsb` per page, ran at about 2 GB/s. The same reads ran
//! at 12–16 GB/s on Intel runners, and 4 KiB reads from one hot page ran fast on both.
//! Aligning only the destination to a cache line did not help, because the distance
//! between source and destination stays the same.
//!
//! The cure is to copy an aliased page in [`BOUNCE_CHUNK`]-byte pieces, each through a small
//! bounce buffer placed so that neither copy is aliased: the bounce is more than
//! [`ALIAS_WINDOW`] bytes past the source, and the destination is more than that past the
//! bounce, both modulo [`ALIAS_PERIOD`]. The bounce stays in L1. [`Bounce`] is the buffer:
//! a fixed-size array the caller keeps on its stack, so the copy never allocates and cannot
//! fail for want of memory. [`stage`] does the copy, through a caller-supplied writer, so
//! the kernel's user-copy routine (which may fault) stays the only code that touches the
//! destination.

/// The page size the aliasing is modulo: the low 12 bits of an address.
pub const ALIAS_PERIOD: usize = 4096;
/// How far past the source, modulo [`ALIAS_PERIOD`], a destination counts as aliased.
/// Loads run at most this far ahead of the stores they could be confused with. 1 KiB
/// covers a `rep movsb` in flight with room to spare.
pub const ALIAS_WINDOW: usize = 1024;
/// The most bytes staged through the bounce buffer in one piece.
pub const BOUNCE_CHUNK: usize = 1024;
/// The size of a bounce buffer. Wherever the buffer sits, a piece of [`BOUNCE_CHUNK`] bytes
/// fits at a start in `0..=BOUNCE_LEN - BOUNCE_CHUNK` that is out of the aliasing window of
/// both the source and the destination (see [`bounce_start`]).
pub const BOUNCE_LEN: usize = BOUNCE_CHUNK + ALIAS_PERIOD / 2;

/// The first distance (bounce minus source, modulo [`ALIAS_PERIOD`]) that is safe for both
/// copies. Every distance from here to `ALIAS_PERIOD - ALIAS_WINDOW` is: it is past the
/// source's window, and the destination (1 to [`ALIAS_WINDOW`] bytes past the source) is
/// then more than [`ALIAS_WINDOW`] bytes past the bounce, modulo the period.
const SAFE_FIRST: usize = ALIAS_WINDOW + 1;

/// A bounce buffer for [`stage`], kept by the caller (on the kernel stack, for example).
pub type Bounce = [u8; BOUNCE_LEN];

/// The distance from `src` to `dst` modulo [`ALIAS_PERIOD`].
const fn distance(src: usize, dst: usize) -> usize {
    dst.wrapping_sub(src) % ALIAS_PERIOD
}

/// Whether a copy from `src` to `dst` is 4K-aliased: the destination is a little past the
/// source (1 to [`ALIAS_WINDOW`] bytes), modulo [`ALIAS_PERIOD`]. A distance of 0 is not:
/// each load then comes before the store it matches.
///
/// # Examples
///
/// ```
/// use ferric_pcache::copy::aliased;
///
/// // A page frame copied to a malloc buffer 16 bytes into a page.
/// assert!(aliased(0x1000, 0x7000_0010));
/// // Page-aligned on both sides, or half a page apart.
/// assert!(!aliased(0x1000, 0x7000_0000));
/// assert!(!aliased(0x1000, 0x7000_0800));
/// ```
pub const fn aliased(src: usize, dst: usize) -> bool {
    let d = distance(src, dst);
    d != 0 && d <= ALIAS_WINDOW
}

/// Where, in a [`Bounce`] at `buffer`, a piece copied from `src` goes: a start of at most
/// `BOUNCE_LEN - BOUNCE_CHUNK` at which neither the copy from `src` nor a copy on to a
/// destination aliased with `src` is aliased.
///
/// The safe starts are 2048 consecutive values modulo [`ALIAS_PERIOD`]. At most 2047 values
/// lie past `BOUNCE_LEN - BOUNCE_CHUNK`, so one of them is always in range: the first safe
/// start, or 0 when the safe run wraps round to it.
///
/// # Examples
///
/// ```
/// use ferric_pcache::copy::{BOUNCE_CHUNK, BOUNCE_LEN, aliased, bounce_start};
///
/// let (src, dst, buffer) = (0x1000, 0x7000_0010, 0x9000_0123);
/// let start = bounce_start(buffer, src);
/// assert!(start + BOUNCE_CHUNK <= BOUNCE_LEN);
/// assert!(!aliased(src, buffer + start));
/// assert!(!aliased(buffer + start, dst));
/// ```
pub const fn bounce_start(buffer: usize, src: usize) -> usize {
    let first = distance(buffer, src.wrapping_add(SAFE_FIRST));
    if first <= BOUNCE_LEN - BOUNCE_CHUNK {
        first
    } else {
        // The safe run is the 2048 distances from `first`, modulo the period; it is long
        // enough to wrap round past 0 whenever `first` is out of range.
        0
    }
}

/// Copies `src` to the destination at address `dst` with `write(offset, bytes)`, which
/// writes `bytes` at `offset` in the destination. An aliased copy goes through `bounce` in
/// pieces of at most [`BOUNCE_CHUNK`] bytes; any other copy is one `write` of all of `src`.
///
/// # Errors
///
/// The first error `write` returns. The bytes before it may have been written.
///
/// # Examples
///
/// ```
/// use ferric_pcache::copy::{BOUNCE_LEN, stage};
///
/// let page = vec![7u8; 4096];
/// let mut out = vec![0u8; 4096];
/// let dst = page.as_ptr() as usize + 16;
/// let mut bounce = [0; BOUNCE_LEN];
/// stage(&page, dst, &mut bounce, |at, bytes| {
///     out[at..at + bytes.len()].copy_from_slice(bytes);
///     Ok::<(), ()>(())
/// })
/// .unwrap();
/// assert_eq!(out, page);
/// ```
pub fn stage<E>(
    src: &[u8],
    dst: usize,
    bounce: &mut Bounce,
    mut write: impl FnMut(usize, &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    if !aliased(src.as_ptr() as usize, dst) {
        return write(0, src);
    }
    let buffer = bounce.as_ptr() as usize;
    for (i, piece) in src.chunks(BOUNCE_CHUNK).enumerate() {
        let start = bounce_start(buffer, piece.as_ptr() as usize);
        let at = i * BOUNCE_CHUNK;
        match bounce.get_mut(start..start + piece.len()) {
            Some(staged) => {
                staged.copy_from_slice(piece);
                write(at, staged)?;
            }
            // `bounce_start` keeps a whole piece in range; copy directly rather than panic.
            None => write(at, piece)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_forward_distances_are_aliased() {
        for d in 1..=ALIAS_WINDOW {
            assert!(aliased(0x5000, 0x5000 + d), "{d}");
            assert!(aliased(0x5000 + 7 * ALIAS_PERIOD, 0x9000 + d), "{d}");
        }
        assert!(!aliased(0x5000, 0x5000));
        assert!(!aliased(0x5000, 0x5000 + ALIAS_WINDOW + 1));
        // The destination a little before the source is not aliased.
        assert!(!aliased(0x5010, 0x5000));
    }

    #[test]
    fn every_bounce_start_fits_and_breaks_the_aliasing() {
        // Only the source's position relative to the buffer, and the destination's
        // relative to the source, matter: check every one.
        let buffer = 0x10_0000;
        for rel in 0..ALIAS_PERIOD {
            let src = 0x40_0000 + rel;
            let start = bounce_start(buffer, src);
            assert!(
                start + BOUNCE_CHUNK <= BOUNCE_LEN,
                "rel {rel}: start {start}"
            );
            let bounce = buffer + start;
            assert!(!aliased(src, bounce), "rel {rel}");
            for d in 1..=ALIAS_WINDOW {
                assert!(!aliased(bounce, src + d), "rel {rel} d {d}");
            }
        }
    }

    #[test]
    fn the_safe_run_is_what_bounce_start_relies_on() {
        let run = (ALIAS_PERIOD - ALIAS_WINDOW) - SAFE_FIRST + 1;
        assert_eq!(run, ALIAS_PERIOD / 2);
        // Out-of-range starts are fewer than the run, so the run reaches 0 from any of them.
        assert!(ALIAS_PERIOD - (BOUNCE_LEN - BOUNCE_CHUNK + 1) < run);
    }

    #[test]
    fn the_top_of_the_address_space_does_not_overflow() {
        assert!(aliased(usize::MAX - 15, 0));
        assert!(bounce_start(usize::MAX, usize::MAX) <= BOUNCE_LEN - BOUNCE_CHUNK);
    }

    /// A page copied to a destination 16 bytes past it, modulo the period: the case the
    /// kernel bounces.
    #[test]
    fn an_aliased_page_is_staged_in_pieces_and_arrives_whole() {
        let mut page = [0u8; ALIAS_PERIOD];
        for (i, byte) in page.iter_mut().enumerate() {
            *byte = (i * 7 % 251) as u8;
        }
        for d in [1, 16, 64, ALIAS_WINDOW] {
            let dst = page.as_ptr() as usize + d;
            let mut bounce = [0; BOUNCE_LEN];
            let mut out = [0; ALIAS_PERIOD];
            let mut pieces = 0;
            let range = bounce_range(&bounce);
            stage(&page, dst, &mut bounce, |at, bytes| {
                let staged = bytes.as_ptr() as usize;
                // Every piece comes from the bounce, never straight from the page, and
                // neither copy it took part in is aliased.
                let from_bounce = range.contains(&staged);
                assert!(from_bounce, "d {d}: piece at {at} skipped the bounce");
                assert!(!aliased(page.as_ptr() as usize + at, staged), "d {d}");
                assert!(!aliased(staged, dst + at), "d {d}");
                out[at..at + bytes.len()].copy_from_slice(bytes);
                pieces += 1;
                Ok::<(), ()>(())
            })
            .expect("the writer does not fail");
            assert_eq!(out, page, "d {d}");
            assert_eq!(pieces, ALIAS_PERIOD / BOUNCE_CHUNK, "d {d}");
        }
    }

    fn bounce_range(bounce: &Bounce) -> core::ops::Range<usize> {
        let start = bounce.as_ptr() as usize;
        start..start + BOUNCE_LEN
    }

    #[test]
    fn an_unaliased_copy_is_one_direct_write() {
        let page = [3u8; 1000];
        let src = page.as_ptr() as usize;
        let mut bounce = [0; BOUNCE_LEN];
        let mut writes = 0;
        stage(&page, src + 2048, &mut bounce, |at, bytes| {
            assert_eq!((at, bytes.as_ptr() as usize, bytes.len()), (0, src, 1000));
            writes += 1;
            Ok::<(), ()>(())
        })
        .expect("the writer does not fail");
        assert_eq!(writes, 1);
    }

    #[test]
    fn a_write_error_stops_the_copy() {
        let page = [1u8; ALIAS_PERIOD];
        let mut bounce = [0; BOUNCE_LEN];
        let mut calls = 0;
        let result = stage(&page, page.as_ptr() as usize + 16, &mut bounce, |_, _| {
            calls += 1;
            if calls == 2 { Err("fault") } else { Ok(()) }
        });
        assert_eq!(result, Err("fault"));
        assert_eq!(calls, 2);
    }
}
