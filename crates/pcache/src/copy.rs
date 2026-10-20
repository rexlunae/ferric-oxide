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
//! The cure is to copy each page through a bounce buffer placed half a page away from the
//! source, modulo 4096. Neither copy is then aliased: the bounce is 2048 bytes past the
//! source, and the destination is about 2048 bytes past the bounce. The bounce page stays
//! in L1. [`aliased`] says when a copy needs this, and [`bounce_start`] says where in a
//! [`BOUNCE_LEN`]-byte buffer the page goes.

/// The page size the aliasing is modulo: the low 12 bits of an address.
pub const ALIAS_PERIOD: usize = 4096;
/// How far past the source, modulo [`ALIAS_PERIOD`], a destination counts as aliased.
/// Loads run at most this far ahead of the stores they could be confused with. 1 KiB
/// covers a `rep movsb` in flight with room to spare.
pub const ALIAS_WINDOW: usize = 1024;
/// The size of a bounce buffer: two periods, so a page fits at any offset modulo 4096.
pub const BOUNCE_LEN: usize = 2 * ALIAS_PERIOD;

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

/// Where, in a bounce buffer of [`BOUNCE_LEN`] bytes at `buffer`, a copy from `src` goes so
/// that it starts half a period past the source, modulo [`ALIAS_PERIOD`]. The result is
/// below [`ALIAS_PERIOD`], so a whole page from there fits in the buffer.
///
/// # Examples
///
/// ```
/// use ferric_pcache::copy::{ALIAS_PERIOD, aliased, bounce_start};
///
/// let (src, dst, buffer) = (0x1000, 0x7000_0010, 0x9000_0123);
/// let bounce = buffer + bounce_start(buffer, src);
/// assert!(!aliased(src, bounce));
/// assert!(!aliased(bounce, dst));
/// assert!(bounce - buffer < ALIAS_PERIOD);
/// ```
pub const fn bounce_start(buffer: usize, src: usize) -> usize {
    distance(buffer, src.wrapping_add(ALIAS_PERIOD / 2))
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
    fn a_bounce_breaks_every_aliased_distance() {
        for buffer in [0x10_0000, 0x10_0001, 0x10_0123, 0x10_0ff0] {
            for src_off in [0, 16, 2048, 4095] {
                let src = 0x40_0000 + src_off;
                for d in 1..=ALIAS_WINDOW {
                    let dst = src + d;
                    let start = bounce_start(buffer, src);
                    assert!(start < ALIAS_PERIOD);
                    assert!(start + ALIAS_PERIOD <= BOUNCE_LEN);
                    let bounce = buffer + start;
                    assert!(!aliased(src, bounce), "src {src:#x} bounce {bounce:#x}");
                    assert!(!aliased(bounce, dst), "bounce {bounce:#x} dst {dst:#x}");
                }
            }
        }
    }

    #[test]
    fn the_top_of_the_address_space_does_not_overflow() {
        assert!(aliased(usize::MAX - 15, 0));
        assert!(bounce_start(usize::MAX, usize::MAX) < ALIAS_PERIOD);
    }
}
