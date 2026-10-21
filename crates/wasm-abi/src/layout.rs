//! Structures the imports write into linear memory.
//!
//! Each is a fixed sequence of little-endian fields with no padding, encoded into a byte
//! array of its `SIZE`, so a guest of either memory width reads it the same way. Guests
//! decode them with the matching `decode`.

use crate::errno::Errno;

/// A time: seconds and nanoseconds (16 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timespec {
    /// Seconds.
    pub seconds: i64,
    /// Nanoseconds, `0..1_000_000_000`.
    pub nanoseconds: u32,
}

impl Timespec {
    /// The encoded size.
    pub const SIZE: usize = 16;

    /// The encoding: `seconds: i64`, `nanoseconds: u32`, four zero bytes.
    pub fn encode(self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        out[..8].copy_from_slice(&self.seconds.to_le_bytes());
        out[8..12].copy_from_slice(&self.nanoseconds.to_le_bytes());
        out
    }

    /// Decodes one, as `futimens` reads them from a guest.
    ///
    /// # Errors
    ///
    /// [`Errno::EINVAL`] if the nanoseconds are out of range or the padding is not zero.
    pub fn decode(bytes: &[u8; Self::SIZE]) -> Result<Self, Errno> {
        let nanoseconds = u32::from_le_bytes(word(bytes, 8));
        if nanoseconds >= 1_000_000_000 || bytes[12..] != [0; 4] {
            return Err(Errno::EINVAL);
        }
        Ok(Self {
            seconds: i64::from_le_bytes(word(bytes, 0)),
            nanoseconds,
        })
    }
}

/// What `fstat` writes (88 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stat {
    /// Device id.
    pub dev: u64,
    /// Inode number.
    pub ino: u64,
    /// File type and permission bits, as Redox's `st_mode`.
    pub mode: u32,
    /// Hard links.
    pub nlink: u32,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Size in bytes.
    pub size: u64,
    /// Preferred I/O block size.
    pub blksize: u32,
    /// Blocks of 512 bytes allocated.
    pub blocks: u64,
    /// Last access.
    pub atime: Timespec,
    /// Last modification.
    pub mtime: Timespec,
}

impl Stat {
    /// The encoded size.
    pub const SIZE: usize = 88;

    /// The encoding, in field order (`blksize` followed by four zero bytes).
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut out = [0; Self::SIZE];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            out[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        put(&self.dev.to_le_bytes());
        put(&self.ino.to_le_bytes());
        put(&self.mode.to_le_bytes());
        put(&self.nlink.to_le_bytes());
        put(&self.uid.to_le_bytes());
        put(&self.gid.to_le_bytes());
        put(&self.size.to_le_bytes());
        put(&self.blksize.to_le_bytes());
        put(&[0; 4]);
        put(&self.blocks.to_le_bytes());
        put(&self.atime.encode());
        put(&self.mtime.encode());
        out
    }

    /// Decodes one, as guest bindings do.
    ///
    /// # Errors
    ///
    /// [`Errno::EINVAL`] if a time or the padding is invalid.
    pub fn decode(bytes: &[u8; Self::SIZE]) -> Result<Self, Errno> {
        if bytes[44..48] != [0; 4] {
            return Err(Errno::EINVAL);
        }
        Ok(Self {
            dev: u64::from_le_bytes(word(bytes, 0)),
            ino: u64::from_le_bytes(word(bytes, 8)),
            mode: u32::from_le_bytes(word(bytes, 16)),
            nlink: u32::from_le_bytes(word(bytes, 20)),
            uid: u32::from_le_bytes(word(bytes, 24)),
            gid: u32::from_le_bytes(word(bytes, 28)),
            size: u64::from_le_bytes(word(bytes, 32)),
            blksize: u32::from_le_bytes(word(bytes, 40)),
            blocks: u64::from_le_bytes(word(bytes, 48)),
            atime: Timespec::decode(&word(bytes, 56))?,
            mtime: Timespec::decode(&word(bytes, 72))?,
        })
    }
}

/// What `fstatvfs` writes (48 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatVfs {
    /// Block size.
    pub block_size: u64,
    /// Total blocks.
    pub blocks: u64,
    /// Free blocks.
    pub blocks_free: u64,
    /// Blocks available to unprivileged users.
    pub blocks_available: u64,
    /// Total inodes, or 0 if the file system does not count them.
    pub files: u64,
    /// Free inodes.
    pub files_free: u64,
}

impl StatVfs {
    /// The encoded size.
    pub const SIZE: usize = 48;

    /// The encoding, six `u64`s in field order.
    pub fn encode(&self) -> [u8; Self::SIZE] {
        let fields = [
            self.block_size,
            self.blocks,
            self.blocks_free,
            self.blocks_available,
            self.files,
            self.files_free,
        ];
        let mut out = [0; Self::SIZE];
        for (chunk, field) in out.chunks_exact_mut(8).zip(fields) {
            chunk.copy_from_slice(&field.to_le_bytes());
        }
        out
    }

    /// Decodes one.
    pub fn decode(bytes: &[u8; Self::SIZE]) -> Self {
        let field = |index: usize| u64::from_le_bytes(word(bytes, index * 8));
        Self {
            block_size: field(0),
            blocks: field(1),
            blocks_free: field(2),
            blocks_available: field(3),
            files: field(4),
            files_free: field(5),
        }
    }
}

/// `N` bytes of `bytes` from `at`; callers pass offsets inside their fixed-size arrays.
fn word<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structures_round_trip_with_fixed_bytes() {
        let time = Timespec {
            seconds: -2,
            nanoseconds: 999_999_999,
        };
        assert_eq!(Timespec::decode(&time.encode()), Ok(time));
        assert_eq!(
            time.encode(),
            [
                0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x9a, 0x3b, 0, 0, 0, 0
            ]
        );
        let mut bad = time.encode();
        bad[12] = 1;
        assert_eq!(Timespec::decode(&bad), Err(Errno::EINVAL));
        let too_many = Timespec {
            seconds: 0,
            nanoseconds: 1_000_000_000,
        };
        assert_eq!(Timespec::decode(&too_many.encode()), Err(Errno::EINVAL));

        let stat = Stat {
            dev: 1,
            ino: 2,
            mode: 0o100644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            size: 4096,
            blksize: 4096,
            blocks: 8,
            atime: time,
            mtime: Timespec {
                seconds: 7,
                nanoseconds: 0,
            },
        };
        let bytes = stat.encode();
        assert_eq!(Stat::decode(&bytes), Ok(stat));
        assert_eq!(&bytes[16..20], &0o100644u32.to_le_bytes());
        assert_eq!(&bytes[48..56], &8u64.to_le_bytes());
        assert_eq!(&bytes[72..80], &7i64.to_le_bytes());

        let vfs = StatVfs {
            block_size: 4096,
            blocks: 10,
            blocks_free: 5,
            blocks_available: 4,
            files: 0,
            files_free: 0,
        };
        assert_eq!(StatVfs::decode(&vfs.encode()), vfs);
    }
}
