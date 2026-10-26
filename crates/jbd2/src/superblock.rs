//! The journal superblock, journal block 0.
//!
//! From "Journal Superblock" in "Ext4 Disk Layout". Every field is big-endian:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0x0 | 12 | block header: `h_magic`, `h_blocktype` (3: version 1, 4: version 2), `h_sequence` |
//! | 0xC | 4 | `s_blocksize` |
//! | 0x10 | 4 | `s_maxlen`: journal length in blocks |
//! | 0x14 | 4 | `s_first`: the first block of the log |
//! | 0x18 | 4 | `s_sequence`: the first transaction expected in the log |
//! | 0x1C | 4 | `s_start`: the block the log starts at; 0 means the journal is clean |
//! | 0x20 | 4 | `s_errno`: nonzero when the journal was aborted on an error |
//! | 0x24 | 4 | `s_feature_compat` (version 2) |
//! | 0x28 | 4 | `s_feature_incompat` (version 2) |
//! | 0x2C | 4 | `s_feature_ro_compat` (version 2) |
//! | 0x30 | 16 | `s_uuid` |
//! | 0x50 | 1 | `s_checksum_type`: 4 is CRC32C |
//! | 0xFC | 4 | `s_checksum`: CRC32C of the 1024-byte superblock with this field zero |

use thiserror::Error;

use crate::MAGIC;
use crate::checksum::crc32c_without;

/// The journal superblock's size in bytes; the rest of journal block 0 is unused.
pub const SIZE: usize = 1024;

const BLOCK_TYPE_V1: u32 = 3;
const BLOCK_TYPE_V2: u32 = 4;

/// `COMPAT_CHECKSUM`: version 1 commit-block checksums.
const COMPAT_CHECKSUM: u32 = 0x1;
/// `INCOMPAT_REVOKE`: the log has revoke blocks.
const INCOMPAT_REVOKE: u32 = 0x1;
/// `INCOMPAT_64BIT`: block numbers in tags and revoke records are 64 bits.
const INCOMPAT_64BIT: u32 = 0x2;
/// `INCOMPAT_ASYNC_COMMIT`: commit blocks may be written before the blocks they commit.
const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
/// `INCOMPAT_CSUM_V2`: version 2 checksums (16-bit tag checksums).
const INCOMPAT_CSUM_V2: u32 = 0x8;
/// `INCOMPAT_CSUM_V3`: version 3 checksums (32-bit tag checksums, 16-byte tags).
const INCOMPAT_CSUM_V3: u32 = 0x10;
/// The incompatible features this crate understands. `INCOMPAT_FAST_COMMIT` (0x20) is not
/// among them.
const INCOMPAT_KNOWN: u32 =
    INCOMPAT_REVOKE | INCOMPAT_64BIT | INCOMPAT_ASYNC_COMMIT | INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3;
/// `s_checksum_type` for CRC32C.
const CHECKSUM_TYPE_CRC32C: u8 = 4;
/// Where `s_checksum` is.
const CHECKSUM_AT: usize = 0xFC;

/// Which checksums the journal carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChecksumKind {
    /// None.
    None,
    /// Version 1 (`COMPAT_CHECKSUM`): a checksum in each commit block over the transaction,
    /// which this crate does not verify.
    V1,
    /// Version 2: CRC32C of the superblock, descriptor, revoke and commit blocks, and the low
    /// 16 bits of a CRC32C of each data block in its tag.
    V2,
    /// Version 3: as version 2, with full 32-bit tag checksums.
    V3,
}

impl ChecksumKind {
    /// Whether blocks end in (or carry) CRC32C checksums: version 2 or 3.
    pub const fn is_crc32c(self) -> bool {
        matches!(self, Self::V2 | Self::V3)
    }
}

/// Why a journal superblock was refused.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuperblockError {
    /// Fewer than the superblock's 1024 bytes were given.
    #[error("a journal superblock is {SIZE} bytes, got {len}")]
    TooShort {
        /// The number of bytes given.
        len: usize,
    },
    /// `h_magic` is wrong: this is not a JBD2 journal.
    #[error("not a JBD2 journal (magic {found:#010x})")]
    BadMagic {
        /// The value found.
        found: u32,
    },
    /// The block is a journal block, but not a superblock.
    #[error("journal block 0 has block type {found}, not a superblock")]
    NotASuperblock {
        /// The `h_blocktype` found.
        found: u32,
    },
    /// `s_blocksize` is not a power of two from 1 KiB to 64 KiB.
    #[error("unsupported journal block size {size}")]
    BadBlockSize {
        /// The value found.
        size: u32,
    },
    /// `s_first`, `s_maxlen` or `s_start` do not describe a log inside the journal.
    #[error("bad journal geometry (first {first}, length {max_len}, start {start})")]
    BadGeometry {
        /// `s_first`.
        first: u32,
        /// `s_maxlen`.
        max_len: u32,
        /// `s_start`.
        start: u32,
    },
    /// The journal uses incompatible features this crate does not implement.
    #[error("unsupported journal features {bits:#x}")]
    UnsupportedFeatures {
        /// The `s_feature_incompat` bits not understood.
        bits: u32,
    },
    /// Version 2 or 3 checksums with an algorithm other than CRC32C.
    #[error("unsupported journal checksum type {found}")]
    UnsupportedChecksumType {
        /// The `s_checksum_type` found.
        found: u8,
    },
    /// `s_checksum` does not match the superblock.
    #[error("journal superblock checksum {stored:#010x}, expected {computed:#010x}")]
    Checksum {
        /// The value stored.
        stored: u32,
        /// The value computed.
        computed: u32,
    },
}

/// A parsed journal superblock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Superblock {
    block_size: u32,
    max_len: u32,
    first: u32,
    sequence: u32,
    start: u32,
    errno: i32,
    incompat: u32,
    checksum: ChecksumKind,
    uuid: [u8; 16],
}

impl Superblock {
    /// Parses and checks the superblock in the first 1024 bytes of `raw`.
    ///
    /// # Errors
    ///
    /// A [`SuperblockError`] naming the first check that failed: length, magic, block type,
    /// block size, geometry, features, checksum type and (for version 2 and 3 checksums) the
    /// superblock checksum, in that order.
    pub fn parse(raw: &[u8]) -> Result<Self, SuperblockError> {
        let raw = raw
            .get(..SIZE)
            .ok_or(SuperblockError::TooShort { len: raw.len() })?;
        let magic = be32(raw, 0x0);
        if magic != MAGIC {
            return Err(SuperblockError::BadMagic { found: magic });
        }
        let block_type = be32(raw, 0x4);
        let v2 = match block_type {
            BLOCK_TYPE_V1 => false,
            BLOCK_TYPE_V2 => true,
            found => return Err(SuperblockError::NotASuperblock { found }),
        };
        let block_size = be32(raw, 0xC);
        if !block_size.is_power_of_two() || !(1024..=65536).contains(&block_size) {
            return Err(SuperblockError::BadBlockSize { size: block_size });
        }
        let (max_len, first, start) = (be32(raw, 0x10), be32(raw, 0x14), be32(raw, 0x1C));
        if first == 0 || first >= max_len || (start != 0 && !(first..max_len).contains(&start)) {
            return Err(SuperblockError::BadGeometry {
                first,
                max_len,
                start,
            });
        }
        // Version 1 superblocks end before the feature fields.
        let (compat, incompat) = if v2 {
            (be32(raw, 0x24), be32(raw, 0x28))
        } else {
            (0, 0)
        };
        if incompat & !INCOMPAT_KNOWN != 0 {
            return Err(SuperblockError::UnsupportedFeatures {
                bits: incompat & !INCOMPAT_KNOWN,
            });
        }
        let checksum = checksum_kind(compat, incompat);
        if checksum.is_crc32c() {
            check_crc(raw)?;
        }
        let mut uuid = [0; 16];
        uuid.copy_from_slice(&raw[0x30..0x40]);
        Ok(Self {
            block_size,
            max_len,
            first,
            sequence: be32(raw, 0x18),
            start,
            errno: be32(raw, 0x20) as i32,
            incompat,
            checksum,
            uuid,
        })
    }

    /// The journal block size in bytes.
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// The journal's length in blocks (`s_maxlen`); the log ends before this block.
    pub const fn max_len(&self) -> u32 {
        self.max_len
    }

    /// The first block of the log (`s_first`).
    pub const fn first(&self) -> u32 {
        self.first
    }

    /// The sequence number of the first transaction in the log (`s_sequence`).
    pub const fn sequence(&self) -> u32 {
        self.sequence
    }

    /// The block the log starts at (`s_start`), or 0 when the journal is clean.
    pub const fn start(&self) -> u32 {
        self.start
    }

    /// Whether the log holds transactions that may need replaying.
    pub const fn needs_recovery(&self) -> bool {
        self.start != 0
    }

    /// `s_errno`: nonzero when the journal was aborted after an error, so the file system
    /// should be checked.
    pub const fn errno(&self) -> i32 {
        self.errno
    }

    /// The journal's checksum version.
    pub const fn checksum(&self) -> ChecksumKind {
        self.checksum
    }

    /// The journal UUID (`s_uuid`), which seeds every version 2 and 3 checksum.
    pub const fn uuid(&self) -> &[u8; 16] {
        &self.uuid
    }

    /// Whether the log has revoke blocks.
    pub const fn has_revoke(&self) -> bool {
        self.incompat & INCOMPAT_REVOKE != 0
    }

    /// Whether commit blocks may reach the disk before the blocks they commit
    /// (`INCOMPAT_ASYNC_COMMIT`), so a valid commit does not prove its data is whole.
    pub const fn is_async_commit(&self) -> bool {
        self.incompat & INCOMPAT_ASYNC_COMMIT != 0
    }

    /// Whether block numbers are 64 bits wide.
    pub const fn is_64bit(&self) -> bool {
        self.incompat & INCOMPAT_64BIT != 0
    }

    /// Empties the log in `raw`, the bytes of journal block 0 that [`Superblock::parse`]
    /// read: sets `s_start` to 0 and `s_sequence` to `next_sequence` (normally
    /// [`crate::Recovery::next_sequence`]), and updates the checksum. Write it back only
    /// after every replayed block is on disk.
    ///
    /// # Errors
    ///
    /// [`SuperblockError::TooShort`] when `raw` is shorter than 1024 bytes.
    pub fn mark_recovered(
        &self,
        raw: &mut [u8],
        next_sequence: u32,
    ) -> Result<(), SuperblockError> {
        let len = raw.len();
        let raw = raw
            .get_mut(..SIZE)
            .ok_or(SuperblockError::TooShort { len })?;
        raw[0x18..0x1C].copy_from_slice(&next_sequence.to_be_bytes());
        raw[0x1C..0x20].copy_from_slice(&0u32.to_be_bytes());
        if self.checksum.is_crc32c() {
            let crc = crc32c_without(&[], raw, CHECKSUM_AT);
            raw[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&crc.to_be_bytes());
        }
        Ok(())
    }
}

fn checksum_kind(compat: u32, incompat: u32) -> ChecksumKind {
    if incompat & INCOMPAT_CSUM_V3 != 0 {
        ChecksumKind::V3
    } else if incompat & INCOMPAT_CSUM_V2 != 0 {
        ChecksumKind::V2
    } else if compat & COMPAT_CHECKSUM != 0 {
        ChecksumKind::V1
    } else {
        ChecksumKind::None
    }
}

fn check_crc(raw: &[u8]) -> Result<(), SuperblockError> {
    let kind = raw[0x50];
    if kind != CHECKSUM_TYPE_CRC32C {
        return Err(SuperblockError::UnsupportedChecksumType { found: kind });
    }
    let stored = be32(raw, CHECKSUM_AT);
    let computed = crc32c_without(&[], raw, CHECKSUM_AT);
    if stored != computed {
        return Err(SuperblockError::Checksum { stored, computed });
    }
    Ok(())
}

/// The big-endian `u32` at `at`; `raw` is long enough for every caller.
pub(crate) fn be32(raw: &[u8], at: usize) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&raw[at..at + 4]);
    u32::from_be_bytes(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A valid version 2 superblock: 1 KiB blocks, 256 blocks, log from block 1.
    pub(crate) fn raw(incompat: u32, start: u32) -> [u8; SIZE] {
        let mut raw = [0; SIZE];
        let fields = [
            (0x0, MAGIC),
            (0x4, BLOCK_TYPE_V2),
            (0xC, 1024),
            (0x10, 256),
            (0x14, 1),
            (0x18, 7),
            (0x1C, start),
            (0x28, incompat),
        ];
        for (at, value) in fields {
            raw[at..at + 4].copy_from_slice(&u32::to_be_bytes(value));
        }
        raw[0x30..0x40].copy_from_slice(&[0xAB; 16]);
        raw[0x50] = CHECKSUM_TYPE_CRC32C;
        let crc = crc32c_without(&[], &raw, CHECKSUM_AT);
        raw[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&crc.to_be_bytes());
        raw
    }

    #[test]
    fn parses_the_fields() {
        let sb = Superblock::parse(&raw(INCOMPAT_REVOKE | INCOMPAT_CSUM_V3, 5)).expect("valid");
        assert_eq!((sb.block_size(), sb.max_len(), sb.first()), (1024, 256, 1));
        assert_eq!((sb.sequence(), sb.start()), (7, 5));
        assert!(sb.needs_recovery() && sb.has_revoke() && !sb.is_64bit());
        assert_eq!(sb.checksum(), ChecksumKind::V3);
        assert_eq!(sb.uuid(), &[0xAB; 16]);
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        let short = Superblock::parse(&[0; 100]);
        assert_eq!(short, Err(SuperblockError::TooShort { len: 100 }));
        let mut bad = raw(0, 0);
        bad[0] = 0;
        assert!(matches!(
            Superblock::parse(&bad),
            Err(SuperblockError::BadMagic { .. })
        ));
        let fast_commit = raw(0x20, 0);
        assert_eq!(
            Superblock::parse(&fast_commit),
            Err(SuperblockError::UnsupportedFeatures { bits: 0x20 })
        );
        let outside = raw(0, 300);
        assert!(matches!(
            Superblock::parse(&outside),
            Err(SuperblockError::BadGeometry { start: 300, .. })
        ));
    }

    #[test]
    fn checks_the_checksum_only_when_there_is_one() {
        let mut csum = raw(INCOMPAT_CSUM_V2, 0);
        csum[0x200] ^= 1;
        assert!(matches!(
            Superblock::parse(&csum),
            Err(SuperblockError::Checksum { .. })
        ));
        let mut plain = raw(0, 0);
        plain[0x200] ^= 1;
        assert!(Superblock::parse(&plain).is_ok());
        let mut md5 = raw(INCOMPAT_CSUM_V3, 0);
        md5[0x50] = 2;
        assert_eq!(
            Superblock::parse(&md5),
            Err(SuperblockError::UnsupportedChecksumType { found: 2 })
        );
    }

    #[test]
    fn marking_recovered_empties_the_log_and_keeps_the_checksum_valid() {
        let mut block = raw(INCOMPAT_CSUM_V3, 9);
        let sb = Superblock::parse(&block).expect("valid");
        sb.mark_recovered(&mut block, 12).expect("long enough");
        let after = Superblock::parse(&block).expect("still valid");
        assert_eq!((after.start(), after.sequence()), (0, 12));
        assert!(!after.needs_recovery());
    }
}
