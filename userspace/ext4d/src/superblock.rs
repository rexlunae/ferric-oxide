//! The superblock fields `ext4d` reads itself: block size, block and inode counts, and the
//! volume's free space, for `statvfs`.
//!
//! Written from "The Super Block" in "Ext4 Disk Layout" on the ext4 wiki at kernel.org
//! (<https://ext4.wiki.kernel.org/index.php/Ext4_Disk_Layout#The_Super_Block>). The
//! superblock is 1024 bytes at byte 1024 of the volume, little-endian:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0x0 | 4 | `s_inodes_count` |
//! | 0x4 | 4 | `s_blocks_count_lo` |
//! | 0x8 | 4 | `s_r_blocks_count_lo` (reserved for the superuser) |
//! | 0xC | 4 | `s_free_blocks_count_lo` |
//! | 0x10 | 4 | `s_free_inodes_count` |
//! | 0x18 | 4 | `s_log_block_size`: the block size is `2 ^ (10 + s_log_block_size)` |
//! | 0x38 | 2 | `s_magic`, 0xEF53 |
//! | 0x5C | 4 | `s_feature_compat`; 0x4 is `COMPAT_HAS_JOURNAL` |
//! | 0x60 | 4 | `s_feature_incompat`; 0x4 is `INCOMPAT_RECOVER` (the journal needs replaying), 0x80 is `INCOMPAT_64BIT` |
//! | 0x150 | 4 | `s_blocks_count_hi` (with `INCOMPAT_64BIT`) |
//! | 0x154 | 4 | `s_r_blocks_count_hi` (with `INCOMPAT_64BIT`) |
//! | 0x158 | 4 | `s_free_blocks_count_hi` (with `INCOMPAT_64BIT`) |
//!
//! The ext4 reader checks the rest (features, checksums) when it loads the volume.

use thiserror::Error;

pub(crate) mod recovery;

/// Where the superblock starts on the volume.
pub const OFFSET: u64 = 1024;
/// The superblock's size in bytes.
pub const SIZE: usize = 1024;
/// `s_magic`.
pub const MAGIC: u16 = 0xEF53;
/// `INCOMPAT_64BIT` in `s_feature_incompat`: block counts have high halves.
pub(crate) const INCOMPAT_64BIT: u32 = 0x80;
/// `COMPAT_HAS_JOURNAL` in `s_feature_compat`: the volume has a journal.
pub(crate) const COMPAT_HAS_JOURNAL: u32 = 0x4;
/// `INCOMPAT_RECOVER` in `s_feature_incompat`: the journal needs replaying.
pub(crate) const INCOMPAT_RECOVER: u32 = 0x4;
/// The largest `s_log_block_size` the format allows (64 KiB blocks).
const MAX_LOG_BLOCK_SIZE: u32 = 6;

/// Why the superblock was refused.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuperblockError {
    /// `s_magic` is not 0xEF53: this is not an ext2, ext3 or ext4 volume.
    #[error("not an ext4 file system (magic {found:#06x}, expected 0xef53)")]
    BadMagic {
        /// The value found.
        found: u16,
    },
    /// `s_log_block_size` gives a block size the format does not allow.
    #[error("unsupported block size (s_log_block_size {log})")]
    BadBlockSize {
        /// The value found.
        log: u32,
    },
}

/// The volume's size and free space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The block size in bytes.
    pub block_size: u32,
    /// The number of blocks.
    pub blocks: u64,
    /// The number of free blocks.
    pub free_blocks: u64,
    /// The number of blocks reserved for the superuser.
    pub reserved_blocks: u64,
    /// The number of inodes.
    pub inodes: u32,
    /// The number of free inodes.
    pub free_inodes: u32,
}

impl Geometry {
    /// Reads the geometry from the superblock's bytes (the [`SIZE`] bytes at [`OFFSET`]).
    ///
    /// # Errors
    ///
    /// [`SuperblockError::BadMagic`] when the bytes are not an ext4 superblock, and
    /// [`SuperblockError::BadBlockSize`] when the block size is out of range.
    ///
    /// # Examples
    ///
    /// ```
    /// use ext4d::superblock::{Geometry, SIZE};
    ///
    /// let mut sb = [0u8; SIZE];
    /// sb[0x4..0x8].copy_from_slice(&4096u32.to_le_bytes()); // s_blocks_count_lo
    /// sb[0x18..0x1c].copy_from_slice(&2u32.to_le_bytes()); // 4 KiB blocks
    /// sb[0x38..0x3a].copy_from_slice(&0xef53u16.to_le_bytes());
    /// let geometry = Geometry::parse(&sb).expect("a superblock");
    /// assert_eq!((geometry.block_size, geometry.blocks), (4096, 4096));
    /// ```
    pub fn parse(sb: &[u8; SIZE]) -> Result<Self, SuperblockError> {
        let magic = u16::from_le_bytes([sb[0x38], sb[0x39]]);
        if magic != MAGIC {
            return Err(SuperblockError::BadMagic { found: magic });
        }
        let log = le32(sb, 0x18);
        if log > MAX_LOG_BLOCK_SIZE {
            return Err(SuperblockError::BadBlockSize { log });
        }
        let wide = le32(sb, 0x60) & INCOMPAT_64BIT != 0;
        let count = |lo: usize, hi: usize| {
            let high = if wide { le32(sb, hi) } else { 0 };
            u64::from(high) << 32 | u64::from(le32(sb, lo))
        };
        Ok(Self {
            block_size: 1024 << log,
            blocks: count(0x4, 0x150),
            reserved_blocks: count(0x8, 0x154),
            free_blocks: count(0xC, 0x158),
            inodes: le32(sb, 0x0),
            free_inodes: le32(sb, 0x10),
        })
    }

    /// The geometry as the scheme ABI's `StatVfs`.
    pub fn to_statvfs(&self) -> syscall::data::StatVfs {
        syscall::data::StatVfs {
            f_bsize: self.block_size,
            f_blocks: self.blocks,
            f_bfree: self.free_blocks,
            f_bavail: self.available_blocks(),
        }
    }

    /// The blocks an unprivileged user may still allocate: the free blocks less the ones
    /// reserved for the superuser.
    pub const fn available_blocks(&self) -> u64 {
        self.free_blocks.saturating_sub(self.reserved_blocks)
    }
}

/// Whether the volume was not cleanly unmounted: it has a journal and `INCOMPAT_RECOVER`.
pub fn needs_recovery(sb: &[u8; SIZE]) -> bool {
    le32(sb, 0x5C) & COMPAT_HAS_JOURNAL != 0 && le32(sb, 0x60) & INCOMPAT_RECOVER != 0
}

fn le32(sb: &[u8; SIZE], at: usize) -> u32 {
    u32::from_le_bytes([sb[at], sb[at + 1], sb[at + 2], sb[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn superblock() -> [u8; SIZE] {
        let mut sb = [0u8; SIZE];
        let mut put = |at: usize, v: u32| sb[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(0x0, 1024);
        put(0x4, 4096);
        put(0x8, 204);
        put(0xC, 2359);
        put(0x10, 806);
        put(0x18, 0);
        put(0x150, 1);
        put(0x158, 2);
        sb[0x38..0x3a].copy_from_slice(&MAGIC.to_le_bytes());
        sb
    }

    #[test]
    fn reads_the_counts() {
        let g = Geometry::parse(&superblock()).expect("valid");
        assert_eq!(
            g,
            Geometry {
                block_size: 1024,
                blocks: 4096,
                free_blocks: 2359,
                reserved_blocks: 204,
                inodes: 1024,
                free_inodes: 806,
            }
        );
        assert_eq!(g.available_blocks(), 2155);
        let vfs = g.to_statvfs();
        assert_eq!(
            (vfs.f_bsize, vfs.f_blocks, vfs.f_bfree, vfs.f_bavail),
            (1024, 4096, 2359, 2155)
        );
    }

    #[test]
    fn high_halves_count_only_with_64bit() {
        let mut sb = superblock();
        sb[0x60..0x64].copy_from_slice(&INCOMPAT_64BIT.to_le_bytes());
        let g = Geometry::parse(&sb).expect("valid");
        assert_eq!(g.blocks, (1 << 32) | 4096);
        assert_eq!(g.free_blocks, (2 << 32) | 2359);
        assert_eq!(g.reserved_blocks, 204);
    }

    #[test]
    fn block_sizes_follow_the_log() {
        let mut sb = superblock();
        sb[0x18..0x1c].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(Geometry::parse(&sb).map(|g| g.block_size), Ok(4096));
        sb[0x18..0x1c].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            Geometry::parse(&sb),
            Err(SuperblockError::BadBlockSize { log: 7 })
        );
    }

    #[test]
    fn recovery_needs_a_journal_and_the_flag() {
        let mut sb = superblock();
        assert!(!needs_recovery(&sb));
        sb[0x60..0x64].copy_from_slice(&INCOMPAT_RECOVER.to_le_bytes());
        assert!(!needs_recovery(&sb), "no journal");
        sb[0x5C..0x60].copy_from_slice(&COMPAT_HAS_JOURNAL.to_le_bytes());
        assert!(needs_recovery(&sb));
    }

    #[test]
    fn other_volumes_are_refused() {
        let mut sb = superblock();
        sb[0x38] = 0;
        assert_eq!(
            Geometry::parse(&sb),
            Err(SuperblockError::BadMagic { found: 0xef00 })
        );
    }
}
