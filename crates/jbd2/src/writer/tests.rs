//! Transactions written into journals in memory and read back with [`crate::Recovery`].
//! ext4d's `tests/write.rs` has `e2fsck` replay and check what the writer writes.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use super::*;
use crate::Recovery;
use crate::checksum::crc32c_without;
use crate::superblock::tests::raw;

const BS: usize = 1024;
/// The journal length [`raw`] gives.
const LEN: usize = 256;
const V2: u32 = 0x8;
const V3: u32 = 0x10;
const WIDE: u32 = 0x2;
const REVOKE: u32 = 0x1;

/// A device that fails once `writes_left` writes have been made.
#[derive(Debug)]
struct Crashed;

impl fmt::Display for Crashed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the device stopped")
    }
}

impl core::error::Error for Crashed {}

/// A journal of [`LEN`] blocks and a file system of 64 blocks, in memory.
struct Memory {
    journal: Vec<u8>,
    home: Vec<u8>,
    writes_left: usize,
    flushes: usize,
}

impl Memory {
    fn new(incompat: u32) -> Self {
        let mut journal = vec![0; LEN * BS];
        journal[..BS].copy_from_slice(&raw(incompat, 0));
        Self {
            journal,
            home: vec![0; 64 * BS],
            writes_left: usize::MAX,
            flushes: 0,
        }
    }

    fn sb(&self) -> Superblock {
        Superblock::parse(&self.journal[..BS]).expect("a valid superblock")
    }

    fn writer(&self) -> Writer {
        Writer::new(&self.journal[..BS]).expect("an empty journal")
    }

    fn spend(&mut self) -> Result<(), Crashed> {
        self.writes_left = self.writes_left.checked_sub(1).ok_or(Crashed)?;
        Ok(())
    }

    /// What a replay would write home now.
    fn replay(&self) -> Vec<(u64, Vec<u8>)> {
        let sb = self.sb();
        if !sb.needs_recovery() {
            return Vec::new();
        }
        let recovery = Recovery::scan(self, &sb).expect("a readable log");
        let mut buf = vec![0; BS];
        recovery
            .writes()
            .iter()
            .map(|write| {
                recovery.read(self, &sb, write, &mut buf).expect("intact");
                (write.target, buf.clone())
            })
            .collect()
    }
}

impl JournalRead for Memory {
    type Error = Crashed;

    fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), Crashed> {
        let at = block as usize * BS;
        buf.copy_from_slice(&self.journal[at..at + BS]);
        Ok(())
    }
}

impl JournalWrite for Memory {
    fn write_block(&mut self, block: u32, buf: &[u8]) -> Result<(), Crashed> {
        self.spend()?;
        let at = block as usize * BS;
        self.journal[at..at + BS].copy_from_slice(buf);
        Ok(())
    }

    fn write_home(&mut self, target: u64, buf: &[u8]) -> Result<(), Crashed> {
        self.spend()?;
        let at = target as usize * BS;
        self.home[at..at + BS].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Crashed> {
        self.flushes += 1;
        Ok(())
    }
}

/// A block of `byte`s.
fn block(byte: u8) -> Vec<u8> {
    vec![byte; BS]
}

/// A block that starts with the magic number, as a file-system block may.
fn magic_block() -> Vec<u8> {
    let mut data = block(0x5A);
    data[..4].copy_from_slice(&MAGIC.to_be_bytes());
    data
}

fn transaction(writer: &Writer, blocks: &[(u64, Vec<u8>)]) -> Transaction {
    let mut transaction = writer.transaction();
    for (target, data) in blocks {
        transaction.log(*target, data).expect("a block");
    }
    transaction
}

const TIME: CommitTime = CommitTime {
    seconds: 1_767_225_600,
    nanoseconds: 5,
};

#[test]
fn a_committed_transaction_replays_with_every_checksum_kind() {
    for incompat in [0, REVOKE, V2, V2 | WIDE, V3, V3 | WIDE | REVOKE] {
        let mut memory = Memory::new(incompat);
        let mut writer = memory.writer();
        let blocks = vec![(3, block(0x11)), (9, magic_block()), (40, block(0x22))];
        let committed = writer
            .commit(&mut memory, transaction(&writer, &blocks), TIME)
            .expect("committed");
        assert_eq!((committed.sequence(), committed.log_blocks()), (7, 5));
        let sb = memory.sb();
        assert_eq!((sb.start(), sb.sequence()), (1, 7), "{incompat:#x}");
        let recovery = Recovery::scan(&memory, &sb).expect("scanned");
        assert_eq!(recovery.transactions(), 1, "{incompat:#x}");
        // One past the first sequence number with no commit, as after e2fsck's replay.
        assert_eq!(recovery.next_sequence(), 9, "{incompat:#x}");
        assert_eq!(memory.replay(), blocks, "{incompat:#x}");
    }
}

#[test]
fn the_log_has_a_descriptor_the_copies_and_a_commit() {
    let mut memory = Memory::new(V3 | WIDE);
    let mut writer = memory.writer();
    let blocks = vec![(0x1_0000_0003, block(0x11)), (9, magic_block())];
    let committed = writer.commit(&mut memory, transaction(&writer, &blocks), TIME);
    assert!(committed.is_ok());
    let sb = memory.sb();
    let log = |n: usize| &memory.journal[n * BS..(n + 1) * BS];
    assert_eq!(log::header(log(1)), Some((DESCRIPTOR, 7)));
    let tags = log::tags(log(1), &sb);
    assert_eq!(
        tags.iter()
            .map(|t| (t.target, t.escaped))
            .collect::<Vec<_>>(),
        [(9, true), (0x1_0000_0003, false)]
    );
    // The UUID follows the first tag only.
    assert_eq!(&log(1)[28..44], sb.uuid());
    assert!(log::tail_ok(log(1), &sb));
    // The escaped copy is stored with its magic number zeroed, and its checksum covers it so.
    assert_eq!(&log(2)[..4], &[0; 4]);
    assert_eq!(tags[0].checksum, log::data_checksum(&sb, 7, log(2)));
    assert_eq!(log(3), &block(0x11)[..]);
    assert_eq!(log::header(log(4)), Some((COMMIT, 7)));
    assert!(log::commit_ok(log(4), &sb));
    assert_eq!(
        &log(4)[0x30..0x3C],
        &[0, 0, 0, 0, 0x69, 0x55, 0xB9, 0, 0, 0, 0, 5]
    );
}

#[test]
fn a_large_transaction_takes_several_descriptors() {
    for incompat in [0, V2, V3 | WIDE] {
        let mut memory = Memory::new(incompat);
        let mut writer = memory.writer();
        let per = writer.tags_per_descriptor();
        // One descriptor full, a second with five tags.
        let blocks: Vec<(u64, Vec<u8>)> = (0..per as u64 + 5)
            .map(|n| (1000 + n, block(n as u8)))
            .collect();
        let tx = transaction(&writer, &blocks);
        assert_eq!(writer.log_blocks(&tx) as usize, blocks.len() + 2 + 1);
        let committed = writer.commit(&mut memory, tx, TIME).expect("fits");
        assert_eq!(committed.log_blocks() as usize, blocks.len() + 3);
        assert_eq!(memory.replay(), blocks, "{incompat:#x}");
    }
}

#[test]
fn a_descriptor_is_filled_to_the_tail() {
    // 1 KiB blocks: (1024 - 12 - 4 - 16) / 16 with version 3, (1024 - 12 - 16) / 8 without
    // checksums, (1024 - 12 - 4 - 16) / 14 with version 2 and 64-bit tags.
    for (incompat, per) in [(V3, 62), (0, 124), (V2 | WIDE, 70)] {
        let memory = Memory::new(incompat);
        assert_eq!(memory.writer().tags_per_descriptor(), per, "{incompat:#x}");
    }
}

#[test]
fn checkpointing_writes_home_and_empties_the_log() {
    let mut memory = Memory::new(V3);
    let mut writer = memory.writer();
    let first = vec![(3, block(0x11)), (4, block(0x12)), (5, magic_block())];
    let committed = writer
        .commit(&mut memory, transaction(&writer, &first), TIME)
        .expect("committed");
    writer
        .checkpoint(&mut memory, committed)
        .expect("checkpointed");
    for (target, data) in &first {
        let at = *target as usize * BS;
        assert_eq!(&memory.home[at..at + BS], &data[..]);
    }
    let sb = memory.sb();
    assert!(!sb.needs_recovery());
    assert_eq!(sb.sequence(), 8);
    assert_eq!(writer.superblock(), &sb);
    // The next transaction is logged over the first, shorter than it: what is left of the
    // first after its commit block is not replayed.
    let second = vec![(4, block(0x33))];
    let committed = writer
        .commit(&mut memory, transaction(&writer, &second), TIME)
        .expect("committed");
    assert_eq!(committed.sequence(), 8);
    assert_eq!(memory.replay(), second);
    writer
        .checkpoint(&mut memory, committed)
        .expect("checkpointed");
    assert_eq!(memory.sb().sequence(), 9);
    assert_eq!(&memory.home[4 * BS..5 * BS], &block(0x33)[..]);
}

#[test]
fn every_step_is_flushed() {
    let mut memory = Memory::new(V3);
    let mut writer = memory.writer();
    let committed = writer.commit(&mut memory, transaction(&writer, &[(3, block(1))]), TIME);
    // The log, the commit block, the superblock.
    assert_eq!(memory.flushes, 3);
    writer
        .checkpoint(&mut memory, committed.expect("committed"))
        .expect("checkpointed");
    // The blocks home, the superblock.
    assert_eq!(memory.flushes, 5);
}

/// A crash after any number of writes leaves the transaction lost or replayable, whole.
#[test]
fn a_crash_at_any_write_leaves_all_or_nothing() {
    let blocks = vec![(3, block(0x11)), (9, magic_block()), (40, block(0x22))];
    // Five log blocks and the superblock, three blocks home and the superblock.
    for writes_left in 0..10 {
        let mut memory = Memory::new(V3 | WIDE);
        memory.writes_left = writes_left;
        let mut writer = memory.writer();
        let done = writer
            .commit(&mut memory, transaction(&writer, &blocks), TIME)
            .and_then(|committed| writer.checkpoint(&mut memory, committed));
        assert!(done.is_err(), "{writes_left}");
        let replay = memory.replay();
        if writes_left < 6 {
            assert!(replay.is_empty(), "{writes_left}: lost whole");
            assert!(memory.home.iter().all(|&b| b == 0), "{writes_left}");
        } else {
            assert_eq!(replay, blocks, "{writes_left}: replayed whole");
        }
    }
}

#[test]
fn a_journal_to_replay_or_check_first_is_refused() {
    assert_eq!(
        Writer::new(&raw(V3, 1)).map(|_| ()),
        Err(OpenError::NotEmpty)
    );
    let mut aborted = raw(V3, 0);
    aborted[0x20..0x24].copy_from_slice(&5u32.to_be_bytes());
    let crc = crc32c_without(&[], &aborted, 0xFC);
    aborted[0xFC..0x100].copy_from_slice(&crc.to_be_bytes());
    assert_eq!(
        Writer::new(&aborted).map(|_| ()),
        Err(OpenError::Aborted { errno: 5 })
    );
    // COMPAT_CHECKSUM: version 1 checksums.
    let mut v1 = raw(0, 0);
    v1[0x24..0x28].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(
        Writer::new(&v1).map(|_| ()),
        Err(OpenError::UnsupportedChecksum)
    );
    let mut longer = raw(V3, 0).to_vec();
    longer.push(0);
    assert_eq!(
        Writer::new(&longer).map(|_| ()),
        Err(OpenError::BlockSize {
            expected: BS,
            found: BS + 1
        })
    );
}

#[test]
fn a_transaction_holds_whole_blocks_that_fit_the_tags() {
    let narrow = Memory::new(V3).writer();
    let mut transaction = narrow.transaction();
    assert_eq!(
        transaction.log(3, &[0; 10]),
        Err(LogError::BlockSize {
            expected: BS,
            found: 10
        })
    );
    assert_eq!(
        transaction.log(1 << 32, &block(0)),
        Err(LogError::TargetTooWide { target: 1 << 32 })
    );
    assert!(transaction.is_empty());
    transaction.log(3, &block(1)).expect("a block");
    transaction.log(3, &block(2)).expect("a block");
    assert_eq!(transaction.len(), 1);
    assert_eq!(transaction.blocks().next(), Some((3, &block(2)[..])));
    let wide = Memory::new(V3 | WIDE).writer();
    assert!(wide.transaction().log(1 << 32, &block(0)).is_ok());
}

#[test]
fn what_cannot_be_committed_is_refused_before_any_write() {
    let mut memory = Memory::new(V3);
    let mut writer = memory.writer();
    let empty = writer.transaction();
    assert!(matches!(
        writer.commit(&mut memory, empty, TIME),
        Err(WriteError::Empty)
    ));
    let other = Memory::new(V3 | WIDE).writer();
    let foreign = transaction(&other, &[(3, block(1))]);
    assert!(matches!(
        writer.commit(&mut memory, foreign, TIME),
        Err(WriteError::WrongJournal)
    ));
    // 255 log blocks: 250 copies need 5 descriptors and a commit.
    let blocks: Vec<(u64, Vec<u8>)> = (0..250).map(|n| (n, block(1))).collect();
    assert!(matches!(
        writer.commit(&mut memory, transaction(&writer, &blocks), TIME),
        Err(WriteError::TooLarge {
            needed: 256,
            room: 255
        })
    ));
    assert_eq!(memory.writes_left, usize::MAX, "nothing was written");
    // A transaction left in the log keeps the next one out.
    let committed = writer
        .commit(&mut memory, transaction(&writer, &[(3, block(1))]), TIME)
        .expect("committed");
    assert!(matches!(
        writer.commit(&mut memory, transaction(&writer, &[(4, block(1))]), TIME),
        Err(WriteError::NotEmpty)
    ));
    writer
        .checkpoint(&mut memory, committed)
        .expect("checkpointed");
}
