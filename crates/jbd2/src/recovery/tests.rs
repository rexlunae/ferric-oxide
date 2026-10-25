//! Recovery on journals built in memory, one rule at a time. `tests/fixtures.rs` covers
//! journals written by e2fsprogs.

use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use super::*;
use crate::checksum::crc32c_without;
use crate::superblock::tests::raw;

const BS: usize = 1024;
const LEN: u32 = 32;
const REVOKE_V3_64: u32 = 0x1 | 0x2 | 0x10;

/// A 32-block journal with 1 KiB blocks and version 3 checksums, written transaction by
/// transaction.
struct Builder {
    bytes: Vec<u8>,
    sb: Superblock,
    at: u32,
    sequence: u32,
}

impl JournalRead for Builder {
    type Error = Infallible;

    fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), Infallible> {
        let start = block as usize * BS;
        buf.copy_from_slice(&self.bytes[start..start + BS]);
        Ok(())
    }
}

fn put(block: &mut [u8], at: usize, value: u32) {
    block[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

impl Builder {
    /// A journal whose log starts at `start` with sequence `sequence`.
    fn new(start: u32, sequence: u32) -> Self {
        Self::with_features(start, sequence, REVOKE_V3_64)
    }

    /// As [`Builder::new`], with `incompat` as the journal's incompatible features.
    fn with_features(start: u32, sequence: u32, incompat: u32) -> Self {
        let mut sb_raw = raw(incompat, start);
        put(&mut sb_raw, 0x10, LEN);
        put(&mut sb_raw, 0x18, sequence);
        let crc = crc32c_without(&[], &sb_raw, 0xFC);
        put(&mut sb_raw, 0xFC, crc);
        let sb = Superblock::parse(&sb_raw).expect("valid");
        let mut bytes = vec![0; LEN as usize * BS];
        bytes[..BS].copy_from_slice(&sb_raw);
        Self {
            bytes,
            sb,
            at: start,
            sequence,
        }
    }

    fn block(&mut self) -> &mut [u8] {
        let start = self.at as usize * BS;
        &mut self.bytes[start..start + BS]
    }

    fn next(&mut self) {
        self.at += 1;
        if self.at == LEN {
            self.at = self.sb.first();
        }
    }

    fn header(&mut self, kind: u32) {
        let sequence = self.sequence;
        let block = self.block();
        block.fill(0);
        put(block, 0, MAGIC);
        put(block, 4, kind);
        put(block, 8, sequence);
    }

    fn seal_tail(&mut self) {
        let uuid = *self.sb.uuid();
        let block = self.block();
        let crc = crc32c_without(&[&uuid], block, BS - 4);
        put(block, BS - 4, crc);
    }

    /// A descriptor and its data blocks: `(target, first data byte)` each, all filled with
    /// that byte. A data block filled with the magic number's first byte, 0xC0, starts with
    /// the magic number and is escaped.
    fn data(&mut self, blocks: &[(u64, u8)]) {
        self.header(DESCRIPTOR);
        let mut contents = Vec::new();
        for (i, &(target, fill)) in blocks.iter().enumerate() {
            let mut data = vec![fill; BS];
            let escaped = fill == 0xC0;
            if escaped {
                data[..4].copy_from_slice(&MAGIC.to_be_bytes());
            }
            let mut stored = data.clone();
            if escaped {
                stored[..4].fill(0);
            }
            let checksum = log::data_checksum(&self.sb, self.sequence, &stored);
            let mut flags = if i == 0 { 0 } else { 0x2 };
            if escaped {
                flags |= 0x1;
            }
            if i == blocks.len() - 1 {
                flags |= 0x8;
            }
            let at = 12 + i * 16 + if i == 0 { 0 } else { 16 };
            let block = self.block();
            put(block, at, target as u32);
            put(block, at + 4, flags);
            put(block, at + 8, (target >> 32) as u32);
            put(block, at + 12, checksum);
            contents.push(stored);
        }
        self.seal_tail();
        for stored in contents {
            self.next();
            self.block().copy_from_slice(&stored);
        }
        self.next();
    }

    fn revoke(&mut self, blocks: &[u64]) {
        self.header(REVOKE);
        let block = self.block();
        put(block, 0xC, 16 + 8 * blocks.len() as u32);
        for (i, &b) in blocks.iter().enumerate() {
            put(block, 16 + 8 * i, (b >> 32) as u32);
            put(block, 20 + 8 * i, b as u32);
        }
        self.seal_tail();
        self.next();
    }

    fn commit(&mut self) {
        self.header(COMMIT);
        let uuid = *self.sb.uuid();
        let block = self.block();
        let crc = crc32c_without(&[&uuid], block, 0x10);
        put(block, 0x10, crc);
        self.next();
        self.sequence = self.sequence.wrapping_add(1);
    }

    fn scan(&self) -> Result<Recovery, RecoveryError<Infallible>> {
        Recovery::scan(self, &self.sb)
    }
}

/// The plan as `(target, fill byte of the replayed contents)`.
fn replayed(journal: &Builder, recovery: &Recovery) -> Vec<(u64, u8)> {
    let mut buf = vec![0; BS];
    recovery
        .writes()
        .iter()
        .map(|write| {
            recovery
                .read(journal, &journal.sb, write, &mut buf)
                .expect("valid");
            (write.target, buf[BS - 1])
        })
        .collect()
}

#[test]
fn the_newest_copy_of_each_block_wins() {
    let mut j = Builder::new(1, 10);
    j.data(&[(100, 1), (101, 1)]);
    j.commit();
    j.data(&[(101, 2), (102, 2)]);
    j.commit();
    let recovery = j.scan().expect("valid");
    assert_eq!(recovery.transactions(), 2);
    assert_eq!(replayed(&j, &recovery), [(100, 1), (101, 2), (102, 2)]);
    assert_eq!(recovery.next_sequence(), 13);
}

#[test]
fn a_revoke_cancels_older_and_same_transaction_copies_only() {
    let mut j = Builder::new(1, 1);
    j.data(&[(100, 1), (101, 1), (102, 1)]);
    j.commit();
    // Transaction 2 logs 102 and revokes it: both earlier copies are cancelled.
    j.data(&[(102, 2)]);
    j.revoke(&[101, 102]);
    j.commit();
    // Transaction 3 logs 101 again after the revoke, so that copy counts.
    j.data(&[(101, 3)]);
    j.commit();
    let recovery = j.scan().expect("valid");
    assert_eq!(replayed(&j, &recovery), [(100, 1), (101, 3)]);
}

#[test]
fn an_uncommitted_transaction_is_not_replayed() {
    let mut j = Builder::new(1, 5);
    j.data(&[(100, 1)]);
    j.commit();
    j.data(&[(100, 2), (200, 2)]);
    let recovery = j.scan().expect("valid");
    assert_eq!(replayed(&j, &recovery), [(100, 1)]);
    // Sequence 6 had no commit, so the next one is 7.
    assert_eq!(recovery.next_sequence(), 7);
}

#[test]
fn a_torn_commit_ends_the_log() {
    let mut j = Builder::new(1, 1);
    j.data(&[(100, 1)]);
    j.commit();
    j.data(&[(100, 2)]);
    let commit = j.at;
    j.commit();
    j.bytes[commit as usize * BS + 0x40] ^= 1;
    let recovery = j.scan().expect("valid");
    assert_eq!(replayed(&j, &recovery), [(100, 1)]);
    assert_eq!(recovery.transactions(), 1);
}

#[test]
fn a_torn_descriptor_ends_the_log() {
    let mut j = Builder::new(1, 1);
    let descriptor = j.at;
    j.data(&[(100, 1)]);
    j.commit();
    j.bytes[descriptor as usize * BS + 100] ^= 1;
    let recovery = j.scan().expect("valid");
    assert!(recovery.writes().is_empty());
    assert_eq!(recovery.next_sequence(), 2);
}

#[test]
fn the_log_wraps_to_its_first_block() {
    // Start four blocks before the end: the second transaction's data wraps round.
    let mut j = Builder::new(LEN - 4, 40);
    j.data(&[(7, 1)]);
    j.commit();
    j.data(&[(8, 2), (9, 2)]);
    j.commit();
    assert_eq!(j.at, 4, "the log wrapped to block 1");
    let recovery = j.scan().expect("valid");
    assert_eq!(replayed(&j, &recovery), [(7, 1), (8, 2), (9, 2)]);
    assert_eq!(recovery.writes()[1].source, 1);
}

#[test]
fn sequence_numbers_wrap() {
    let mut j = Builder::new(1, u32::MAX);
    j.data(&[(100, 1)]);
    j.revoke(&[100]);
    j.commit();
    // Transaction 0 comes after u32::MAX, so this copy outlives the revoke.
    j.data(&[(100, 2)]);
    j.commit();
    let recovery = j.scan().expect("valid");
    assert_eq!(replayed(&j, &recovery), [(100, 2)]);
    assert_eq!(recovery.next_sequence(), 2);
}

#[test]
fn escaped_blocks_get_their_magic_back() {
    let mut j = Builder::new(1, 1);
    j.data(&[(100, 0xC0)]);
    j.commit();
    let recovery = j.scan().expect("valid");
    let mut buf = vec![0; BS];
    recovery
        .read(&j, &j.sb, &recovery.writes()[0], &mut buf)
        .expect("valid");
    assert_eq!(buf[..4], MAGIC.to_be_bytes());
    assert!(buf[4..].iter().all(|&b| b == 0xC0));
}

#[test]
fn an_escaped_block_checksummed_with_its_magic_is_accepted_too() {
    let mut j = Builder::new(1, 1);
    let descriptor = j.at;
    j.data(&[(100, 0xC0)]);
    j.commit();
    // Rewrite the tag's checksum over the block as it will be written.
    let mut restored = vec![0xC0; BS];
    restored[..4].copy_from_slice(&MAGIC.to_be_bytes());
    let checksum = log::data_checksum(&j.sb, 1, &restored);
    let d = descriptor as usize * BS;
    put(&mut j.bytes[d..d + BS], 24, checksum);
    j.at = descriptor;
    j.seal_tail();
    assert_eq!(j.scan().expect("valid").writes().len(), 1);
}

#[test]
fn a_corrupt_data_block_in_a_committed_transaction_is_an_error() {
    let mut j = Builder::new(1, 3);
    j.data(&[(100, 1)]);
    j.commit();
    j.bytes[2 * BS + 9] ^= 1;
    let err = j.scan().expect_err("bad data");
    assert!(matches!(
        err,
        RecoveryError::DataChecksum {
            source_block: 2,
            target: 100,
            sequence: 3
        }
    ));
}

#[test]
fn a_log_that_overruns_itself_is_refused() {
    let mut j = Builder::new(1, 1);
    let tags: Vec<(u64, u8)> = (0..40).map(|i| (i, 1)).collect();
    // 40 tags with their UUID fit in a descriptor but not in a 31-block log. Write the
    // descriptor alone: the data would wrap over it.
    j.header(DESCRIPTOR);
    for (i, &(target, _)) in tags.iter().enumerate() {
        let at = 12 + i * 16 + if i == 0 { 0 } else { 16 };
        let block = j.block();
        put(block, at, target as u32);
        put(block, at + 4, if i == 0 { 0 } else { 0x2 });
    }
    j.seal_tail();
    assert!(matches!(j.scan(), Err(RecoveryError::LogOverrun)));
}

#[test]
fn a_clean_journal_plans_nothing() {
    let j = Builder::new(0, 9);
    let recovery = j.scan().expect("valid");
    assert!(recovery.writes().is_empty());
    assert_eq!(recovery.transactions(), 0);
    assert_eq!(
        recovery.next_sequence(),
        9,
        "a clean journal keeps its sequence"
    );
}

#[test]
fn reads_need_a_whole_block() {
    let mut j = Builder::new(1, 1);
    j.data(&[(100, 1)]);
    j.commit();
    let recovery = j.scan().expect("valid");
    let mut short = [0; 10];
    let err = recovery
        .read(&j, &j.sb, &recovery.writes()[0], &mut short)
        .expect_err("short");
    assert!(matches!(
        err,
        RecoveryError::BufferSize {
            expected: 1024,
            found: 10
        }
    ));
}

/// `INCOMPAT_ASYNC_COMMIT` on top of the usual features.
const ASYNC: u32 = REVOKE_V3_64 | 0x4;

/// Flips a bit in the data block at journal block `block`.
fn damage(j: &mut Builder, block: u32) {
    j.bytes[block as usize * BS + 300] ^= 1;
}

#[test]
fn an_async_commit_over_damaged_data_ends_the_log() {
    let mut j = Builder::with_features(1, 10, ASYNC);
    j.data(&[(100, 1)]);
    j.commit();
    // Transaction 11's commit reached the disk, but its data (block 5) did not.
    j.data(&[(200, 2), (201, 2)]);
    j.commit();
    j.data(&[(300, 3)]);
    j.commit();
    damage(&mut j, 5);
    let recovery = j.scan().expect("transaction 10 is replayed");
    assert_eq!(replayed(&j, &recovery), [(100, 1)]);
    assert_eq!(recovery.transactions(), 1);
    // Sequence 11 had no (whole) commit.
    assert_eq!(recovery.next_sequence(), 12);
}

#[test]
fn without_async_commits_damaged_data_under_a_commit_is_an_error() {
    let mut j = Builder::new(1, 10);
    j.data(&[(100, 1)]);
    j.commit();
    j.data(&[(200, 2)]);
    j.commit();
    damage(&mut j, 5);
    assert!(matches!(
        j.scan(),
        Err(RecoveryError::DataChecksum {
            source_block: 5,
            target: 200,
            sequence: 11
        })
    ));
}

#[test]
fn a_damaged_copy_that_is_never_written_does_not_stop_the_replay() {
    let mut j = Builder::new(1, 1);
    // Blocks 2 and 3 hold transaction 1's copies of 100 and 101.
    j.data(&[(100, 1), (101, 1)]);
    j.commit();
    // 100 gets a newer copy and 101 is revoked: the damaged old copies are not written.
    j.data(&[(100, 2)]);
    j.revoke(&[101]);
    j.commit();
    damage(&mut j, 2);
    damage(&mut j, 3);
    let recovery = j.scan().expect("only intact copies are written");
    assert_eq!(replayed(&j, &recovery), [(100, 2)]);
}
