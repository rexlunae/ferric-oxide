//! Writing transactions into an empty journal, and checkpointing them.
//!
//! A [`Writer`] holds the journal superblock. [`Writer::commit`] writes one [`Transaction`]
//! into the log and [`Writer::checkpoint`] writes its blocks home and empties the log again,
//! in this order, with a flush after each step:
//!
//! 1. The log, from `s_first`: for each run of blocks that fits one descriptor block, the
//!    descriptor and then the copies of the blocks it lists. A copy whose first four bytes
//!    are the magic number is logged with them zeroed and its tag marked `ESCAPE`.
//! 2. The commit block, with the transaction's sequence number (`s_sequence`).
//! 3. The journal superblock, with `s_start` set to `s_first`: from here on a replay finds
//!    the transaction.
//! 4. ([`Writer::checkpoint`]) Each block at its home location on the file system.
//! 5. The journal superblock, emptied: `s_start` 0 and `s_sequence` one past the
//!    transaction, as [`Superblock::mark_recovered`] leaves it after a replay.
//!
//! A crash before step 3 is on disk loses the transaction as a whole; the log blocks that
//! were written are not reachable from an empty superblock. A crash after it leaves a
//! journal that [`crate::Recovery`] (or `e2fsck`) replays, writing the same blocks
//! [`Writer::checkpoint`] would. Either way the file system sees all of the transaction or
//! none of it. Only one transaction is in the log at a time, and the log always starts at
//! `s_first`, so it never wraps.
//!
//! A [`Writer`] tracks where it is: empty, holding one committed transaction, or poisoned.
//! Once a commit or checkpoint has started writing, any device error poisons it. The
//! journal on disk may then hold a transaction the writer cannot account for, so it refuses
//! every later call with [`WriteError::Poisoned`] until the journal is replayed and a new
//! writer made. [`Writer::checkpoint`] takes only the [`Committed`] token of the
//! transaction that writer has pending.
//!
//! The descriptor, commit and superblock checksums (with version 2 or 3 checksums) and the
//! tag checksums are written as the `log` module reads them, and a tag's checksum covers the
//! copy as logged (with an escaped magic number zeroed), which is what e2fsprogs 1.47's
//! `debugfs` writes. Journals with version 1 checksums (`COMPAT_CHECKSUM`), which this crate
//! does not verify, are not written to.
//!
//! # Specification
//!
//! Written from the "Journal (jbd2)" section of "Ext4 Disk Layout" on the ext4 wiki at
//! kernel.org ("Block Header", "Super Block", "Descriptor Block", "Commit Block"), with the
//! details the layout leaves open taken from the same e2fsprogs 1.47 experiments as the rest
//! of the crate (see the crate docs) and checked by replaying what this module writes with
//! `e2fsck`. No code here comes from the Linux kernel or from e2fsprogs.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use thiserror::Error;

use crate::checksum::crc32c_without;
use crate::log::{
    self, COMMIT, COMMIT_CHECKSUM_AT, DESCRIPTOR, FLAG_LAST_TAG, FLAG_SAME_UUID, TAGS_AT, Tag,
    UUID_LEN, put32,
};
use crate::{ChecksumKind, JournalRead, MAGIC, Superblock, SuperblockError};

/// Where a commit block keeps `h_commit_sec`.
const COMMIT_SEC_AT: usize = 0x30;
/// Where a commit block keeps `h_commit_nsec`.
const COMMIT_NSEC_AT: usize = 0x38;

/// Writes blocks of a journal and of the file system it protects.
///
/// Journal block numbers are as in [`JournalRead`]; file-system ("home") block numbers are
/// those in descriptor tags. For an ext4 journal inode both live on the same device.
pub trait JournalWrite: JournalRead {
    /// Writes `buf`, one journal block long, to journal block `block`.
    ///
    /// # Errors
    ///
    /// Whatever the underlying device reports.
    fn write_block(&mut self, block: u32, buf: &[u8]) -> Result<(), Self::Error>;

    /// Writes `buf`, one block long, to file-system block `target`.
    ///
    /// # Errors
    ///
    /// Whatever the underlying device reports.
    fn write_home(&mut self, target: u64, buf: &[u8]) -> Result<(), Self::Error>;

    /// Returns once every block written so far is on stable storage.
    ///
    /// # Errors
    ///
    /// Whatever the underlying device reports.
    fn flush(&mut self) -> Result<(), Self::Error>;
}

impl<J: JournalWrite + ?Sized> JournalWrite for &mut J {
    fn write_block(&mut self, block: u32, buf: &[u8]) -> Result<(), Self::Error> {
        (**self).write_block(block, buf)
    }

    fn write_home(&mut self, target: u64, buf: &[u8]) -> Result<(), Self::Error> {
        (**self).write_home(target, buf)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        (**self).flush()
    }
}

/// Why a block could not be added to a [`Transaction`].
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogError {
    /// The block is not one journal block long.
    #[error("a journal block is {expected} bytes, the block given is {found}")]
    BlockSize {
        /// The journal block size.
        expected: usize,
        /// The length given.
        found: usize,
    },
    /// The block number needs more than 32 bits and the journal lacks `INCOMPAT_64BIT`.
    #[error("block {target} does not fit a 32-bit journal tag")]
    TargetTooWide {
        /// The block number.
        target: u64,
    },
}

/// Why a [`Writer`] could not be made for a journal.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenError {
    /// The journal superblock was refused.
    #[error("bad journal superblock")]
    Superblock(#[from] SuperblockError),
    /// Journal block 0 was given with a length other than the journal's block size.
    #[error("the journal's block size is {expected}, block 0 was given as {found} bytes")]
    BlockSize {
        /// `s_blocksize`.
        expected: usize,
        /// The length given.
        found: usize,
    },
    /// The journal still holds a log: it must be replayed first.
    #[error("the journal is not empty")]
    NotEmpty,
    /// The journal was aborted after an error (`s_errno`); the file system needs checking.
    #[error("the journal was aborted with error {errno}")]
    Aborted {
        /// `s_errno`.
        errno: i32,
    },
    /// The journal uses version 1 checksums, which this crate does not write.
    #[error("the journal's version 1 checksums are not supported for writing")]
    UnsupportedChecksum,
}

/// Why a transaction could not be committed or checkpointed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WriteError<E: core::error::Error + 'static> {
    /// The journal superblock could not be updated (block 0 is too short).
    #[error("cannot update the journal superblock")]
    Superblock(#[from] SuperblockError),
    /// The journal still holds an earlier transaction that was not checkpointed.
    #[error("the journal is not empty")]
    NotEmpty,
    /// An earlier commit or checkpoint failed on the device, so the journal may hold a
    /// transaction this writer cannot account for. Replay the journal and make a new
    /// [`Writer`].
    #[error("an earlier device error left the journal to be replayed")]
    Poisoned,
    /// [`Writer::checkpoint`] was given a [`Committed`] that this writer did not commit, or
    /// that it has already checkpointed.
    #[error("the committed transaction does not belong to this writer")]
    ForeignCommit,
    /// The transaction has no blocks.
    #[error("the transaction is empty")]
    Empty,
    /// The transaction was made for another journal's block size or tag width.
    #[error("the transaction was made for another journal")]
    WrongJournal,
    /// The transaction needs more log blocks than the journal has.
    #[error("the transaction needs {needed} log blocks, the journal has {room}")]
    TooLarge {
        /// Descriptor, data and commit blocks the transaction takes.
        needed: u32,
        /// Log blocks in the journal (`s_maxlen - s_first`).
        room: u32,
    },
    /// Writing a journal block failed.
    #[error("cannot write journal block {block}")]
    Write {
        /// The journal block.
        block: u32,
        /// The device's error.
        #[source]
        source: E,
    },
    /// Writing a block home failed.
    #[error("cannot write file-system block {target}")]
    WriteHome {
        /// The file-system block.
        target: u64,
        /// The device's error.
        #[source]
        source: E,
    },
    /// Flushing the device failed.
    #[error("cannot flush the device")]
    Flush(#[source] E),
}

/// The blocks one transaction changes: each file-system block's new contents.
///
/// Logging a block twice keeps the second copy. The blocks are logged and checkpointed in
/// block-number order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    block_size: usize,
    wide: bool,
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl Transaction {
    /// Adds file-system block `target` with the contents `data`, replacing any copy already
    /// in the transaction.
    ///
    /// # Errors
    ///
    /// [`LogError::BlockSize`] when `data` is not one journal block long, and
    /// [`LogError::TargetTooWide`] for a block number over 32 bits in a journal without
    /// `INCOMPAT_64BIT`.
    pub fn log(&mut self, target: u64, data: &[u8]) -> Result<(), LogError> {
        if data.len() != self.block_size {
            return Err(LogError::BlockSize {
                expected: self.block_size,
                found: data.len(),
            });
        }
        if !self.wide && u32::try_from(target).is_err() {
            return Err(LogError::TargetTooWide { target });
        }
        self.blocks.insert(target, data.to_vec());
        Ok(())
    }

    /// The blocks logged so far, in block-number order.
    pub fn blocks(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.blocks
            .iter()
            .map(|(&target, data)| (target, data.as_slice()))
    }

    /// The number of blocks logged.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether no block has been logged.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// When a transaction was committed, for the commit block's `h_commit_sec` and
/// `h_commit_nsec`. Replay does not use it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitTime {
    /// Seconds since the Unix epoch.
    pub seconds: u64,
    /// Nanoseconds into the second.
    pub nanoseconds: u32,
}

/// A transaction that is in the log and still to be checkpointed.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a committed transaction must be checkpointed before the next one"]
pub struct Committed {
    transaction: Transaction,
    /// The [`Writer`] that committed it.
    writer: usize,
    sequence: u32,
    log_blocks: u32,
}

impl Committed {
    /// The transaction's sequence number.
    pub const fn sequence(&self) -> u32 {
        self.sequence
    }

    /// The journal blocks it took: descriptors, copies and the commit block.
    pub const fn log_blocks(&self) -> u32 {
        self.log_blocks
    }
}

/// Writes transactions into a journal whose log is empty.
///
/// # Examples
///
/// ```
/// use core::convert::Infallible;
/// use ferric_jbd2::{CommitTime, JournalRead, JournalWrite, Recovery, Superblock, Writer};
///
/// /// A 64-block journal of 1 KiB blocks and a 64-block file system, in memory.
/// struct Memory {
///     journal: Vec<u8>,
///     home: Vec<u8>,
/// }
///
/// impl JournalRead for Memory {
///     type Error = Infallible;
///     fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), Infallible> {
///         let at = block as usize * 1024;
///         buf.copy_from_slice(&self.journal[at..at + 1024]);
///         Ok(())
///     }
/// }
///
/// impl JournalWrite for Memory {
///     fn write_block(&mut self, block: u32, buf: &[u8]) -> Result<(), Infallible> {
///         let at = block as usize * 1024;
///         self.journal[at..at + 1024].copy_from_slice(buf);
///         Ok(())
///     }
///     fn write_home(&mut self, target: u64, buf: &[u8]) -> Result<(), Infallible> {
///         let at = target as usize * 1024;
///         self.home[at..at + 1024].copy_from_slice(buf);
///         Ok(())
///     }
///     fn flush(&mut self) -> Result<(), Infallible> {
///         Ok(())
///     }
/// }
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// // An empty version 2 journal: magic, block type 4, block size, length, first log block,
/// // sequence 1, start 0.
/// let mut journal = vec![0u8; 64 * 1024];
/// for (at, value) in [(0, 0xc03b_3998u32), (4, 4), (12, 1024), (16, 64), (20, 1), (24, 1)] {
///     journal[at..at + 4].copy_from_slice(&value.to_be_bytes());
/// }
/// let mut memory = Memory { journal, home: vec![0; 64 * 1024] };
/// let mut writer = Writer::new(&memory.journal[..1024])?;
/// let mut transaction = writer.transaction();
/// transaction.log(5, &[0x55; 1024])?;
/// let committed = writer.commit(&mut memory, transaction, CommitTime::default())?;
///
/// // A crash now would leave a journal that replays the transaction.
/// let sb = Superblock::parse(&memory.journal[..1024])?;
/// let recovery = Recovery::scan(&memory, &sb)?;
/// assert_eq!(recovery.writes()[0].target, 5);
///
/// writer.checkpoint(&mut memory, committed)?;
/// assert_eq!(memory.home[5 * 1024], 0x55);
/// assert!(!Superblock::parse(&memory.journal[..1024])?.needs_recovery());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Writer {
    /// Journal block 0 as it is on disk.
    raw: Vec<u8>,
    sb: Superblock,
    /// Tells this writer's [`Committed`] tokens from another's.
    id: usize,
    state: State,
}

/// Where a [`Writer`] is between transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// The log is empty: the next transaction may be committed.
    Empty,
    /// Transaction `sequence` is in the log, waiting for its checkpoint.
    Committed {
        /// Its sequence number.
        sequence: u32,
    },
    /// A device error stopped a commit or checkpoint part-way, so the journal on disk may
    /// not be what this writer last wrote. Only a replay and a new writer move on.
    Poisoned,
}

/// The source of [`Writer::id`]s.
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

impl Writer {
    /// A writer for the journal whose block 0 is `raw` (one journal block long).
    ///
    /// # Errors
    ///
    /// [`OpenError::Superblock`] for a superblock [`Superblock::parse`] refuses,
    /// [`OpenError::BlockSize`] when `raw` is not one journal block long,
    /// [`OpenError::NotEmpty`] for a journal that needs recovery,
    /// [`OpenError::Aborted`] for one aborted after an error, and
    /// [`OpenError::UnsupportedChecksum`] for version 1 checksums.
    pub fn new(raw: &[u8]) -> Result<Self, OpenError> {
        let sb = Superblock::parse(raw)?;
        let expected = sb.block_size() as usize;
        if raw.len() != expected {
            return Err(OpenError::BlockSize {
                expected,
                found: raw.len(),
            });
        }
        if sb.needs_recovery() {
            return Err(OpenError::NotEmpty);
        }
        if sb.errno() != 0 {
            return Err(OpenError::Aborted { errno: sb.errno() });
        }
        if sb.checksum() == ChecksumKind::V1 {
            return Err(OpenError::UnsupportedChecksum);
        }
        Ok(Self {
            raw: raw.to_vec(),
            sb,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            state: State::Empty,
        })
    }

    /// The journal superblock as last written.
    pub const fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// An empty transaction for this journal.
    pub fn transaction(&self) -> Transaction {
        Transaction {
            block_size: self.sb.block_size() as usize,
            wide: self.sb.is_64bit(),
            blocks: BTreeMap::new(),
        }
    }

    /// The journal blocks `transaction` takes in the log: its descriptors, its copies and
    /// the commit block.
    pub fn log_blocks(&self, transaction: &Transaction) -> u32 {
        let copies = transaction.len();
        let descriptors = copies.div_ceil(self.tags_per_descriptor());
        u32::try_from(descriptors + copies + 1).unwrap_or(u32::MAX)
    }

    /// Writes `transaction` into the log and points the journal superblock at it (steps 1 to
    /// 3 of the module docs). Once this returns the transaction survives a crash; write it
    /// home with [`Writer::checkpoint`] before committing another.
    ///
    /// # Errors
    ///
    /// [`WriteError::NotEmpty`] while an earlier transaction is still in the log,
    /// [`WriteError::Poisoned`] after an earlier device error, [`WriteError::Empty`] for a
    /// transaction with no blocks, [`WriteError::WrongJournal`] for one made by another
    /// writer's [`Writer::transaction`] for a different journal, [`WriteError::TooLarge`]
    /// when it does not fit the log, and [`WriteError::Write`], [`WriteError::Flush`] or
    /// [`WriteError::Superblock`] when the device fails. Nothing is written before those
    /// checks pass. After a device error the superblock may or may not point at the
    /// transaction, so the writer is poisoned: replay the journal and make a new writer.
    pub fn commit<J: JournalWrite>(
        &mut self,
        journal: &mut J,
        transaction: Transaction,
        time: CommitTime,
    ) -> Result<Committed, WriteError<J::Error>> {
        match self.state {
            State::Poisoned => return Err(WriteError::Poisoned),
            State::Committed { .. } => return Err(WriteError::NotEmpty),
            State::Empty if self.sb.needs_recovery() => return Err(WriteError::NotEmpty),
            State::Empty => {}
        }
        if transaction.is_empty() {
            return Err(WriteError::Empty);
        }
        if transaction.block_size != self.sb.block_size() as usize
            || transaction.wide != self.sb.is_64bit()
        {
            return Err(WriteError::WrongJournal);
        }
        let needed = self.log_blocks(&transaction);
        let room = self.sb.max_len().saturating_sub(self.sb.first());
        if needed > room {
            return Err(WriteError::TooLarge { needed, room });
        }
        let sequence = self.sb.sequence();
        // From the first write on, any failure leaves the journal in an unknown state.
        self.state = State::Poisoned;
        self.log(journal, &transaction, sequence, time)?;
        self.state = State::Committed { sequence };
        Ok(Committed {
            transaction,
            writer: self.id,
            sequence,
            log_blocks: needed,
        })
    }

    /// Writes the log, the commit block and the superblock for [`Writer::commit`].
    fn log<J: JournalWrite>(
        &mut self,
        journal: &mut J,
        transaction: &Transaction,
        sequence: u32,
        time: CommitTime,
    ) -> Result<(), WriteError<J::Error>> {
        let mut at = self.sb.first();
        let blocks: Vec<(u64, &[u8])> = transaction.blocks().collect();
        for run in blocks.chunks(self.tags_per_descriptor()) {
            at = self.log_run(journal, at, sequence, run)?;
        }
        flush(journal)?;
        let commit = self.commit_block(sequence, time);
        write(journal, at, &commit)?;
        flush(journal)?;
        let first = self.sb.first();
        self.write_superblock(journal, sequence, first)
    }

    /// Writes a committed transaction's blocks home, then empties the log (steps 4 and 5 of
    /// the module docs).
    ///
    /// # Errors
    ///
    /// [`WriteError::ForeignCommit`] for a [`Committed`] that is not this writer's pending
    /// transaction (another writer's, or one already checkpointed), [`WriteError::Poisoned`]
    /// after an earlier device error, and [`WriteError::WriteHome`], [`WriteError::Write`],
    /// [`WriteError::Flush`] or [`WriteError::Superblock`] when the device fails. The
    /// transaction is still in the log then, and a replay finishes the checkpoint; the writer
    /// is poisoned.
    pub fn checkpoint<J: JournalWrite>(
        &mut self,
        journal: &mut J,
        committed: Committed,
    ) -> Result<(), WriteError<J::Error>> {
        match self.state {
            State::Poisoned => return Err(WriteError::Poisoned),
            State::Committed { sequence }
                if committed.writer == self.id && committed.sequence == sequence => {}
            State::Committed { .. } | State::Empty => return Err(WriteError::ForeignCommit),
        }
        self.state = State::Poisoned;
        self.write_home(journal, &committed)?;
        self.state = State::Empty;
        Ok(())
    }

    /// Writes the blocks home and empties the log for [`Writer::checkpoint`].
    fn write_home<J: JournalWrite>(
        &mut self,
        journal: &mut J,
        committed: &Committed,
    ) -> Result<(), WriteError<J::Error>> {
        for (target, data) in committed.transaction.blocks() {
            journal
                .write_home(target, data)
                .map_err(|source| WriteError::WriteHome { target, source })?;
        }
        flush(journal)?;
        self.write_superblock(journal, committed.sequence.wrapping_add(1), 0)
    }

    /// How many tags one descriptor block holds: the first is followed by the UUID.
    fn tags_per_descriptor(&self) -> usize {
        let room = self.sb.block_size() as usize - TAGS_AT - log::tail_len(&self.sb) - UUID_LEN;
        room / log::tag_size(&self.sb)
    }

    /// Writes one descriptor at journal block `at` and the copies it lists after it;
    /// returns the next free journal block.
    fn log_run<J: JournalWrite>(
        &self,
        journal: &mut J,
        mut at: u32,
        sequence: u32,
        run: &[(u64, &[u8])],
    ) -> Result<u32, WriteError<J::Error>> {
        let mut descriptor = self.header(DESCRIPTOR, sequence);
        let mut copies = Vec::with_capacity(run.len());
        let mut tag_at = TAGS_AT;
        for (i, &(target, data)) in run.iter().enumerate() {
            let mut copy = data.to_vec();
            let escaped = copy[..4] == MAGIC.to_be_bytes();
            if escaped {
                copy[..4].fill(0);
            }
            let checksum = if self.sb.checksum().is_crc32c() {
                log::data_checksum(&self.sb, sequence, &copy)
            } else {
                0
            };
            let tag = Tag {
                target,
                escaped,
                checksum,
            };
            let mut flags = if i == 0 { 0 } else { FLAG_SAME_UUID };
            if i + 1 == run.len() {
                flags |= FLAG_LAST_TAG;
            }
            log::put_tag(&mut descriptor, tag_at, &self.sb, &tag, flags);
            tag_at += log::tag_size(&self.sb);
            if i == 0 {
                descriptor[tag_at..tag_at + UUID_LEN].copy_from_slice(self.sb.uuid());
                tag_at += UUID_LEN;
            }
            copies.push(copy);
        }
        if self.sb.checksum().is_crc32c() {
            let tail = descriptor.len() - 4;
            let crc = crc32c_without(&[self.sb.uuid()], &descriptor, tail);
            put32(&mut descriptor, tail, crc);
        }
        write(journal, at, &descriptor)?;
        for copy in copies {
            at += 1;
            write(journal, at, &copy)?;
        }
        Ok(at + 1)
    }

    /// A commit block for transaction `sequence`.
    fn commit_block(&self, sequence: u32, time: CommitTime) -> Vec<u8> {
        let mut block = self.header(COMMIT, sequence);
        block[COMMIT_SEC_AT..COMMIT_SEC_AT + 8].copy_from_slice(&time.seconds.to_be_bytes());
        put32(&mut block, COMMIT_NSEC_AT, time.nanoseconds);
        if self.sb.checksum().is_crc32c() {
            let crc = crc32c_without(&[self.sb.uuid()], &block, COMMIT_CHECKSUM_AT);
            put32(&mut block, COMMIT_CHECKSUM_AT, crc);
        }
        block
    }

    /// A zeroed journal block with the block header.
    fn header(&self, kind: u32, sequence: u32) -> Vec<u8> {
        let mut block = vec![0; self.sb.block_size() as usize];
        put32(&mut block, 0, MAGIC);
        put32(&mut block, 4, kind);
        put32(&mut block, 8, sequence);
        block
    }

    /// Writes the superblock with `s_sequence` and `s_start` set, and flushes.
    fn write_superblock<J: JournalWrite>(
        &mut self,
        journal: &mut J,
        sequence: u32,
        start: u32,
    ) -> Result<(), WriteError<J::Error>> {
        let mut raw = self.raw.clone();
        self.sb.set_log(&mut raw, sequence, start)?;
        write(journal, 0, &raw)?;
        flush(journal)?;
        self.sb = Superblock::parse(&raw)?;
        self.raw = raw;
        Ok(())
    }
}

fn write<J: JournalWrite>(
    journal: &mut J,
    block: u32,
    buf: &[u8],
) -> Result<(), WriteError<J::Error>> {
    journal
        .write_block(block, buf)
        .map_err(|source| WriteError::Write { block, source })
}

fn flush<J: JournalWrite>(journal: &mut J) -> Result<(), WriteError<J::Error>> {
    journal.flush().map_err(WriteError::Flush)
}

#[cfg(test)]
mod tests;
