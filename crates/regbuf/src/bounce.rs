//! How the kernel copies a writable bounce page back into its region.
//!
//! A grant that covers a region page only in part is mapped through a bounce page holding a
//! copy of the granted bytes (`table::PartialPage`). While the mapping exists, the region's own
//! copy of those bytes may change too: the owner writes its buffer, or another grant's mapping
//! is taken down and copies its writes back. Copying the whole bounce page back would undo
//! those newer writes. So the kernel keeps a snapshot of the granted bytes as they were when
//! it filled the bounce page, and [`copy_changed`] copies back only the bytes the holder
//! changed.
//!
//! The rule per byte: a byte the holder changed takes the holder's value; every other byte
//! keeps the region's current value. A holder that writes a byte back to its old value counts
//! as not having changed it, and when the holder and someone else both change a byte, the
//! holder's write, copied back last, wins.

/// Copies into `region` each byte of `bounce` that differs from `snapshot` at the same offset,
/// and returns how many it copied. The three slices are the granted bytes of one page; should
/// their lengths differ, only the common prefix is looked at.
///
/// # Examples
///
/// ```
/// use ferric_regbuf::bounce::copy_changed;
///
/// let snapshot = *b"aaaa";
/// let mut region = *b"abba"; // the owner wrote the middle bytes since
/// let bounce = *b"aaaz"; // the holder wrote only the last byte
/// assert_eq!(copy_changed(&mut region, &bounce, &snapshot), 1);
/// assert_eq!(&region, b"abbz");
/// ```
pub fn copy_changed(region: &mut [u8], bounce: &[u8], snapshot: &[u8]) -> usize {
    let mut copied = 0;
    for ((dst, &now), &then) in region.iter_mut().zip(bounce).zip(snapshot) {
        if now != then {
            *dst = now;
            copied += 1;
        }
    }
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_bounce_keeps_newer_region_writes() {
        // The finding: the owner changed A to B after the mapping; the holder did nothing.
        let snapshot = [b'A'; 8];
        let bounce = snapshot;
        let mut region = [b'B'; 8];
        assert_eq!(copy_changed(&mut region, &bounce, &snapshot), 0);
        assert_eq!(region, [b'B'; 8]);
    }

    #[test]
    fn holder_writes_land_and_other_bytes_stay() {
        let snapshot = [0_u8; 6];
        let bounce = [0, 7, 0, 9, 0, 0];
        // Another grant's teardown wrote byte 4 meanwhile.
        let mut region = [0, 0, 0, 0, 5, 0];
        assert_eq!(copy_changed(&mut region, &bounce, &snapshot), 2);
        assert_eq!(region, [0, 7, 0, 9, 5, 0]);
    }

    #[test]
    fn two_mappings_of_one_partial_page_both_land() {
        // Two writable mappings of the same grant have separate bounce pages; each copies
        // back only its own writes, so neither undoes the other.
        let snapshot = [1_u8; 4];
        let first = [2, 1, 1, 1];
        let second = [1, 1, 1, 3];
        let mut region = snapshot;
        copy_changed(&mut region, &first, &snapshot);
        copy_changed(&mut region, &second, &snapshot);
        assert_eq!(region, [2, 1, 1, 3]);
    }

    #[test]
    fn conflicting_write_takes_the_holders_value() {
        let snapshot = [1_u8];
        let mut region = [2_u8];
        assert_eq!(copy_changed(&mut region, &[3], &snapshot), 1);
        assert_eq!(region, [3]);
    }

    #[test]
    fn mismatched_lengths_use_the_common_prefix() {
        let mut region = [0_u8; 2];
        assert_eq!(copy_changed(&mut region, &[1, 1, 1], &[0, 0, 0, 0]), 2);
        assert_eq!(region, [1, 1]);
    }
}
