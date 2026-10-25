//! Working out what a replay writes.
//!
//! [`Recovery::scan`] follows "Journal Replay" ideas in "Ext4 Disk Layout": the log is read
//! from `s_start`, expecting `s_sequence`, one transaction after another. A block that lacks
//! the magic number or has another sequence number ends the log. So does a descriptor or
//! revoke block whose tail checksum fails, or a commit block whose checksum fails: those are
//! the blocks of a transaction that was still being written, and a transaction counts only
//! once its commit block is valid. The log wraps from `s_maxlen - 1` back to `s_first`.
//!
//! A revoke record for block `b` in transaction `r` cancels the copies of `b` logged by
//! transactions up to and including `r`: they are stale (the block was freed, then maybe
//! reused for data that bypasses the journal). Copies in later transactions still count.
//! Of the copies left, the newest wins, so each block is written once.
//!
//! With version 2 or 3 checksums, the data block of every tag is read and checked when its
//! transaction's commit block is reached, before anything is planned:
//!
//! - With `INCOMPAT_ASYNC_COMMIT` the commit block may be written before the data it
//!   commits, so a transaction with a damaged data block is one that was still being
//!   written: it counts as uncommitted and ends the log, and the transactions before it
//!   are replayed.
//! - Otherwise the commit block is written last, so a damaged block under a valid commit is
//!   damage to the log. It stops the replay if that copy is the one to write; a copy that a
//!   newer one replaces, or that is revoked, is never written, so its damage does not
//!   matter.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use thiserror::Error;

use crate::log::{self, COMMIT, DESCRIPTOR, REVOKE};
use crate::{JournalRead, MAGIC, Superblock};

/// One block to write back: the newest live journal copy of a file-system block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    /// The file-system block to write.
    pub target: u64,
    /// The journal block that holds its contents.
    pub source: u32,
    /// The transaction that logged it.
    pub sequence: u32,
    escaped: bool,
    checksum: u32,
    /// Whether the data block matched its checksum when the log was scanned.
    intact: bool,
}

/// Why a scan or a read for replay failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RecoveryError<E: core::error::Error + 'static> {
    /// Reading a journal block failed.
    #[error("cannot read journal block {block}")]
    Read {
        /// The journal block.
        block: u32,
        /// The device's error.
        #[source]
        source: E,
    },
    /// The buffer passed to [`Recovery::read`] is not one journal block long.
    #[error("a journal block is {expected} bytes, the buffer is {found}")]
    BufferSize {
        /// The journal block size.
        expected: usize,
        /// The buffer's length.
        found: usize,
    },
    /// A revoke block of a committed transaction counts more bytes than the block holds.
    #[error("revoke block at journal block {block} counts {count} bytes")]
    BadRevokeCount {
        /// The journal block.
        block: u32,
        /// Its `r_count`.
        count: u32,
    },
    /// The log runs on past its own start (a descriptor lists more blocks than the journal
    /// has room for), which a working journal never writes.
    #[error("the journal log runs past its own start")]
    LogOverrun,
    /// A data block of a committed transaction does not match its tag's checksum.
    #[error(
        "journal block {source_block} (file-system block {target}, transaction {sequence}) \
         fails its checksum"
    )]
    DataChecksum {
        /// The journal block.
        source_block: u32,
        /// The file-system block it belongs at.
        target: u64,
        /// The transaction.
        sequence: u32,
    },
}

/// The result of scanning a journal: what to write, and what to leave in the superblock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    writes: Vec<Write>,
    transactions: u32,
    next_sequence: u32,
    block_size: usize,
}

/// A transaction being read: its tags, each with its data block and whether that block
/// matched its checksum, and its revoke records.
#[derive(Default)]
struct Transaction {
    tags: Vec<(log::Tag, u32, bool)>,
    revokes: Vec<u64>,
}

/// Where the scan is in the log.
struct Cursor<'a> {
    sb: &'a Superblock,
    block: u32,
    /// Log blocks visited, to stop a log that overruns itself.
    visited: u32,
}

impl Cursor<'_> {
    /// The log block `n` blocks after the current one.
    fn ahead(&self, n: u32) -> u32 {
        let len = u64::from(self.sb.max_len() - self.sb.first());
        let offset = (u64::from(self.block - self.sb.first()) + u64::from(n)) % len;
        // `offset < len`, and `first + len` is `max_len`, a u32.
        self.sb.first() + offset as u32
    }

    /// Moves `n` blocks on, or fails once the scan has gone further than all the way round.
    fn advance<E: core::error::Error>(&mut self, n: u32) -> Result<(), RecoveryError<E>> {
        let len = self.sb.max_len() - self.sb.first();
        self.visited = self.visited.saturating_add(n);
        if self.visited > len {
            return Err(RecoveryError::LogOverrun);
        }
        self.block = self.ahead(n);
        Ok(())
    }
}

impl Recovery {
    /// Scans the log of the journal `sb` describes and plans the replay. A clean journal
    /// ([`Superblock::needs_recovery`] false) gives an empty plan.
    ///
    /// Every data block in the log is read and checked here (see the module docs for what
    /// a damaged one means), so a plan that comes back can be applied in full.
    ///
    /// # Errors
    ///
    /// [`RecoveryError::Read`] when a block cannot be read, [`RecoveryError::BadRevokeCount`]
    /// for a malformed revoke block, [`RecoveryError::LogOverrun`] for a log that overruns itself and
    /// [`RecoveryError::DataChecksum`] when a block to replay fails its checksum.
    pub fn scan<J: JournalRead>(
        journal: &J,
        sb: &Superblock,
    ) -> Result<Self, RecoveryError<J::Error>> {
        let block_size = sb.block_size() as usize;
        let mut recovery = Self {
            writes: Vec::new(),
            transactions: 0,
            next_sequence: sb.sequence(),
            block_size,
        };
        if !sb.needs_recovery() {
            return Ok(recovery);
        }
        let mut end_sequence = sb.sequence();
        let committed = read_log(journal, sb, &mut end_sequence)?;
        recovery.next_sequence = end_sequence.wrapping_add(1);
        recovery.transactions = u32::try_from(committed.len()).unwrap_or(u32::MAX);
        recovery.writes = plan(sb, committed);
        if let Some(bad) = recovery.writes.iter().find(|write| !write.intact) {
            return Err(RecoveryError::DataChecksum {
                source_block: bad.source,
                target: bad.target,
                sequence: bad.sequence,
            });
        }
        Ok(recovery)
    }

    /// The blocks to write, one per file-system block, in block order.
    pub fn writes(&self) -> &[Write] {
        &self.writes
    }

    /// How many committed transactions the log held.
    pub const fn transactions(&self) -> u32 {
        self.transactions
    }

    /// The sequence number to store in the superblock once the replay is on disk (see
    /// [`Superblock::mark_recovered`]): one past the first sequence that had no commit, or
    /// the superblock's own for a clean journal.
    pub const fn next_sequence(&self) -> u32 {
        self.next_sequence
    }

    /// Reads the contents to write for `write` into `buf` (one journal block long): checks
    /// the data checksum and puts back an escaped magic number.
    ///
    /// # Errors
    ///
    /// [`RecoveryError::BufferSize`] for a buffer of the wrong length, [`RecoveryError::Read`]
    /// when the block cannot be read and [`RecoveryError::DataChecksum`] when it fails its
    /// checksum.
    pub fn read<J: JournalRead>(
        &self,
        journal: &J,
        sb: &Superblock,
        write: &Write,
        buf: &mut [u8],
    ) -> Result<(), RecoveryError<J::Error>> {
        if buf.len() != self.block_size {
            return Err(RecoveryError::BufferSize {
                expected: self.block_size,
                found: buf.len(),
            });
        }
        read_block(journal, write.source, buf)?;
        if !checksum_ok(sb, write.sequence, write.escaped, write.checksum, buf) {
            return Err(RecoveryError::DataChecksum {
                source_block: write.source,
                target: write.target,
                sequence: write.sequence,
            });
        }
        if write.escaped {
            buf[..4].copy_from_slice(&MAGIC.to_be_bytes());
        }
        Ok(())
    }
}

/// Whether `data`, as read from the log for a tag of transaction `sequence`, matches the
/// tag's checksum; always true for a journal without version 2 or 3 checksums. For an
/// escaped block either form is accepted (see the crate docs).
fn checksum_ok(
    sb: &Superblock,
    sequence: u32,
    escaped: bool,
    checksum: u32,
    data: &mut [u8],
) -> bool {
    if !sb.checksum().is_crc32c() || log::data_checksum(sb, sequence, data) == checksum {
        return true;
    }
    if !escaped {
        return false;
    }
    let stored = [data[0], data[1], data[2], data[3]];
    data[..4].copy_from_slice(&MAGIC.to_be_bytes());
    let ok = log::data_checksum(sb, sequence, data) == checksum;
    data[..4].copy_from_slice(&stored);
    ok
}

fn read_block<J: JournalRead>(
    journal: &J,
    block: u32,
    buf: &mut [u8],
) -> Result<(), RecoveryError<J::Error>> {
    journal
        .read_block(block, buf)
        .map_err(|source| RecoveryError::Read { block, source })
}

/// Reads the log and returns its committed transactions with their sequence numbers.
/// `sequence` is left at the first sequence that had no valid commit.
fn read_log<J: JournalRead>(
    journal: &J,
    sb: &Superblock,
    sequence: &mut u32,
) -> Result<Vec<(u32, Transaction)>, RecoveryError<J::Error>> {
    let mut buf = vec![0; sb.block_size() as usize];
    let mut cursor = Cursor {
        sb,
        block: sb.start(),
        visited: 0,
    };
    let mut committed = Vec::new();
    let mut open = Transaction::default();
    loop {
        read_block(journal, cursor.block, &mut buf)?;
        match log::header(&buf) {
            Some((DESCRIPTOR, seq)) if seq == *sequence && log::tail_ok(&buf, sb) => {
                let tags = log::tags(&buf, sb);
                let count = u32::try_from(tags.len()).unwrap_or(u32::MAX);
                for (i, tag) in (1..).zip(tags) {
                    open.tags.push((tag, cursor.ahead(i), true));
                }
                cursor.advance(1 + count)?;
            }
            Some((REVOKE, seq)) if seq == *sequence && log::tail_ok(&buf, sb) => {
                let records =
                    log::revokes(&buf, sb).map_err(|bad| RecoveryError::BadRevokeCount {
                        block: cursor.block,
                        count: bad.0,
                    })?;
                open.revokes.extend(records);
                cursor.advance(1)?;
            }
            Some((COMMIT, seq)) if seq == *sequence && log::commit_ok(&buf, sb) => {
                let whole = check_data(journal, sb, seq, &mut open, &mut buf)?;
                if !whole && sb.is_async_commit() {
                    // The commit reached the disk before all of its data: the transaction
                    // was still being written, so the log ends before it.
                    return Ok(committed);
                }
                committed.push((seq, core::mem::take(&mut open)));
                *sequence = sequence.wrapping_add(1);
                cursor.advance(1)?;
            }
            // Anything else ends the log; `open`, if it has blocks, never committed.
            _ => return Ok(committed),
        }
    }
}

/// Reads and checks every data block of transaction `seq`, marking each tag; returns whether
/// all of them matched.
fn check_data<J: JournalRead>(
    journal: &J,
    sb: &Superblock,
    seq: u32,
    transaction: &mut Transaction,
    buf: &mut [u8],
) -> Result<bool, RecoveryError<J::Error>> {
    if !sb.checksum().is_crc32c() {
        return Ok(true);
    }
    let mut whole = true;
    for (tag, source, intact) in &mut transaction.tags {
        read_block(journal, *source, buf)?;
        *intact = checksum_ok(sb, seq, tag.escaped, tag.checksum, buf);
        whole &= *intact;
    }
    Ok(whole)
}

/// Whether transaction `a` is the same as or older than `b`, with sequence numbers that
/// wrap around.
fn not_after(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) as i32 >= 0
}

/// The newest unrevoked copy of every block the committed transactions logged.
fn plan(sb: &Superblock, committed: Vec<(u32, Transaction)>) -> Vec<Write> {
    let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
    if sb.has_revoke() {
        for (seq, transaction) in &committed {
            for &block in &transaction.revokes {
                revoked
                    .entry(block)
                    .and_modify(|newest| {
                        if not_after(*newest, *seq) {
                            *newest = *seq;
                        }
                    })
                    .or_insert(*seq);
            }
        }
    }
    let mut writes: BTreeMap<u64, Write> = BTreeMap::new();
    for (seq, transaction) in committed {
        for (tag, source, intact) in transaction.tags {
            let cancelled = revoked
                .get(&tag.target)
                .is_some_and(|&revoked_in| not_after(seq, revoked_in));
            if !cancelled {
                writes.insert(
                    tag.target,
                    Write {
                        target: tag.target,
                        source,
                        sequence: seq,
                        escaped: tag.escaped,
                        checksum: tag.checksum,
                        intact,
                    },
                );
            }
        }
    }
    writes.into_values().collect()
}

#[cfg(test)]
mod tests;
