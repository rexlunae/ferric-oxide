//! Replaying the journal onto the volume before it is served (features.toml
//! `fs.ext4_journal_replay`, Cargo feature `journal-replay`, off by default).
//!
//! A volume that was not cleanly unmounted has `INCOMPAT_RECOVER` set: its journal holds
//! committed transactions whose blocks may not have reached their home locations. The
//! `ext4-view` reader copes by overlaying the journal in memory, which is all a read-only
//! mount needs. Writing to the volume needs the replay on disk first; this module does it,
//! as the groundwork for writable ext4:
//!
//! 1. `map` finds the journal inode (`s_journal_inum`, normally 8) and its blocks.
//! 2. `ferric-jbd2` scans the log and plans the writes, checking every checksum before
//!    anything is written.
//! 3. Each planned block is written to the volume, then the device is flushed.
//! 4. The journal superblock is marked empty (`s_start` 0, next sequence) and flushed.
//! 5. `INCOMPAT_RECOVER` is cleared in the volume superblock, whose checksum is updated
//!    when the volume has `metadata_csum`, and flushed.
//!
//! The order makes the replay safe to interrupt: until step 4 is on disk the journal still
//! describes every block, so replaying again gives the same result. That is the order
//! `e2fsck -E journal_only` leaves its results in; the tests compare the two block by block.
//!
//! The same journal inode map, opened journal superblock and block access also serve
//! `crate::write` (features.toml `fs.ext4_write`), which writes new transactions through the
//! journal.
//!
//! Only internal journals are replayed; a journal on another device is refused with
//! [`JournalError::ExternalJournal`]. Orphan-inode processing, which e2fsck also does after
//! a crash, is not part of journal replay and is left for writable ext4.
//!
//! # Specification
//!
//! See the `map` module for the ext4 structures and `ferric-jbd2` for the journal format, both from
//! "Ext4 Disk Layout" on the ext4 wiki at kernel.org. No code here comes from the Linux
//! kernel or from e2fsprogs.

pub(crate) mod map;

use core::ops::{Deref, DerefMut};

use ferric_jbd2::{JournalRead, Recovery, RecoveryError, Superblock, SuperblockError};
use thiserror::Error;

use crate::device::{ReadAt, Window, WindowError, WriteAt};
use crate::superblock::{self, INCOMPAT_RECOVER, SuperblockError as VolumeSuperblockError};
use map::{BlockMap, CHECKSUM_AT, RO_COMPAT_METADATA_CSUM, Volume};

/// Where the volume superblock keeps `s_feature_incompat`.
const INCOMPAT_AT: usize = 0x60;
/// Where the volume superblock keeps `s_feature_ro_compat`.
const RO_COMPAT_AT: usize = 0x64;

/// Why the journal could not be replayed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum JournalError {
    /// The volume superblock was refused.
    #[error("bad superblock")]
    Superblock(#[from] VolumeSuperblockError),
    /// The volume superblock fails its checksum (with `metadata_csum`).
    #[error("superblock checksum {stored:#010x}, expected {computed:#010x}")]
    SuperblockChecksum {
        /// The value stored.
        stored: u32,
        /// The value computed.
        computed: u32,
    },
    /// The journal inode's block map or size is not the copy the superblock keeps.
    #[error("the journal inode does not match the superblock's copy of its block map")]
    JournalBackupMismatch,
    /// The journal inode maps a block outside the volume.
    #[error("the journal inode maps block {block}, outside the volume")]
    JournalOutsideVolume {
        /// The first block of the run.
        block: u64,
    },
    /// The journal inode maps a block twice, or onto the volume's fixed metadata.
    #[error("the journal inode maps block {block}, which is already in use")]
    JournalOverlap {
        /// The first block that overlaps.
        block: u64,
    },
    /// The journal asks to replay a block that belongs to the journal itself.
    #[error("the journal would overwrite its own block {block}")]
    TargetInJournal {
        /// The block.
        block: u64,
    },
    /// The volume superblock's inode or group-descriptor fields are out of range.
    #[error("the superblock's inode or group descriptor sizes are out of range")]
    BadSuperblock,
    /// The journal lives on another device.
    #[error("the journal is on an external device, which is not supported")]
    ExternalJournal,
    /// A layout this code does not handle.
    #[error("unsupported: {what}")]
    Unsupported {
        /// What it is.
        what: &'static str,
    },
    /// The journal inode is not a usable journal file.
    #[error("journal inode {number} is malformed")]
    BadInode {
        /// The inode number.
        number: u32,
    },
    /// The journal inode's extent tree is malformed.
    #[error("the journal inode's extent tree is malformed")]
    BadExtentTree,
    /// A block number points past the end of the volume.
    #[error("block {block} is past the end of the volume")]
    BlockOutOfRange {
        /// The block number.
        block: u64,
    },
    /// The journal's block size is not the volume's.
    #[error("the journal's block size {journal} is not the volume's {volume}")]
    BlockSize {
        /// The journal block size.
        journal: u32,
        /// The volume block size.
        volume: u32,
    },
    /// The inode is shorter than the length the journal superblock gives.
    #[error("the journal claims {claimed} blocks but its inode has {mapped}")]
    Length {
        /// `s_maxlen`.
        claimed: u32,
        /// Blocks in the inode.
        mapped: u32,
    },
    /// The journal superblock was refused.
    #[error("bad journal superblock")]
    JournalSuperblock(#[from] SuperblockError),
    /// Scanning the log or reading a block to replay failed.
    #[error("journal recovery failed")]
    Recovery(#[from] RecoveryError<BlockError>),
    /// Reading or writing the volume failed.
    #[error("device")]
    Device(#[from] WindowError),
}

/// Why reading or writing a journal block failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BlockError {
    /// The block is past the end of the journal inode.
    #[error("journal block {block} is not mapped")]
    Unmapped {
        /// The journal block.
        block: u32,
    },
    /// A file-system block to write is past the end of the volume.
    #[error("block {block} is past the end of the volume")]
    OutOfRange {
        /// The block number.
        block: u64,
    },
    /// The device failed the read or write.
    #[error("device")]
    Device(#[from] WindowError),
}

/// What [`replay`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The volume did not need recovery (or has no journal); nothing was written.
    Clean,
    /// The journal was replayed and emptied.
    Replayed {
        /// Committed transactions found in the log.
        transactions: u32,
        /// Blocks written to the volume.
        blocks: usize,
        /// The journal's `s_errno`: nonzero when it was aborted after an error, in which
        /// case the volume should be checked with `e2fsck`.
        journal_errno: i32,
    },
}

/// Whether the ext4 volume in `window` has an internal journal that needs replaying: open
/// the device for writing and call [`replay`] when it does.
///
/// # Errors
///
/// A [`JournalError`] when the superblock cannot be read or is malformed, or the journal is
/// on another device ([`JournalError::ExternalJournal`]).
pub fn needs_replay<R: ReadAt>(window: &Window<R>) -> Result<bool, JournalError> {
    Volume::parse(&read_volume_superblock(window)?)?.needs_replay()
}

/// Replays the journal of the ext4 volume in `window` if it needs it.
///
/// # Errors
///
/// A [`JournalError`] for a volume or journal that cannot be read or is malformed, an
/// external journal, a block that fails its checksum, or a failed write. Nothing is
/// written unless the whole log has been read and checked first; a failure while writing
/// leaves the journal intact, so the replay can be run again.
///
/// # Examples
///
/// ```no_run
/// use ext4d::device::Window;
/// use ext4d::journal::{self, Outcome};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let image = std::fs::OpenOptions::new().read(true).write(true).open("disk.ext4")?;
/// let mut window = Window::whole(image)?;
/// if let Outcome::Replayed { blocks, .. } = journal::replay(&mut window)? {
///     println!("replayed {blocks} blocks");
/// }
/// # Ok(())
/// # }
/// ```
pub fn replay<R: WriteAt>(window: &mut Window<R>) -> Result<Outcome, JournalError> {
    let sb = read_volume_superblock(window)?;
    let volume = Volume::parse(&sb)?;
    if !volume.needs_replay()? {
        return Ok(Outcome::Clean);
    }
    let map = BlockMap::load(&volume, window)?;
    let (journal_sb_at, mut raw, jsb) = open_journal(&*window, &map, &volume)?;
    if !jsb.needs_recovery() {
        // The journal was emptied but the flag not cleared: a replay that stopped between
        // steps 4 and 5. Only step 5 is left.
        clear_recover_flag(window)?;
        return Ok(Outcome::Replayed {
            transactions: 0,
            blocks: 0,
            journal_errno: jsb.errno(),
        });
    }
    let journal = Journal {
        window: &*window,
        map: &map,
        volume: &volume,
    };
    let recovery = Recovery::scan(&journal, &jsb)?;
    for write in recovery.writes() {
        if write.target >= volume.blocks {
            return Err(JournalError::BlockOutOfRange {
                block: write.target,
            });
        }
        if map.contains(write.target) {
            return Err(JournalError::TargetInJournal {
                block: write.target,
            });
        }
    }
    write_blocks(window, &volume, &map, &jsb, &recovery)?;
    jsb.mark_recovered(&mut raw, recovery.next_sequence())?;
    window.write_all_at(&raw, journal_sb_at)?;
    window.flush()?;
    clear_recover_flag(window)?;
    Ok(Outcome::Replayed {
        transactions: recovery.transactions(),
        blocks: recovery.writes().len(),
        journal_errno: jsb.errno(),
    })
}

/// Reads and checks the journal superblock: returns where it is on the volume, journal block
/// 0's bytes and the parsed superblock.
pub(crate) fn open_journal<R: ReadAt>(
    window: &Window<R>,
    map: &BlockMap,
    volume: &Volume,
) -> Result<(u64, Vec<u8>, Superblock), JournalError> {
    let journal = Journal {
        window,
        map,
        volume,
    };
    let journal_sb_at = journal
        .offset(0)
        .map_err(|source| RecoveryError::Read { block: 0, source })?;
    let mut raw = vec![0; volume.block_size as usize];
    journal
        .read_block(0, &mut raw)
        .map_err(|source| RecoveryError::Read { block: 0, source })?;
    let jsb = Superblock::parse(&raw)?;
    if jsb.block_size() != volume.block_size {
        return Err(JournalError::BlockSize {
            journal: jsb.block_size(),
            volume: volume.block_size,
        });
    }
    if jsb.max_len() > map.len {
        return Err(JournalError::Length {
            claimed: jsb.max_len(),
            mapped: map.len,
        });
    }
    Ok((journal_sb_at, raw, jsb))
}

/// Writes every planned block, then flushes.
fn write_blocks<R: WriteAt>(
    window: &mut Window<R>,
    volume: &Volume,
    map: &BlockMap,
    jsb: &Superblock,
    recovery: &Recovery,
) -> Result<(), JournalError> {
    let mut buf = vec![0; volume.block_size as usize];
    for write in recovery.writes() {
        let journal = Journal {
            window: &*window,
            map,
            volume,
        };
        recovery.read(&journal, jsb, write, &mut buf)?;
        window.write_all_at(&buf, write.target * u64::from(volume.block_size))?;
    }
    window.flush()?;
    Ok(())
}

/// Clears `INCOMPAT_RECOVER` in the volume superblock, updates its checksum, and flushes.
/// The superblock is read again, and its own features decide the checksum, since the replay
/// may have rewritten it.
fn clear_recover_flag<R: WriteAt>(window: &mut Window<R>) -> Result<(), JournalError> {
    set_recover_flag(window, false)
}

/// Sets or clears `INCOMPAT_RECOVER` in the volume superblock, updates its checksum, and
/// flushes.
pub(crate) fn set_recover_flag<R: WriteAt>(
    window: &mut Window<R>,
    recover: bool,
) -> Result<(), JournalError> {
    let mut sb = read_volume_superblock(window)?;
    let mut incompat = map::le32(&sb, INCOMPAT_AT) & !INCOMPAT_RECOVER;
    if recover {
        incompat |= INCOMPAT_RECOVER;
    }
    sb[INCOMPAT_AT..INCOMPAT_AT + 4].copy_from_slice(&incompat.to_le_bytes());
    if map::le32(&sb, RO_COMPAT_AT) & RO_COMPAT_METADATA_CSUM != 0 {
        let crc = ferric_jbd2::crc32c(&[&sb[..CHECKSUM_AT]]);
        sb[CHECKSUM_AT..].copy_from_slice(&crc.to_le_bytes());
    }
    window.write_all_at(&sb, superblock::OFFSET)?;
    window.flush()?;
    Ok(())
}

pub(crate) fn read_volume_superblock<R: ReadAt>(
    window: &Window<R>,
) -> Result<[u8; superblock::SIZE], JournalError> {
    let mut sb = [0; superblock::SIZE];
    window.read_exact_at(&mut sb, superblock::OFFSET)?;
    Ok(sb)
}

/// The journal inode's blocks, read (and, through a `&mut Window`, written) through the
/// volume.
pub(crate) struct Journal<'a, W> {
    pub(crate) window: W,
    pub(crate) map: &'a BlockMap,
    pub(crate) volume: &'a Volume,
}

impl<R: ReadAt, W: Deref<Target = Window<R>>> Journal<'_, W> {
    /// The byte offset of journal block `block` on the volume.
    fn offset(&self, block: u32) -> Result<u64, BlockError> {
        let physical = self
            .map
            .physical(block)
            .filter(|&physical| physical < self.volume.blocks)
            .ok_or(BlockError::Unmapped { block })?;
        Ok(physical * u64::from(self.volume.block_size))
    }
}

impl<R: ReadAt, W: Deref<Target = Window<R>>> JournalRead for Journal<'_, W> {
    type Error = BlockError;

    fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), BlockError> {
        let offset = self.offset(block)?;
        Ok(self.window.read_exact_at(buf, offset)?)
    }
}

impl<R: WriteAt, W: DerefMut<Target = Window<R>>> ferric_jbd2::JournalWrite for Journal<'_, W> {
    fn write_block(&mut self, block: u32, buf: &[u8]) -> Result<(), BlockError> {
        let offset = self.offset(block)?;
        Ok(self.window.write_all_at(buf, offset)?)
    }

    fn write_home(&mut self, target: u64, buf: &[u8]) -> Result<(), BlockError> {
        if target >= self.volume.blocks {
            return Err(BlockError::OutOfRange { block: target });
        }
        let offset = target * u64::from(self.volume.block_size);
        Ok(self.window.write_all_at(buf, offset)?)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(self.window.flush()?)
    }
}
