//! Changing the volume through its journal (features.toml `fs.ext4_write`, Cargo feature
//! `write`, off by default): the first step towards writable ext4.
//!
//! Every change is one JBD2 transaction, written with
//! [`ferric_jbd2::Writer`]. For each call:
//!
//! 1. The volume superblock is checked: the journal must be internal and empty (a volume
//!    that needs recovery is replayed first, by [`crate::journal::replay`]), and every
//!    feature the volume has must be one this module knows it can write under.
//! 2. The changed metadata blocks are built in memory, with their checksums.
//! 3. `INCOMPAT_RECOVER` is set in the volume superblock and flushed, so that `e2fsck` and
//!    ext4d replay the journal if the volume is not cleanly finished.
//! 4. The transaction is logged and committed, and the journal superblock pointed at it.
//! 5. The blocks are written home (the checkpoint) and the journal emptied.
//! 6. `INCOMPAT_RECOVER` is cleared again.
//!
//! A crash at any point leaves the volume as it was, or a journal whose replay finishes the
//! change; the tests crash a volume after every write and check both, and CI has `e2fsck`
//! replay and check what is left.
//!
//! The operations so far change one existing inode in place ([`change`]): its access,
//! modification and change times ([`set_times`], `futimens`), its permission bits
//! (`fchmod`), or its owner and group (`fchown`), each with a new change time; and
//! [`write_data`] writes into the blocks a regular file already has (`write`), as ext4's
//! ordered mode does: the data goes home first, unjournaled, and the inode's new size and
//! times then follow in one transaction. Nothing is allocated or freed yet, so a write
//! that would need a new block is refused. The scheme reaches them through
//! [`crate::fs::Fs::set_times`], [`crate::fs::Fs::set_mode`], [`crate::fs::Fs::set_owner`]
//! and [`crate::fs::Fs::write`], which check the caller's permission as POSIX says and find
//! the inode with [`crate::lookup`]. [`check_writable`] tells, at mount time, whether a
//! volume can be served read-write.
//!
//! An owner or group above 65535 needs the high halves the inode keeps for Linux and Hurd
//! volumes (`s_creator_os`); other creators keep different fields there, so such IDs are
//! refused on their volumes. On a volume with `quota`, a new owner or group would have to
//! move the inode's usage between quota records, which this module does not do yet, so it is
//! refused; a new mode is not counted and is written.
//!
//! # Specification
//!
//! Written from "Ext4 Disk Layout" on the ext4 wiki at kernel.org: "The Super Block"
//! (feature flags, `s_checksum_seed`, `s_creator_os`), "Inode Table" (the fields,
//! `i_extra_isize`, and the per-OS `osd2` union holding the high halves of the owner and
//! group),
//! "Inode Timestamps" (the epoch bits and nanoseconds in the `_extra` fields) and
//! "Checksums" (the inode checksum: CRC32C over the inode number, `i_generation` and the
//! whole inode with its checksum fields zero, from the volume's checksum seed). The journal
//! format is `ferric-jbd2`'s. The inode checksum was checked against volumes made by
//! e2fsprogs 1.47's `mke2fs`, and what this module writes is checked with its `e2fsck`. No
//! code here comes from the Linux kernel or from e2fsprogs.

use std::time::{SystemTime, UNIX_EPOCH};

use ferric_jbd2::{CommitTime, LogError, OpenError, Writer};
use thiserror::Error;

use crate::device::{ReadAt, Window, WindowError, WriteAt};
use crate::inode::{
    I_ATIME, I_ATIME_EXTRA, I_CTIME, I_CTIME_EXTRA, I_MTIME, I_MTIME_EXTRA, has_extra, le16,
};
use crate::journal::map::{BlockMap, RO_COMPAT_METADATA_CSUM, Volume, le32};
use crate::journal::{
    self, BlockError, Journal, JournalError, open_journal, read_volume_superblock,
};
use crate::superblock::COMPAT_HAS_JOURNAL;

/// `s_feature_incompat` bits that writing an inode's times, mode, owner or data in place is
/// safe under: `filetype`,
/// `recover`, `extents`, `64bit`, `flex_bg`, `ea_inode`, `csum_seed`, `largedir`,
/// `inline_data`, `encrypt` and `casefold`. Not `compression`, `journal_dev`, `mmp` (another
/// host may have the volume), `dirdata` or `meta_bg` (whose journal map the replay code
/// does not check yet), nor any bit the layout does not name.
const INCOMPAT_WRITABLE: u32 =
    0x2 | 0x4 | 0x40 | 0x80 | 0x200 | 0x400 | 0x2000 | 0x4000 | 0x8000 | 0x1_0000 | 0x2_0000;
/// `s_feature_ro_compat` bits likewise: `sparse_super`, `large_file`, `huge_file`,
/// `gdt_csum`, `dir_nlink`, `extra_isize`, `quota`, `bigalloc`, `metadata_csum`, `project`,
/// `verity` and `orphan_present`. Not `btree_dir`, `has_snapshot`, `replica`, `readonly` (the
/// volume asks not to be written) or `shared_blocks`, nor any bit the layout does not name.
const RO_COMPAT_WRITABLE: u32 =
    0x1 | 0x2 | 0x8 | 0x10 | 0x20 | 0x40 | 0x100 | 0x200 | 0x400 | 0x2000 | 0x8000 | 0x1_0000;

/// `s_feature_incompat`: metadata checksums start from `s_checksum_seed`, not the UUID.
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
/// Where the superblock keeps `s_uuid`.
const S_UUID: usize = 0x68;
/// Where the superblock keeps `s_checksum_seed`.
const S_CHECKSUM_SEED: usize = 0x270;
/// Where the superblock keeps `s_creator_os`, and the values whose inodes keep the high
/// halves of the owner and group at `0x78` and `0x7A`.
const S_CREATOR_OS: usize = 0x48;
const CREATOR_LINUX: u32 = 0;
const CREATOR_HURD: u32 = 1;
/// `s_feature_ro_compat`: quota files count each owner's and group's blocks and inodes.
const RO_COMPAT_QUOTA: u32 = 0x100;

/// `i_uid` and `i_gid`: the low 16 bits of the owner and group.
const I_UID: usize = 0x2;
const I_GID: usize = 0x18;
/// `l_i_uid_high` and `l_i_gid_high` (Linux; Hurd's are at the same offsets).
const I_UID_HIGH: usize = 0x78;
const I_GID_HIGH: usize = 0x7A;
/// `i_mode`'s file type bits and a regular file's value of them.
const S_IFMT: u16 = 0xF000;
const S_IFREG: u16 = 0x8000;
/// `i_mode`'s permission bits, and the set-ID bits among them.
const PERMISSION_BITS: u16 = 0o7777;
const S_ISUID: u16 = 0o4000;
const S_ISGID: u16 = 0o2000;

/// `i_size_lo` and `i_size_high`.
const I_SIZE_LO: usize = 0x4;
const I_SIZE_HIGH: usize = 0x6C;
/// `i_flags`, and the flags a data write checks.
const I_FLAGS: usize = 0x20;
const IMMUTABLE_FL: u32 = 0x10;
const APPEND_FL: u32 = 0x20;
const ENCRYPT_FL: u32 = 0x800;
const JOURNAL_DATA_FL: u32 = 0x4000;
const VERITY_FL: u32 = 0x10_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
/// `s_feature_ro_compat`: files may be 2^31 bytes or longer.
const RO_COMPAT_LARGE_FILE: u32 = 0x2;

/// Where an inode keeps `i_mode`.
const I_MODE: usize = 0x0;
/// `i_generation`, which the inode checksum covers first.
const I_GENERATION: usize = 0x64;
/// `l_i_checksum_lo`: the low 16 bits of the inode checksum.
const I_CHECKSUM_LO: usize = 0x7C;
/// `i_checksum_hi`: the high 16 bits of the inode checksum.
const I_CHECKSUM_HI: usize = 0x82;

/// Why the volume could not be changed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WriteError {
    /// The volume or its journal could not be read, or is malformed.
    #[error("cannot open the journal")]
    Journal(#[from] JournalError),
    /// The volume has no journal to write through.
    #[error("the volume has no journal")]
    NoJournal,
    /// The volume was not cleanly unmounted: replay its journal first.
    #[error("the volume's journal needs replaying first")]
    NeedsReplay,
    /// The volume has a feature this module does not write under.
    #[error("the volume has {field} feature {flags:#x}, which ext4d does not write under")]
    UnsupportedFeature {
        /// `s_feature_incompat` or `s_feature_ro_compat`.
        field: &'static str,
        /// The bits that are not supported.
        flags: u32,
    },
    /// The inode is free (`i_mode` is zero).
    #[error("inode {number} is not in use")]
    FreeInode {
        /// The inode number.
        number: u32,
    },
    /// The inode fails its checksum, so it is not rewritten.
    #[error("inode {number} has checksum {stored:#010x}, expected {computed:#010x}")]
    InodeChecksum {
        /// The inode number.
        number: u32,
        /// The value stored.
        stored: u32,
        /// The value computed.
        computed: u32,
    },
    /// A time cannot be stored in this inode: outside 1901 to 2038 without the extra
    /// fields, or 1901 to 2446 with them, or with nanoseconds of a second or more.
    #[error("the time {seconds}.{nanoseconds:09} cannot be stored in inode {number}")]
    TimeOutOfRange {
        /// The inode number.
        number: u32,
        /// The seconds since the epoch.
        seconds: i64,
        /// The nanoseconds.
        nanoseconds: u32,
    },
    /// Mode bits outside the permission bits (`0o7777`).
    #[error("{bits:#o} is not a set of permission bits")]
    InvalidPermissions {
        /// The bits given.
        bits: u16,
    },
    /// An owner or group above 65535 on a volume whose inodes keep only the low 16 bits
    /// (`s_creator_os` other than Linux or Hurd).
    #[error("inode {number} cannot hold the owner or group {id}")]
    IdOutOfRange {
        /// The inode number.
        number: u32,
        /// The ID that does not fit.
        id: u32,
    },
    /// A new owner or group on a volume with `quota`, whose quota files would have to move
    /// the inode's usage from one ID to another, which this module does not do.
    #[error("inode {number} is counted in the volume's quotas, which ext4d does not update")]
    QuotaTracked {
        /// The inode number.
        number: u32,
    },
    /// A data write to a file that is not a regular one.
    #[error("inode {number} is not a regular file")]
    NotAFile {
        /// The inode number.
        number: u32,
    },
    /// A data write to a file whose flags (`i_flags`) forbid it: immutable or append-only.
    #[error("inode {number} is immutable or append-only (flags {flags:#x})")]
    FileFlags {
        /// The inode number.
        number: u32,
        /// The flags that forbid the write.
        flags: u32,
    },
    /// A data write to a file kept in a way this module does not write: inline data,
    /// encrypted, verity-protected or with its data journaled.
    #[error("inode {number}'s data is kept in a way ext4d does not write yet (flags {flags:#x})")]
    UnsupportedFile {
        /// The inode number.
        number: u32,
        /// The flags that say how.
        flags: u32,
    },
    /// A data write to a block of the file that has no initialized block on the volume (a
    /// hole, an unwritten extent, or past the blocks it has): it would need a block
    /// allocated, which this module does not do yet.
    #[error("block {block} of inode {number} is not allocated")]
    NotAllocated {
        /// The inode number.
        number: u32,
        /// The block within the file.
        block: u64,
    },
    /// The inode maps a block of the file to one outside the volume or inside the journal.
    #[error("inode {number} maps a block to {physical}, outside its data")]
    BadDataBlock {
        /// The inode number.
        number: u32,
        /// The volume block it maps to.
        physical: u64,
    },
    /// A write ending past the largest size the inode can hold (2^31 - 1 bytes without
    /// `large_file`).
    #[error("inode {number} cannot be {size} bytes long")]
    FileTooLarge {
        /// The inode number.
        number: u32,
        /// The size the write would leave.
        size: u64,
    },
    /// The journal cannot be written to.
    #[error("cannot write to the journal")]
    Open(#[from] OpenError),
    /// A block could not be added to the transaction.
    #[error("cannot log the block")]
    Log(#[from] LogError),
    /// Committing or checkpointing the transaction failed.
    #[error("the journal transaction failed")]
    Transaction(#[from] ferric_jbd2::WriteError<BlockError>),
    /// Reading or writing the volume failed.
    #[error("device")]
    Device(#[from] WindowError),
}

/// A point in time: seconds since the Unix epoch and nanoseconds into the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    /// Seconds since 1970-01-01 00:00:00 UTC; negative before it.
    pub seconds: i64,
    /// Nanoseconds, below 1 000 000 000.
    pub nanoseconds: u32,
}

impl Timestamp {
    /// The current time, or the epoch if the clock is before it.
    pub fn now() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            seconds: i64::try_from(now.as_secs()).unwrap_or(i64::MAX),
            nanoseconds: now.subsec_nanos(),
        }
    }
}

/// A change [`change`] makes to an inode. Each also sets the change time (`i_ctime`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Change {
    /// The access, modification and change times (`futimens`).
    Times(Times),
    /// The permission bits (`fchmod`).
    Mode(Mode),
    /// The owner and group (`fchown`).
    Owner(Owner),
    /// What a data write leaves in the inode (`write`): a new size if the file grew, and
    /// the modification and change times.
    Written(Written),
}

/// What a [`Change::Written`] writes, after [`write_data`] has written the data itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// The new size (`i_size_lo`, `i_size_high`), when the write ended past the old one.
    pub size: Option<u64>,
    /// The new modification and change time.
    pub changed: Timestamp,
}

/// The permission bits a [`Change::Mode`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    /// The new permission bits, within `0o7777`; the file type is kept.
    pub permissions: u16,
    /// The new change time.
    pub changed: Timestamp,
}

/// The owner and group a [`Change::Owner`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    /// The new owner (`i_uid`), or `None` to keep it.
    pub uid: Option<u32>,
    /// The new group (`i_gid`), or `None` to keep it.
    pub gid: Option<u32>,
    /// Whether to clear the set-user-ID and set-group-ID bits of a regular file, as POSIX
    /// `chown` does for callers without privilege.
    pub drop_set_id: bool,
    /// The new change time.
    pub changed: Timestamp,
}

/// The times [`set_times`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Times {
    /// The new access time (`i_atime`), or `None` to keep it.
    pub accessed: Option<Timestamp>,
    /// The new modification time (`i_mtime`), or `None` to keep it.
    pub modified: Option<Timestamp>,
    /// The new change time (`i_ctime`): setting times is itself a change to the inode.
    pub changed: Timestamp,
}

/// Sets the access, modification and change times of inode `number` in one journal
/// transaction.
///
/// # Errors
///
/// [`WriteError::NeedsReplay`] for a volume that was not cleanly unmounted,
/// [`WriteError::NoJournal`] and [`WriteError::UnsupportedFeature`] for volumes this module
/// does not write, [`WriteError::Journal`] for an inode number out of range or a volume or
/// journal that is malformed, [`WriteError::FreeInode`], [`WriteError::InodeChecksum`] for an
/// inode that is already damaged, [`WriteError::TimeOutOfRange`], and
/// [`WriteError::Transaction`] or [`WriteError::Device`] when the device fails. After a
/// device failure the change may or may not have been made; the journal is left for
/// [`crate::journal::replay`] to finish it.
///
/// # Examples
///
/// ```no_run
/// use ext4d::device::Window;
/// use ext4d::write::{self, Timestamp, Times};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let image = std::fs::OpenOptions::new().read(true).write(true).open("disk.ext4")?;
/// let mut window = Window::whole(image)?;
/// let now = Timestamp { seconds: 1_767_225_600, nanoseconds: 0 };
/// // Inode 12, the first file mke2fs makes.
/// write::set_times(&mut window, 12, &Times { accessed: Some(now), modified: None, changed: now })?;
/// # Ok(())
/// # }
/// ```
pub fn set_times<R: WriteAt>(
    window: &mut Window<R>,
    number: u32,
    times: &Times,
) -> Result<(), WriteError> {
    change(window, number, &Change::Times(*times))
}

/// Makes `change` to inode `number` in one journal transaction: the inode is read, checked
/// (in use, and passing its checksum), changed in memory with its checksum updated, logged,
/// committed and written home.
///
/// # Errors
///
/// Those of [`set_times`], and for the change itself: [`WriteError::TimeOutOfRange`] for a
/// time or change time that does not fit, [`WriteError::InvalidPermissions`] for mode bits
/// outside `0o7777`, [`WriteError::IdOutOfRange`] for an owner or group above 65535 on a
/// volume whose inodes have no high halves for them, and [`WriteError::QuotaTracked`] for a
/// new owner or group on a volume with `quota`.
///
/// # Examples
///
/// ```no_run
/// use ext4d::device::Window;
/// use ext4d::write::{self, Change, Mode, Timestamp};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let image = std::fs::OpenOptions::new().read(true).write(true).open("disk.ext4")?;
/// let mut window = Window::whole(image)?;
/// // chmod 0640 on inode 12, the first file mke2fs makes.
/// let mode = Mode { permissions: 0o640, changed: Timestamp::now() };
/// write::change(&mut window, 12, &Change::Mode(mode))?;
/// # Ok(())
/// # }
/// ```
pub fn change<R: WriteAt>(
    window: &mut Window<R>,
    number: u32,
    change: &Change,
) -> Result<(), WriteError> {
    let target = writable_volume(window)?;
    let volume = &target.volume;
    let map = BlockMap::load(volume, window)?;
    let (_, raw, _) = open_journal(&*window, &map, volume)?;
    let mut writer = Writer::new(&raw)?;

    let block_size = u64::from(volume.block_size);
    let at = volume.inode_offset(window, number)?;
    let (home, within) = (at / block_size, (at % block_size) as usize);
    let mut block = vec![0; volume.block_size as usize];
    window.read_exact_at(&mut block, home * block_size)?;
    let inode = &mut block[within..within + volume.inode_size as usize];
    check_inode(&target, number, inode)?;
    apply(&target, number, inode, change)?;
    if let Some(seed) = target.checksum_seed {
        let checksum = inode_checksum(seed, number, inode);
        put_checksum(inode, checksum);
    }

    let mut transaction = writer.transaction();
    transaction.log(home, &block)?;
    journal::set_recover_flag(window, true)?;
    let mut journal = Journal {
        window: &mut *window,
        map: &map,
        volume,
    };
    let committed = writer.commit(&mut journal, transaction, commit_time())?;
    writer.checkpoint(&mut journal, committed)?;
    journal::set_recover_flag(window, false)?;
    Ok(())
}

/// Checks that inode `number`, whose bytes are `inode`, is in use and passes its checksum,
/// before anything is written for it.
fn check_inode(target: &Target, number: u32, inode: &[u8]) -> Result<(), WriteError> {
    if le16(inode, I_MODE) == 0 {
        return Err(WriteError::FreeInode { number });
    }
    if let Some(seed) = target.checksum_seed {
        let (stored, computed) = (stored_checksum(inode), inode_checksum(seed, number, inode));
        if stored != computed {
            return Err(WriteError::InodeChecksum {
                number,
                stored,
                computed,
            });
        }
    }
    Ok(())
}

/// Makes `change` to the bytes of inode `number`, in memory.
fn apply(
    target: &Target,
    number: u32,
    inode: &mut [u8],
    change: &Change,
) -> Result<(), WriteError> {
    let changed = match change {
        Change::Times(times) => {
            put_time_of(inode, number, I_ATIME, I_ATIME_EXTRA, times.accessed)?;
            put_time_of(inode, number, I_MTIME, I_MTIME_EXTRA, times.modified)?;
            times.changed
        }
        Change::Mode(mode) => {
            put_permissions(inode, mode.permissions)?;
            mode.changed
        }
        Change::Owner(owner) => {
            put_owner(target, number, inode, owner)?;
            owner.changed
        }
        Change::Written(written) => {
            if let Some(size) = written.size {
                put_size(target, number, inode, size)?;
            }
            put_time_of(inode, number, I_MTIME, I_MTIME_EXTRA, Some(written.changed))?;
            written.changed
        }
    };
    put_time_of(inode, number, I_CTIME, I_CTIME_EXTRA, Some(changed))
}

/// Stores `time`, if there is one, as [`put_time`] does.
fn put_time_of(
    inode: &mut [u8],
    number: u32,
    field: usize,
    extra: usize,
    time: Option<Timestamp>,
) -> Result<(), WriteError> {
    let Some(time) = time else {
        return Ok(());
    };
    put_time(inode, field, extra, time).ok_or(WriteError::TimeOutOfRange {
        number,
        seconds: time.seconds,
        nanoseconds: time.nanoseconds,
    })
}

/// Replaces the permission bits of `i_mode` (`0o7777`: the set-ID and sticky bits and
/// read, write and execute for owner, group and others), keeping its file type.
fn put_permissions(inode: &mut [u8], permissions: u16) -> Result<(), WriteError> {
    if permissions & !PERMISSION_BITS != 0 {
        return Err(WriteError::InvalidPermissions { bits: permissions });
    }
    let mode = le16(inode, I_MODE) & !PERMISSION_BITS | permissions;
    inode[I_MODE..I_MODE + 2].copy_from_slice(&mode.to_le_bytes());
    Ok(())
}

/// The owner and group stored in the inode: the low halves, with the high halves when the
/// volume's inodes have them.
fn owner_of(target: &Target, inode: &[u8]) -> (u32, u32) {
    let id = |low, high| {
        let low = u32::from(le16(inode, low));
        if target.high_ids {
            u32::from(le16(inode, high)) << 16 | low
        } else {
            low
        }
    };
    (id(I_UID, I_UID_HIGH), id(I_GID, I_GID_HIGH))
}

/// Stores `owner`'s owner and group in the inode, and drops its set-ID bits if it asks.
fn put_owner(
    target: &Target,
    number: u32,
    inode: &mut [u8],
    owner: &Owner,
) -> Result<(), WriteError> {
    let old = owner_of(target, inode);
    let new = (owner.uid.unwrap_or(old.0), owner.gid.unwrap_or(old.1));
    if new != old && target.volume.ro_compat & RO_COMPAT_QUOTA != 0 {
        return Err(WriteError::QuotaTracked { number });
    }
    for (id, low, high) in [(new.0, I_UID, I_UID_HIGH), (new.1, I_GID, I_GID_HIGH)] {
        if target.high_ids {
            inode[high..high + 2].copy_from_slice(&((id >> 16) as u16).to_le_bytes());
        } else if id > 0xFFFF {
            return Err(WriteError::IdOutOfRange { number, id });
        }
        inode[low..low + 2].copy_from_slice(&(id as u16).to_le_bytes());
    }
    let mode = le16(inode, I_MODE);
    if owner.drop_set_id && mode & S_IFMT == S_IFREG {
        let mode = mode & !(S_ISUID | S_ISGID);
        inode[I_MODE..I_MODE + 2].copy_from_slice(&mode.to_le_bytes());
    }
    Ok(())
}

/// Writes `data` at byte `offset` of the regular file inode `number`, into the blocks it
/// already has, then records the write in its inode (a new size if it grew, and `changed`
/// as its modification and change time) in one journal transaction ([`Change::Written`]).
///
/// As in ext4's default ordered mode, the data is not journaled: it is written to its
/// blocks and flushed before the inode's transaction is logged, so after a crash the inode
/// never claims a size whose data is not on the disk. A crash before the commit leaves the
/// new data in place with the old size and times. Nothing is allocated: every block the
/// write touches must already be an initialized block of the file, and the new size and
/// times must fit in the inode, or nothing is written. An empty `data` writes nothing.
///
/// # Errors
///
/// Those of [`change`]; [`WriteError::NotAFile`] for anything but a regular file;
/// [`WriteError::FileFlags`] for an immutable or append-only file;
/// [`WriteError::UnsupportedFile`] for inline data, encryption, verity or journaled data;
/// [`WriteError::NotAllocated`] when a block the write touches is a hole, an unwritten
/// extent or past the file's blocks; [`WriteError::BadDataBlock`] for a block mapped
/// outside the volume or into the journal; and [`WriteError::FileTooLarge`].
///
/// # Examples
///
/// ```no_run
/// use ext4d::device::Window;
/// use ext4d::write::{self, Timestamp};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let image = std::fs::OpenOptions::new().read(true).write(true).open("disk.ext4")?;
/// let mut window = Window::whole(image)?;
/// // Overwrite the first bytes of inode 12, the first file mke2fs makes.
/// write::write_data(&mut window, 12, 0, b"HELLO", Timestamp::now())?;
/// # Ok(())
/// # }
/// ```
pub fn write_data<R: WriteAt>(
    window: &mut Window<R>,
    number: u32,
    offset: u64,
    data: &[u8],
    changed: Timestamp,
) -> Result<(), WriteError> {
    if data.is_empty() {
        return Ok(());
    }
    let target = writable_volume(window)?;
    let inode = target.volume.read_inode(window, number)?;
    check_inode(&target, number, &inode)?;
    check_data_writable(number, &inode)?;
    let end = offset
        .checked_add(data.len() as u64)
        .ok_or(WriteError::FileTooLarge {
            number,
            size: u64::MAX,
        })?;
    let size = u64::from(le32(&inode, I_SIZE_HIGH)) << 32 | u64::from(le32(&inode, I_SIZE_LO));
    let written = Written {
        size: (end > size).then_some(end),
        changed,
    };
    // A size or time the inode cannot hold must be refused before any data is written,
    // not after: the change that records them is made on a copy first.
    apply(
        &target,
        number,
        &mut inode.clone(),
        &Change::Written(written),
    )?;
    let blocks = data_blocks(&target.volume, window, number, &inode, offset, end)?;
    let block_size = u64::from(target.volume.block_size);
    let mut buf = vec![0; target.volume.block_size as usize];
    for (logical, physical) in blocks {
        // The part of this block the write covers, and where it is in `data`.
        let start = (logical * block_size).max(offset);
        let stop = ((logical + 1) * block_size).min(end);
        let within = (start - logical * block_size) as usize;
        let len = (stop - start) as usize;
        let from = (start - offset) as usize;
        let piece = &data[from..from + len];
        if len == buf.len() {
            window.write_all_at(piece, physical * block_size)?;
        } else {
            window.read_exact_at(&mut buf, physical * block_size)?;
            buf[within..within + len].copy_from_slice(piece);
            window.write_all_at(&buf, physical * block_size)?;
        }
    }
    window.flush()?;
    change(window, number, &Change::Written(written))
}

/// Checks that the data of inode `number` (`inode`) is one this module can write in place.
fn check_data_writable(number: u32, inode: &[u8]) -> Result<(), WriteError> {
    if le16(inode, I_MODE) & S_IFMT != S_IFREG {
        return Err(WriteError::NotAFile { number });
    }
    let flags = le32(inode, I_FLAGS);
    let forbidden = flags & (IMMUTABLE_FL | APPEND_FL);
    if forbidden != 0 {
        return Err(WriteError::FileFlags {
            number,
            flags: forbidden,
        });
    }
    let unsupported = flags & (ENCRYPT_FL | JOURNAL_DATA_FL | VERITY_FL | INLINE_DATA_FL);
    if unsupported != 0 {
        return Err(WriteError::UnsupportedFile {
            number,
            flags: unsupported,
        });
    }
    Ok(())
}

/// The volume block of each block of inode `number` that bytes `offset..end` touch, by
/// block within the file, all checked before anything is written.
fn data_blocks<R: ReadAt>(
    volume: &Volume,
    window: &Window<R>,
    number: u32,
    inode: &[u8],
    offset: u64,
    end: u64,
) -> Result<Vec<(u64, u64)>, WriteError> {
    let block_size = u64::from(volume.block_size);
    let map = BlockMap::of_inode(volume, window, inode, number)?;
    let journal = BlockMap::load(volume, window)?;
    (offset / block_size..end.div_ceil(block_size))
        .map(|logical| {
            let physical = u32::try_from(logical)
                .ok()
                .and_then(|block| map.physical(block))
                .ok_or(WriteError::NotAllocated {
                    number,
                    block: logical,
                })?;
            if physical >= volume.blocks || journal.contains(physical) {
                return Err(WriteError::BadDataBlock { number, physical });
            }
            Ok((logical, physical))
        })
        .collect()
}

/// Stores `size` in `i_size_lo` and `i_size_high`.
fn put_size(target: &Target, number: u32, inode: &mut [u8], size: u64) -> Result<(), WriteError> {
    if size > i32::MAX as u64 && target.volume.ro_compat & RO_COMPAT_LARGE_FILE == 0 {
        return Err(WriteError::FileTooLarge { number, size });
    }
    inode[I_SIZE_LO..I_SIZE_LO + 4].copy_from_slice(&(size as u32).to_le_bytes());
    inode[I_SIZE_HIGH..I_SIZE_HIGH + 4].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
    Ok(())
}

/// Checks that this module can write to the volume `window` holds, without writing: what
/// every change checks first, for deciding at mount time whether to serve the volume
/// read-write.
///
/// # Errors
///
/// The refusals of [`set_times`] that depend on the volume, not the inode:
/// [`WriteError::NeedsReplay`], [`WriteError::NoJournal`], [`WriteError::UnsupportedFeature`]
/// and [`WriteError::Journal`]; and [`WriteError::Open`] for a journal the writer refuses
/// (one that is aborted or has version 1 checksums).
pub fn check_writable<R: ReadAt>(window: &Window<R>) -> Result<(), WriteError> {
    let target = writable_volume(window)?;
    let map = BlockMap::load(&target.volume, window)?;
    let (_, raw, _) = open_journal(window, &map, &target.volume)?;
    Writer::new(&raw)?;
    Ok(())
}

/// What a change needs to know about the volume.
struct Target {
    /// The superblock fields.
    volume: Volume,
    /// What metadata checksums start from (with `metadata_csum`): `s_checksum_seed`, or the
    /// CRC32C of the UUID.
    checksum_seed: Option<u32>,
    /// Whether inodes keep the high 16 bits of the owner and group, at `l_i_uid_high` and
    /// `l_i_gid_high` (`s_creator_os` Linux) or `h_i_uid_high` and `h_i_gid_high` (Hurd), at
    /// the same offsets. Other creators keep other fields there.
    high_ids: bool,
}

/// The volume's [`Target`], once it is known that this module may write to it.
fn writable_volume<R: ReadAt>(window: &Window<R>) -> Result<Target, WriteError> {
    let sb = read_volume_superblock(window)?;
    let volume = Volume::parse(&sb)?;
    if volume.needs_replay()? {
        return Err(WriteError::NeedsReplay);
    }
    if volume.compat & COMPAT_HAS_JOURNAL == 0 || volume.journal_inode == 0 {
        return Err(WriteError::NoJournal);
    }
    let checks = [
        ("incompat", volume.incompat, INCOMPAT_WRITABLE),
        ("ro_compat", volume.ro_compat, RO_COMPAT_WRITABLE),
    ];
    for (field, flags, writable) in checks {
        if flags & !writable != 0 {
            return Err(WriteError::UnsupportedFeature {
                field,
                flags: flags & !writable,
            });
        }
    }
    let seed = (volume.ro_compat & RO_COMPAT_METADATA_CSUM != 0).then(|| {
        if volume.incompat & INCOMPAT_CSUM_SEED != 0 {
            le32(&sb, S_CHECKSUM_SEED)
        } else {
            ferric_jbd2::crc32c(&[&sb[S_UUID..S_UUID + 16]])
        }
    });
    let high_ids = matches!(le32(&sb, S_CREATOR_OS), CREATOR_LINUX | CREATOR_HURD);
    Ok(Target {
        volume,
        checksum_seed: seed,
        high_ids,
    })
}

/// The commit block's time: now, or the epoch if the clock is before it.
fn commit_time() -> CommitTime {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    CommitTime {
        seconds: now.as_secs(),
        nanoseconds: now.subsec_nanos(),
    }
}

/// Stores `time` in the seconds field at `field` and, if the inode has it, the extra field
/// at `extra`; `None` when it does not fit.
///
/// The seconds field holds the low 32 bits of the seconds, read as signed. The extra field's
/// low two bits (the epoch) add multiples of 2^32 to that, and its upper 30 bits hold the
/// nanoseconds.
fn put_time(inode: &mut [u8], field: usize, extra: usize, time: Timestamp) -> Option<()> {
    if time.nanoseconds >= 1_000_000_000 {
        return None;
    }
    // The low 32 bits, as the signed value the field is read as.
    let low = time.seconds as i32;
    let epoch = (time.seconds - i64::from(low)) >> 32;
    if has_extra(inode, extra + 4) {
        if !(0..4).contains(&epoch) {
            return None;
        }
        let encoded = time.nanoseconds << 2 | epoch as u32;
        inode[extra..extra + 4].copy_from_slice(&encoded.to_le_bytes());
    } else if epoch != 0 {
        return None;
    }
    inode[field..field + 4].copy_from_slice(&low.to_le_bytes());
    Some(())
}

/// The checksum stored in the inode: both halves, or the low one without `i_checksum_hi`.
fn stored_checksum(inode: &[u8]) -> u32 {
    let low = u32::from(le16(inode, I_CHECKSUM_LO));
    if has_extra(inode, I_CHECKSUM_HI + 2) {
        u32::from(le16(inode, I_CHECKSUM_HI)) << 16 | low
    } else {
        low
    }
}

/// The inode checksum, as [`stored_checksum`] reads it: CRC32C from the volume's seed over
/// the inode number, `i_generation` and the inode with its checksum fields zero.
fn inode_checksum(seed: u32, number: u32, inode: &[u8]) -> u32 {
    let hi = has_extra(inode, I_CHECKSUM_HI + 2);
    let mut copy = inode.to_vec();
    copy[I_CHECKSUM_LO..I_CHECKSUM_LO + 2].fill(0);
    if hi {
        copy[I_CHECKSUM_HI..I_CHECKSUM_HI + 2].fill(0);
    }
    let crc = ferric_jbd2::crc32c_seeded(
        seed,
        &[
            &number.to_le_bytes(),
            &inode[I_GENERATION..I_GENERATION + 4],
            &copy,
        ],
    );
    if hi { crc } else { crc & 0xFFFF }
}

/// Stores `checksum` (from [`inode_checksum`]) in the inode.
fn put_checksum(inode: &mut [u8], checksum: u32) {
    inode[I_CHECKSUM_LO..I_CHECKSUM_LO + 2].copy_from_slice(&(checksum as u16).to_le_bytes());
    if has_extra(inode, I_CHECKSUM_HI + 2) {
        let high = (checksum >> 16) as u16;
        inode[I_CHECKSUM_HI..I_CHECKSUM_HI + 2].copy_from_slice(&high.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inode::{I_EXTRA_ISIZE, time as read_time};

    fn inode(extra_isize: u16) -> Vec<u8> {
        let mut inode = vec![0; 256];
        inode[I_EXTRA_ISIZE..I_EXTRA_ISIZE + 2].copy_from_slice(&extra_isize.to_le_bytes());
        inode
    }

    fn time(seconds: i64, nanoseconds: u32) -> Timestamp {
        Timestamp {
            seconds,
            nanoseconds,
        }
    }

    #[test]
    fn only_plain_regular_files_take_data() {
        let mut inode = vec![0; 256];
        inode[I_MODE..I_MODE + 2].copy_from_slice(&(S_IFREG | 0o644).to_le_bytes());
        check_data_writable(12, &inode).expect("a regular file");
        for (flags, refused) in [
            (IMMUTABLE_FL, true),
            (APPEND_FL, true),
            (ENCRYPT_FL, false),
            (JOURNAL_DATA_FL, false),
            (VERITY_FL, false),
            (INLINE_DATA_FL, false),
        ] {
            inode[I_FLAGS..I_FLAGS + 4].copy_from_slice(&(0x8_0000 | flags).to_le_bytes());
            let err = check_data_writable(12, &inode).expect_err("refused");
            if refused {
                assert!(matches!(err, WriteError::FileFlags { flags: f, .. } if f == flags));
            } else {
                assert!(matches!(err, WriteError::UnsupportedFile { flags: f, .. } if f == flags));
            }
        }
        inode[I_FLAGS..I_FLAGS + 4].fill(0);
        inode[I_MODE..I_MODE + 2].copy_from_slice(&0o40_755u16.to_le_bytes());
        assert!(matches!(
            check_data_writable(2, &inode),
            Err(WriteError::NotAFile { number: 2 })
        ));
    }

    #[test]
    fn put_permissions_keeps_the_file_type() {
        let mut inode = vec![0; 256];
        inode[I_MODE..I_MODE + 2].copy_from_slice(&(S_IFREG | 0o644).to_le_bytes());
        put_permissions(&mut inode, 0o4750).expect("permission bits");
        assert_eq!(le16(&inode, I_MODE), S_IFREG | 0o4750);
        assert!(matches!(
            put_permissions(&mut inode, S_IFREG | 0o644),
            Err(WriteError::InvalidPermissions { bits }) if bits == S_IFREG | 0o644
        ));
        assert_eq!(
            le16(&inode, I_MODE),
            S_IFREG | 0o4750,
            "a refused mode writes nothing"
        );
    }

    #[test]
    fn times_take_the_epoch_bits_and_nanoseconds_in_the_extra_field() {
        let mut inode = inode(32);
        // From 1901-12-13 (i32::MIN) to 2446-05-10 (i32::MAX in epoch 3).
        for (seconds, low, epoch) in [
            (0, 0, 0),
            (-1, -1, 0),
            (i64::from(i32::MIN), i32::MIN, 0),
            (1 << 31, i32::MIN, 1),
            ((1 << 32) + 5, 5, 1),
            (3 * (1 << 32) + i64::from(i32::MAX), i32::MAX, 3),
        ] {
            put_time(
                &mut inode,
                I_MTIME,
                I_MTIME_EXTRA,
                time(seconds, 999_999_999),
            )
            .expect("fits");
            assert_eq!(le32(&inode, I_MTIME) as i32, low, "{seconds}");
            let extra = le32(&inode, I_MTIME_EXTRA);
            assert_eq!((extra & 3, extra >> 2), (epoch, 999_999_999), "{seconds}");
            let decoded = i64::from(low) + (i64::from(extra & 3) << 32);
            assert_eq!(decoded, seconds);
            // What crate::inode reads back, for fstat's change time.
            assert_eq!(
                read_time(&inode, I_MTIME, I_MTIME_EXTRA),
                (seconds, 999_999_999)
            );
        }
        assert!(put_time(&mut inode, I_MTIME, I_MTIME_EXTRA, time(1 << 34, 0)).is_none());
        assert!(
            put_time(
                &mut inode,
                I_MTIME,
                I_MTIME_EXTRA,
                time(i64::from(i32::MIN) - 1, 0)
            )
            .is_none()
        );
        assert!(put_time(&mut inode, I_MTIME, I_MTIME_EXTRA, time(0, 1_000_000_000)).is_none());
    }

    #[test]
    fn without_the_extra_fields_times_are_32_bit_seconds() {
        for mut inode in [vec![0; 128], inode(4)] {
            put_time(&mut inode, I_ATIME, I_ATIME_EXTRA, time(-5, 7)).expect("fits");
            assert_eq!(le32(&inode, I_ATIME) as i32, -5);
            assert!(inode[128..].iter().skip(4).all(|&b| b == 0));
            assert!(put_time(&mut inode, I_ATIME, I_ATIME_EXTRA, time(1 << 31, 0)).is_none());
        }
    }

    #[test]
    fn the_checksum_is_cut_to_16_bits_without_its_high_half() {
        let mut small = vec![0x5Au8; 128];
        let checksum = inode_checksum(1, 12, &small);
        assert!(checksum <= 0xFFFF);
        put_checksum(&mut small, checksum);
        assert_eq!(stored_checksum(&small), checksum);
        let mut large = inode(32);
        let checksum = inode_checksum(1, 12, &large);
        put_checksum(&mut large, checksum);
        assert_eq!(stored_checksum(&large), checksum);
        // The checksum fields do not count, so storing it does not change it.
        assert_eq!(inode_checksum(1, 12, &large), checksum);
        assert_ne!(inode_checksum(1, 13, &large), checksum);
    }
}
