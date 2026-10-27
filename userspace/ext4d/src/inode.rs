//! Fields of the on-disk inode that `ext4-view` does not expose, read and (with `write`)
//! written in place: the timestamps and the extra-field size that says which of their
//! high-precision halves an inode has.
//!
//! # Specification
//!
//! Written from "Ext4 Disk Layout" on the ext4 wiki at kernel.org: "Inode Table" (the field
//! offsets and `i_extra_isize`) and "Inode Timestamps" (the epoch bits and nanoseconds in the
//! `_extra` fields). No code here comes from the Linux kernel or from e2fsprogs.

#[cfg(feature = "write")]
pub(crate) mod data;

/// `i_atime`, `i_ctime`, `i_mtime`: the low 32 bits of the seconds, signed.
pub(crate) const I_ATIME: usize = 0x8;
pub(crate) const I_CTIME: usize = 0xC;
pub(crate) const I_MTIME: usize = 0x10;
/// `i_extra_isize`: how many bytes past the first 128 the fields below take.
pub(crate) const I_EXTRA_ISIZE: usize = 0x80;
/// `i_ctime_extra`, `i_mtime_extra`, `i_atime_extra`: epoch bits and nanoseconds.
pub(crate) const I_CTIME_EXTRA: usize = 0x84;
pub(crate) const I_MTIME_EXTRA: usize = 0x88;
pub(crate) const I_ATIME_EXTRA: usize = 0x8C;
/// The size of the original (revision 0) inode, before the extra fields.
pub(crate) const GOOD_OLD_INODE_SIZE: usize = 128;

/// Whether the inode's extra fields reach past byte `end`.
pub(crate) fn has_extra(inode: &[u8], end: usize) -> bool {
    inode.len() > GOOD_OLD_INODE_SIZE
        && GOOD_OLD_INODE_SIZE + usize::from(le16(inode, I_EXTRA_ISIZE)) >= end
        && inode.len() >= end
}

/// The time in the seconds field at `field` and, if the inode has it, the extra field at
/// `extra`, as seconds since the epoch and nanoseconds.
///
/// The seconds field holds the low 32 bits of the seconds, read as signed. The extra field's
/// low two bits (the epoch) add multiples of 2^32 to that, and its upper 30 bits hold the
/// nanoseconds. Without the extra field the time is whole seconds from 1901 to 2038.
pub(crate) fn time(inode: &[u8], field: usize, extra: usize) -> (i64, u32) {
    let low = i64::from(le32(inode, field) as i32);
    if has_extra(inode, extra + 4) {
        let encoded = le32(inode, extra);
        (low + (i64::from(encoded & 3) << 32), encoded >> 2)
    } else {
        (low, 0)
    }
}

/// The inode's access, modification and change times, all read from the same copy of it.
pub(crate) fn times(inode: &[u8]) -> InodeTimes {
    InodeTimes {
        accessed: time(inode, I_ATIME, I_ATIME_EXTRA),
        modified: time(inode, I_MTIME, I_MTIME_EXTRA),
        changed: time(inode, I_CTIME, I_CTIME_EXTRA),
    }
}

/// The little-endian `u16` at `at`; every caller's buffer is long enough.
pub(crate) fn le16(raw: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([raw[at], raw[at + 1]])
}

/// The little-endian `u32` at `at`; every caller's buffer is long enough.
fn le32(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

use crate::lookup::InodeTimes;

#[cfg(test)]
mod tests {
    use super::*;

    fn change_time(inode: &[u8]) -> (i64, u32) {
        times(inode).changed
    }

    /// A 256-byte inode with `i_extra_isize` `extra_isize`, change time `ctime` and
    /// `i_ctime_extra` `extra`.
    fn inode(extra_isize: u16, ctime: u32, extra: u32) -> Vec<u8> {
        let mut inode = vec![0; 256];
        inode[I_EXTRA_ISIZE..I_EXTRA_ISIZE + 2].copy_from_slice(&extra_isize.to_le_bytes());
        inode[I_CTIME..I_CTIME + 4].copy_from_slice(&ctime.to_le_bytes());
        inode[I_CTIME_EXTRA..I_CTIME_EXTRA + 4].copy_from_slice(&extra.to_le_bytes());
        inode
    }

    #[test]
    fn the_extra_field_adds_epochs_and_nanoseconds() {
        // basic.ext4's files: 0x6adbb8f6 with an empty extra field.
        assert_eq!(change_time(&inode(32, 0x6ADB_B8F6, 0)), (0x6ADB_B8F6, 0));
        // 999_999_999 ns in epoch 0; then the epochs past 2038, up to 2446.
        let ns = 999_999_999u32 << 2;
        assert_eq!(change_time(&inode(32, 5, ns)), (5, 999_999_999));
        assert_eq!(change_time(&inode(32, 0x8000_0000, 1)), (1 << 31, 0));
        assert_eq!(
            change_time(&inode(32, 0x7FFF_FFFF, 3)),
            (3 * (1 << 32) + i64::from(i32::MAX), 0)
        );
        // Before 1970.
        assert_eq!(change_time(&inode(32, u32::MAX, 0)), (-1, 0));
    }

    #[test]
    fn without_the_extra_field_times_are_32_bit_seconds() {
        // i_extra_isize too small to reach i_ctime_extra, and a 128-byte inode.
        assert_eq!(
            change_time(&inode(4, 0x8000_0000, 7 << 2)),
            (i64::from(i32::MIN), 0)
        );
        let mut small = inode(32, 9, 7 << 2);
        small.truncate(GOOD_OLD_INODE_SIZE);
        assert_eq!(change_time(&small), (9, 0));
    }
}
