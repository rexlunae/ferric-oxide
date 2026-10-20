//! How the kernel copies cached bytes to a reader's buffer.
//!
//! Cached pages start on page boundaries, and a reader's buffer usually does not: an
//! allocation from `malloc` typically starts a few bytes into a page. On AMD Zen 3 and Zen 4
//! cores, `rep movsb` from an aligned source to a misaligned destination runs many times
//! slower than the same copy with an aligned destination. The GNU C Library tracks this as
//! bug 30994, and its `memcpy` avoids `rep movsb` on those cores for that reason. CI runners
//! on AMD EPYC 7763 (Zen 3) showed it here: warm 1 MiB reads from the cache ran at about
//! 2 GB/s, where an Intel runner reached 16 GB/s.
//!
//! [`split_for_destination`] gives the cure the kernel uses. It copies a short head first, up
//! to the next [`DESTINATION_ALIGN`] boundary of the destination, so the bulk of the copy has
//! an aligned destination. Only the source is then misaligned, which those cores handle at
//! full speed. On other cores the split costs one extra short copy per page.

/// The destination alignment the bulk of each copy gets, one cache line.
pub const DESTINATION_ALIGN: usize = 64;

/// Splits a copy of `len` bytes to the address `destination` into a head and the rest. The
/// head is as many bytes as it takes to bring the destination to a [`DESTINATION_ALIGN`]
/// boundary (at most `len`), and the rest starts at that boundary. The two always add up to
/// `len`, and the head is 0 when the destination is already aligned.
///
/// # Examples
///
/// ```
/// use ferric_pcache::copy::split_for_destination;
///
/// // A buffer 16 bytes into a page: 48 bytes reach the next cache line.
/// assert_eq!(split_for_destination(0x7000_0010, 4096), (48, 4048));
/// // An aligned buffer needs no head.
/// assert_eq!(split_for_destination(0x7000_0040, 4096), (0, 4096));
/// // A copy shorter than the head is all head.
/// assert_eq!(split_for_destination(0x7000_0010, 8), (8, 0));
/// ```
pub const fn split_for_destination(destination: usize, len: usize) -> (usize, usize) {
    let to_boundary = destination.wrapping_neg() % DESTINATION_ALIGN;
    let head = if to_boundary < len { to_boundary } else { len };
    (head, len - head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rest_starts_on_a_boundary() {
        for destination in 0x1000..0x1000 + 2 * DESTINATION_ALIGN {
            for len in [0, 1, 63, 64, 65, 4095, 4096] {
                let (head, rest) = split_for_destination(destination, len);
                assert_eq!(head + rest, len);
                assert!(head < DESTINATION_ALIGN);
                if rest > 0 {
                    assert_eq!((destination + head) % DESTINATION_ALIGN, 0);
                }
            }
        }
    }

    #[test]
    fn an_aligned_destination_has_no_head() {
        assert_eq!(split_for_destination(0, 4096), (0, 4096));
        assert_eq!(split_for_destination(0x7fff_0000, 100), (0, 100));
    }

    #[test]
    fn the_top_of_the_address_space_does_not_overflow() {
        assert_eq!(split_for_destination(usize::MAX, 4096), (1, 4095));
        assert_eq!(split_for_destination(usize::MAX - 15, 8), (8, 0));
    }
}
