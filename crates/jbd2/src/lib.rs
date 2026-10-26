//! `ferric-jbd2`: recovery planning for the JBD2 journal that ext3 and ext4 keep, `no_std`.
//!
//! A journal is a circular log of transactions. Each transaction is a run of blocks: one or
//! more *descriptor* blocks, each followed by the copies of the file-system blocks it lists,
//! optional *revoke* blocks, and a *commit* block. After a crash, a volume whose superblock
//! says it needs recovery must have every committed transaction written back to its home
//! location before it is mounted for writing. This crate works out what to write; the caller
//! (ext4d) does the writing.
//!
//! The pieces:
//!
//! - [`Superblock`]: the journal superblock (journal block 0), its features and checksum
//!   type, and [`Superblock::mark_recovered`], which empties the log once the replay is on
//!   disk.
//! - [`Recovery::scan`]: walks the log from `s_start`, keeps the transactions that have a
//!   valid commit block, applies their revoke records and yields one [`Write`] per
//!   file-system block: the newest copy of it that was not revoked.
//! - [`Recovery::read`]: reads the journal copy for a [`Write`], checks its checksum and
//!   restores an escaped magic number, ready to write to the file system.
//! - [`crc32c`]: the CRC32C form JBD2 and ext4 store, for callers that update ext4
//!   metadata after a replay.
//! - [`JournalRead`]: how the crate reads journal blocks. Journal block `n` is block `n` of
//!   the journal inode (or external device); mapping it to a disk address is the caller's job.
//!
//! # Specification
//!
//! Written from the "Journal (jbd2)" section of "Ext4 Disk Layout" on the ext4 wiki at
//! kernel.org (<https://ext4.wiki.kernel.org/index.php/Ext4_Disk_Layout#Journal_.28jbd2.29>),
//! which gives the block header, the superblock fields and features, the descriptor tag
//! layouts, revoke and commit blocks, and what each checksum covers. No code here comes from
//! the Linux kernel or from e2fsprogs.
//!
//! Where that document is silent, the behaviour was settled by experiment against
//! e2fsprogs 1.47, the reference tools: `debugfs` (`journal_open -c -v N`, `journal_write`)
//! wrote transactions into test volumes and `e2fsck -E journal_only` replayed them, and the
//! results were compared block by block. That is how the following were determined:
//!
//! - Every checksum is CRC32C (Castagnoli) started from `0xFFFF_FFFF` and *not* inverted at
//!   the end, over the journal UUID followed by the bytes the layout names (the superblock's
//!   own checksum covers just the superblock). That equals the standard CRC32C of the same
//!   bytes, XOR `0xFFFF_FFFF`.
//! - The tag checksum of an escaped block covers the block as stored, with its first four
//!   bytes zeroed. Since the layout does not say which form is meant, [`Recovery::read`]
//!   accepts either.
//! - With version 2 checksums, descriptor tags are two bytes longer than their fields
//!   (see the `log` module).
//! - After a replay, `s_sequence` is one past the first sequence number that had no commit:
//!   e2fsck leaves the log empty with a sequence that no leftover block can match.
//!
//! # What is not handled
//!
//! Fast commits (`INCOMPAT_FAST_COMMIT`) are refused with
//! [`SuperblockError::UnsupportedFeatures`]. The version 1 commit-block checksums
//! (`COMPAT_CHECKSUM`) are not verified; version 2 and 3 checksums are.
//!
//! # Examples
//!
//! ```
//! use ferric_jbd2::{JournalRead, Recovery, Superblock};
//!
//! /// A journal held in memory: block `n` is `bytes[n * 1024..]`.
//! struct Memory(Vec<u8>);
//!
//! impl JournalRead for Memory {
//!     type Error = core::convert::Infallible;
//!     fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
//!         let start = block as usize * buf.len();
//!         buf.copy_from_slice(&self.0[start..start + buf.len()]);
//!         Ok(())
//!     }
//! }
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // An empty version 2 journal of 64 blocks of 1 KiB: magic, block type 4, sequence 1,
//! // block size, length, first log block, start 0 (nothing to recover).
//! let mut journal = vec![0u8; 64 * 1024];
//! for (at, value) in [(0, 0xc03b_3998u32), (4, 4), (8, 1), (12, 1024), (16, 64), (20, 1), (24, 1)] {
//!     journal[at..at + 4].copy_from_slice(&value.to_be_bytes());
//! }
//! let journal = Memory(journal);
//! let mut raw = [0; 1024];
//! journal.read_block(0, &mut raw)?;
//! let sb = Superblock::parse(&raw)?;
//! assert!(!sb.needs_recovery());
//! let recovery = Recovery::scan(&journal, &sb)?;
//! assert!(recovery.writes().is_empty());
//! # Ok(())
//! # }
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

mod checksum;
mod log;
mod recovery;
mod superblock;

pub use checksum::crc32c;
pub use recovery::{Recovery, RecoveryError, Write};
pub use superblock::{ChecksumKind, Superblock, SuperblockError};

/// The magic number at the start of every journal metadata block (`h_magic`).
pub const MAGIC: u32 = 0xC03B_3998;

/// Reads blocks of a journal.
///
/// Block `n` is the journal's `n`th block: block 0 is the journal superblock, and the log
/// runs from [`Superblock::first`] to [`Superblock::max_len`]. For an ext4 journal inode the
/// caller maps these through the inode's extents.
pub trait JournalRead {
    /// The error a read can fail with.
    type Error: core::error::Error + 'static;

    /// Fills `buf`, which is one journal block long, with journal block `block`.
    ///
    /// # Errors
    ///
    /// Whatever the underlying device reports.
    fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
}

impl<J: JournalRead + ?Sized> JournalRead for &J {
    type Error = J::Error;

    fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        (**self).read_block(block, buf)
    }
}
