//! The blocks of the log: the common header, descriptor tags, revoke records and commit
//! checksums.
//!
//! From "Block Header", "Descriptor Block", "Revocation Block" and "Commit Block" in "Ext4
//! Disk Layout". All fields are big-endian. Every metadata block starts with the 12-byte
//! header `h_magic`, `h_blocktype`, `h_sequence`.
//!
//! A descriptor block lists, from byte 12, one tag for each data block that follows it in
//! the log. A tag is, with version 3 checksums, `t_blocknr`, `t_flags`, `t_blocknr_high`,
//! `t_checksum` (4 bytes each); otherwise `t_blocknr` (4), `t_checksum` (2), `t_flags` (2) and,
//! with `INCOMPAT_64BIT`, `t_blocknr_high` (4). A tag without `SAME_UUID` is followed by a
//! 16-byte UUID. With version 2 or 3 checksums the block ends in a 4-byte tail checksum.
//!
//! With version 2 checksums a tag takes two bytes more than those fields, 10 bytes (14 with
//! `INCOMPAT_64BIT`), the last two zero. The layout does not say so; it was found in
//! descriptor blocks e2fsprogs 1.47 wrote, where the next tag (or the UUID) starts two
//! bytes after `t_flags` or `t_blocknr_high`, and `e2fsck` replays them. The unit test
//! `version_2_tags_from_e2fsprogs` holds such a block's bytes.
//!
//! A revoke block has `r_count`, the bytes used including the 16-byte header, at 0xC and
//! then block numbers of 4 bytes (8 with `INCOMPAT_64BIT`), and the same tail.
//!
//! A commit block keeps its version 2 or 3 checksum in `h_chksum[0]` at 0x10.

use alloc::vec::Vec;

use crate::MAGIC;
use crate::checksum::{crc32c, crc32c_without};
use crate::superblock::{ChecksumKind, Superblock, be32};

/// `h_blocktype` of a descriptor block.
pub(crate) const DESCRIPTOR: u32 = 1;
/// `h_blocktype` of a commit block.
pub(crate) const COMMIT: u32 = 2;
/// `h_blocktype` of a revoke block.
pub(crate) const REVOKE: u32 = 5;

/// The block's first four bytes were the magic number and were zeroed in the log.
const FLAG_ESCAPE: u32 = 0x1;
/// No UUID follows this tag.
const FLAG_SAME_UUID: u32 = 0x2;
/// This is the descriptor's last tag.
const FLAG_LAST_TAG: u32 = 0x8;

/// Where a commit block keeps its checksum.
const COMMIT_CHECKSUM_AT: usize = 0x10;
/// Where revoke records start.
const REVOKE_RECORDS_AT: usize = 16;

/// A metadata block's header, when it has the magic number.
pub(crate) fn header(block: &[u8]) -> Option<(u32, u32)> {
    (be32(block, 0) == MAGIC).then(|| (be32(block, 4), be32(block, 8)))
}

/// One descriptor tag: the file-system block its data block belongs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tag {
    pub(crate) target: u64,
    pub(crate) escaped: bool,
    /// `t_checksum`, 16 bits wide (version 2) or 32 (version 3).
    pub(crate) checksum: u32,
}

/// The bytes at the end of a descriptor or revoke block that hold its checksum, if any.
fn tail_len(sb: &Superblock) -> usize {
    if sb.checksum().is_crc32c() { 4 } else { 0 }
}

/// Whether a descriptor or revoke block's tail checksum matches (or there is none).
pub(crate) fn tail_ok(block: &[u8], sb: &Superblock) -> bool {
    if !sb.checksum().is_crc32c() {
        return true;
    }
    let at = block.len() - 4;
    be32(block, at) == crc32c_without(&[sb.uuid()], block, at)
}

/// Whether a commit block's checksum matches (or there is none this crate verifies).
pub(crate) fn commit_ok(block: &[u8], sb: &Superblock) -> bool {
    if !sb.checksum().is_crc32c() {
        return true;
    }
    be32(block, COMMIT_CHECKSUM_AT) == crc32c_without(&[sb.uuid()], block, COMMIT_CHECKSUM_AT)
}

/// The tags of a descriptor block, in order. Parsing stops at `LAST_TAG` or where the next
/// tag would run into the tail.
pub(crate) fn tags(block: &[u8], sb: &Superblock) -> Vec<Tag> {
    let v3 = sb.checksum() == ChecksumKind::V3;
    let v2_padding = if sb.checksum() == ChecksumKind::V2 {
        2
    } else {
        0
    };
    let size = match (v3, sb.is_64bit()) {
        (true, _) => 16,
        (false, true) => 12 + v2_padding,
        (false, false) => 8 + v2_padding,
    };
    let end = block.len() - tail_len(sb);
    let mut tags = Vec::new();
    let mut at = 12;
    while at + size <= end {
        let (low, flags, high, checksum) = if v3 {
            (
                be32(block, at),
                be32(block, at + 4),
                be32(block, at + 8),
                be32(block, at + 12),
            )
        } else {
            let packed = be32(block, at + 4);
            let high = if sb.is_64bit() {
                be32(block, at + 8)
            } else {
                0
            };
            (be32(block, at), packed & 0xFFFF, high, packed >> 16)
        };
        let high = if sb.is_64bit() { high } else { 0 };
        tags.push(Tag {
            target: u64::from(high) << 32 | u64::from(low),
            escaped: flags & FLAG_ESCAPE != 0,
            checksum,
        });
        at += size;
        if flags & FLAG_SAME_UUID == 0 {
            at += 16;
        }
        if flags & FLAG_LAST_TAG != 0 {
            break;
        }
    }
    tags
}

/// A revoke block whose `r_count` does not fit it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BadCount(pub(crate) u32);

/// The block numbers a revoke block revokes.
pub(crate) fn revokes(block: &[u8], sb: &Superblock) -> Result<Vec<u64>, BadCount> {
    let count = be32(block, 0xC);
    let end = block.len() - tail_len(sb);
    let used = usize::try_from(count).map_err(|_| BadCount(count))?;
    if !(REVOKE_RECORDS_AT..=end).contains(&used) {
        return Err(BadCount(count));
    }
    let size = if sb.is_64bit() { 8 } else { 4 };
    let records = &block[REVOKE_RECORDS_AT..used];
    Ok(records
        .chunks_exact(size)
        .map(|record| {
            if size == 8 {
                u64::from(be32(record, 0)) << 32 | u64::from(be32(record, 4))
            } else {
                u64::from(be32(record, 0))
            }
        })
        .collect())
}

/// The checksum a tag carries for `data` in transaction `sequence`, at the tag's width.
pub(crate) fn data_checksum(sb: &Superblock, sequence: u32, data: &[u8]) -> u32 {
    let crc = crc32c(&[sb.uuid(), &sequence.to_be_bytes(), data]);
    if sb.checksum() == ChecksumKind::V3 {
        crc
    } else {
        crc & 0xFFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superblock::tests::raw;

    fn sb(incompat: u32) -> Superblock {
        Superblock::parse(&raw(incompat, 0)).expect("valid")
    }

    fn put(block: &mut [u8], at: usize, value: u32) {
        block[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }

    #[test]
    fn version_3_tags_are_16_bytes_with_a_uuid_after_the_first() {
        let sb = sb(0x10 | 0x2);
        let mut block = [0u8; 1024];
        // Tag 1: block 0x1_0000_0005, escaped, then its UUID.
        put(&mut block, 12, 5);
        put(&mut block, 16, FLAG_ESCAPE);
        put(&mut block, 20, 1);
        put(&mut block, 24, 0xDEAD_BEEF);
        // Tag 2 after the UUID: block 9, same UUID, last.
        put(&mut block, 44, 9);
        put(&mut block, 48, FLAG_SAME_UUID | FLAG_LAST_TAG);
        let tags = tags(&block, &sb);
        assert_eq!(tags.len(), 2);
        assert_eq!(
            tags[0],
            Tag {
                target: 0x1_0000_0005,
                escaped: true,
                checksum: 0xDEAD_BEEF
            }
        );
        assert_eq!((tags[1].target, tags[1].escaped), (9, false));
    }

    #[test]
    fn version_2_tags_from_e2fsprogs() {
        // The first 58 bytes of a descriptor debugfs wrote for blocks 0x5dc-0x5de on a 64bit
        // volume with version 2 checksums: 14-byte tags, the UUID (zero here) after the first.
        let mut block = [0u8; 1024];
        let prefix: [(usize, u32); 7] = [
            (12, 0x0000_05dc),
            (16, 0xb619_0000),
            (40, 0x0000_0000),
            (44, 0x05dd_b619),
            (48, 0x0002_0000),
            (56, 0x0000_05de),
            (60, 0x4c61_000a),
        ];
        for (at, value) in prefix {
            put(&mut block, at, value);
        }
        let targets: Vec<(u64, u32)> = tags(&block, &sb(0x8 | 0x2))
            .iter()
            .map(|tag| (tag.target, tag.checksum))
            .collect();
        assert_eq!(targets, [(0x5dc, 0xb619), (0x5dd, 0xb619), (0x5de, 0x4c61)]);
    }

    #[test]
    fn version_2_tags_pack_the_checksum_with_the_flags() {
        let sb = sb(0x8);
        let mut block = [0u8; 1024];
        put(&mut block, 12, 77);
        put(&mut block, 16, 0xBEEF_0000 | FLAG_SAME_UUID | FLAG_LAST_TAG);
        let tags = tags(&block, &sb);
        assert_eq!(
            tags,
            [Tag {
                target: 77,
                escaped: false,
                checksum: 0xBEEF
            }]
        );
    }

    #[test]
    fn tags_stop_before_the_tail() {
        let sb = sb(0x10);
        // No LAST_TAG: every tag has SAME_UUID, so (1024 - 12 - 4) / 16 = 63 fit, and the
        // bytes after them (the tail) are not read as a 64th.
        let mut block = [0u8; 1024];
        for i in 0..63 {
            put(&mut block, 12 + i * 16 + 4, FLAG_SAME_UUID);
        }
        block[1020..].fill(0xFF);
        assert_eq!(tags(&block[..], &sb).len(), 63);
    }

    #[test]
    fn revoke_records_follow_the_count() {
        let wide = sb(0x2 | 0x1);
        let mut block = [0u8; 1024];
        put(&mut block, 0xC, 16 + 16);
        put(&mut block, 16, 1);
        put(&mut block, 20, 2);
        put(&mut block, 28, 3);
        assert_eq!(revokes(&block, &wide), Ok(alloc::vec![0x1_0000_0002, 3]));
        let narrow = sb(0x1);
        assert_eq!(revokes(&block, &narrow), Ok(alloc::vec![1, 2, 0, 3]));
        put(&mut block, 0xC, 2000);
        assert_eq!(revokes(&block, &narrow), Err(BadCount(2000)));
    }

    #[test]
    fn tails_and_commits_are_checked_with_version_2_and_3_only() {
        let sb3 = sb(0x10);
        let mut block = [0u8; 1024];
        assert!(!tail_ok(&block, &sb3));
        let crc = crc32c_without(&[sb3.uuid()], &block, 1020);
        put(&mut block, 1020, crc);
        assert!(tail_ok(&block, &sb3));
        let crc = crc32c_without(&[sb3.uuid()], &block, COMMIT_CHECKSUM_AT);
        put(&mut block, COMMIT_CHECKSUM_AT, crc);
        assert!(commit_ok(&block, &sb3));
        let none = sb(0);
        assert!(tail_ok(&[0; 1024], &none) && commit_ok(&[0; 1024], &none));
    }
}
