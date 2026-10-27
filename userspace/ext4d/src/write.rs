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
//! [`write_data`] writes a regular file's data (`write`), as ext4's ordered mode does: the
//! data goes home first, unjournaled, and the inode's new size, times and any blocks
//! allocated for holes or past the end then follow in one transaction. [`truncate`] sets a
//! file's size, freeing the blocks past the new end (`ftruncate`, `O_TRUNC`), and [`create`]
//! makes an empty regular file in a directory (`O_CREAT`). The scheme reaches them through
//! [`crate::fs::Fs::set_times`], [`crate::fs::Fs::set_mode`], [`crate::fs::Fs::set_owner`],
//! [`crate::fs::Fs::write`], [`crate::fs::Fs::truncate`] and [`crate::fs::Fs::open_as`],
//! which check the caller's permission as POSIX says and find the inode with
//! [`crate::lookup`].
//!
//! Allocation is split into three private modules: `txn` reads and changes the block and
//! inode bitmaps, the group descriptors and the superblock's free counts inside the one
//! transaction, with every checksum; `extents` maps and unmaps blocks in an inode's extent
//! tree (lengthening an extent, inserting one in a root or leaf with room, cutting the tree
//! back); and `dir` adds an entry to a linear directory, growing it by a block when no block
//! has room. The group descriptor, extent and directory records they read and write are
//! declared in `ondisk`, with `simple_endian`'s `#[derive(Endianize)]`. Not done yet: growing an extent tree by a level or splitting a leaf, converting
//! unwritten extents, block-mapped (ext3) files, hashed directories, holding the reserved
//! blocks back, and quota updates (so allocation and creation on a `quota` volume are
//! refused). [`check_writable`] tells, at mount time, whether a
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
//! "Inode Timestamps" (the epoch bits and nanoseconds in the `_extra` fields),
//! "Block Group Descriptors", "Block and inode Bitmaps", "Extent Tree", "Linear (Classic)
//! Directories" and "Checksums" (the inode checksum: CRC32C over the inode number,
//! `i_generation` and the whole inode with its checksum fields zero, from the volume's
//! checksum seed; and those of the descriptors, bitmaps, extent blocks and directory
//! blocks). The journal
//! format is `ferric-jbd2`'s. The inode checksum was checked against volumes made by
//! e2fsprogs 1.47's `mke2fs`, and what this module writes is checked with its `e2fsck`. No
//! code here comes from the Linux kernel or from e2fsprogs.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ferric_jbd2::{CommitTime, LogError, OpenError, Writer};
use thiserror::Error;

use crate::device::{ReadAt, Window, WindowError, WriteAt};
use crate::inode::data::Data;
use crate::inode::{
    GOOD_OLD_INODE_SIZE, I_ATIME, I_ATIME_EXTRA, I_CTIME, I_CTIME_EXTRA, I_EXTRA_ISIZE, I_MTIME,
    I_MTIME_EXTRA, has_extra, le16,
};
use crate::journal::map::{BlockMap, RO_COMPAT_METADATA_CSUM, Volume, le32};
use crate::journal::{
    self, BlockError, Journal, JournalError, open_journal, read_volume_superblock,
};
use crate::superblock::COMPAT_HAS_JOURNAL;

mod dir;
mod extents;
mod ondisk;
mod txn;

use txn::Txn;

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

/// `i_flags`, and the flags a data write checks.
const IMMUTABLE_FL: u32 = 0x10;
const APPEND_FL: u32 = 0x20;
const ENCRYPT_FL: u32 = 0x800;
const JOURNAL_DATA_FL: u32 = 0x4000;
const VERITY_FL: u32 = 0x10_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
/// `s_feature_ro_compat`: files may be 2^31 bytes or longer.
const RO_COMPAT_LARGE_FILE: u32 = 0x2;
/// `s_feature_ro_compat`: group descriptors have a CRC16 (without `metadata_csum`).
const RO_COMPAT_GDT_CSUM: u32 = 0x10;
/// `s_feature_ro_compat`: blocks are allocated in clusters, which this module does not do.
const RO_COMPAT_BIGALLOC: u32 = 0x200;
/// `s_feature_ro_compat`: inodes have a project ID (`i_projid`).
const RO_COMPAT_PROJECT: u32 = 0x2000;
/// `s_feature_incompat`: files may have extent trees, as every file this module makes does.
const INCOMPAT_EXTENTS: u32 = 0x40;
/// Where the superblock keeps `s_min_extra_isize`.
const S_MIN_EXTRA_ISIZE: usize = 0x15C;
/// `i_flags`: `i_block` holds an extent tree.
const EXTENTS_FL: u32 = 0x8_0000;
/// `i_flags`: `i_blocks` counts file-system blocks, not 512-byte sectors.
const HUGE_FILE_FL: u32 = 0x4_0000;
/// `i_blocks_lo` and `l_i_blocks_high`: the blocks the inode uses.
const I_BLOCKS_LO: usize = 0x1C;
const I_BLOCKS_HIGH: usize = 0x74;
/// `i_links_count`.
const I_LINKS_COUNT: usize = 0x1A;
/// `i_crtime` and `i_crtime_extra`: the creation time.
const I_CRTIME: usize = 0x90;
const I_CRTIME_EXTRA: usize = 0x94;
/// `i_projid`.
const I_PROJID: usize = 0x9C;
/// `i_extra_isize` for new inodes: the fields up to `i_projid`.
const NEW_EXTRA_ISIZE: u16 = 32;
/// The file type bits of a directory.
const S_IFDIR: u16 = 0x4000;
/// `ext4_dir_entry_2.file_type` of a regular file.
const FT_REG_FILE: u8 = 1;
/// The longest name a directory entry holds.
const NAME_MAX: usize = 255;

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
    /// An inode prefix could not be decoded or encoded.
    #[error("cannot encode or decode the inode data fields")]
    InodeEncoding(#[from] std::io::Error),
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
    /// A data write to a block of the file that is in an unwritten extent, which this
    /// module does not convert yet.
    #[error("block {block} of inode {number} is not allocated")]
    NotAllocated {
        /// The inode number.
        number: u32,
        /// The block within the file.
        block: u64,
    },
    /// The inode maps file data outside the volume, into the journal or into fixed metadata.
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
    /// A block would have to be allocated for, or freed from, a file whose blocks are not
    /// mapped by an extent tree (an ext2 or ext3 block map), which this module does not do.
    #[error("inode {number} has no extent tree")]
    NoExtents {
        /// The inode number.
        number: u32,
    },
    /// A new extent is needed in a tree leaf that is full: the tree would have to grow,
    /// which this module does not do yet.
    #[error("inode {number}'s extent tree has no room for another extent")]
    ExtentTreeFull {
        /// The inode number.
        number: u32,
    },
    /// The inode's extent tree is malformed.
    #[error("inode {number} has a malformed extent tree")]
    BadExtentTree {
        /// The inode number.
        number: u32,
    },
    /// No initialized block group has a free block or inode.
    #[error("no space left on the volume")]
    NoSpace,
    /// A block to read, allocate or free is outside the volume's groups, part of its fixed
    /// metadata or journal, or (to free) already free.
    #[error("block {block} cannot be used or freed")]
    BadBlock {
        /// The volume block.
        block: u64,
    },
    /// A group descriptor fails its checksum, so its group is not changed.
    #[error("group {group}'s descriptor has checksum {stored:#06x}, expected {computed:#06x}")]
    GroupChecksum {
        /// The group.
        group: u32,
        /// The value stored.
        stored: u32,
        /// The value computed.
        computed: u32,
    },
    /// A block or inode bitmap fails its checksum, so its group is not changed.
    #[error("group {group}'s bitmap has checksum {stored:#010x}, expected {computed:#010x}")]
    BitmapChecksum {
        /// The group.
        group: u32,
        /// The value stored.
        stored: u32,
        /// The value computed.
        computed: u32,
    },
    /// An extent tree or directory block fails its checksum, so it is not changed.
    #[error("block {block} has checksum {stored:#010x}, expected {computed:#010x}")]
    BlockChecksum {
        /// The volume block.
        block: u64,
        /// The value stored.
        stored: u32,
        /// The value computed.
        computed: u32,
    },
    /// A directory block holds an entry whose lengths do not fit it, or lacks its checksum
    /// tail.
    #[error("directory inode {number} is malformed at byte {offset} of block {block}")]
    BadDirectory {
        /// The directory's inode number.
        number: u32,
        /// The directory's logical block.
        block: u64,
        /// Where in the block.
        offset: usize,
    },
    /// A file would be created in a directory kept in a way this module does not add entries
    /// to: hashed, encrypted, inline-data, case-insensitive or without an extent tree.
    #[error("directory inode {number} is kept in a way ext4d does not add to (flags {flags:#x})")]
    UnsupportedDirectory {
        /// The directory's inode number.
        number: u32,
        /// The flags that say how (`0x80000`, the extent flag, when it is missing).
        flags: u32,
    },
    /// A file would be created in an inode that is not a directory.
    #[error("inode {number} is not a directory")]
    NotADirectory {
        /// The inode number.
        number: u32,
    },
    /// A file would be created under a name its directory already has.
    #[error("directory inode {directory} already has the name")]
    Exists {
        /// The directory's inode number.
        directory: u32,
    },
    /// A name that cannot be a directory entry: empty, `.`, `..`, or holding a `/` or a
    /// NUL.
    #[error("not a valid file name")]
    InvalidName,
    /// A name longer than the 255 bytes a directory entry holds.
    #[error("the name is longer than 255 bytes")]
    NameTooLong,
    /// The directory a file would be created in could not be searched.
    #[error("cannot search the directory")]
    Directory(#[from] crate::lookup::LookupError),
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
    let block_size = u64::from(volume.block_size);
    let at = volume.inode_offset(window, number)?;
    let (home, within) = (at / block_size, (at % block_size) as usize);
    let mut block = vec![0; volume.block_size as usize];
    window.read_exact_at(&mut block, home * block_size)?;
    let inode = &mut block[within..within + volume.inode_size as usize];
    check_inode(&target, number, inode)?;
    apply(&target, number, inode, change)?;
    seal(&target, number, inode);
    commit(window, &target, &map, BTreeMap::from([(home, block)]))
}

/// Writes `blocks` (volume block number to new contents) through the journal in one
/// transaction: `INCOMPAT_RECOVER` is set, the transaction logged and committed, the blocks
/// written home and the journal emptied, and `INCOMPAT_RECOVER` cleared.
fn commit<R: WriteAt>(
    window: &mut Window<R>,
    target: &Target,
    map: &BlockMap,
    blocks: BTreeMap<u64, Vec<u8>>,
) -> Result<(), WriteError> {
    let volume = &target.volume;
    let (_, raw, _) = open_journal(&*window, map, volume)?;
    let mut writer = Writer::new(&raw)?;
    let mut transaction = writer.transaction();
    for (home, block) in &blocks {
        transaction.log(*home, block)?;
    }
    journal::set_recover_flag(window, true)?;
    let mut journal = Journal {
        window: &mut *window,
        map,
        volume,
    };
    let committed = writer.commit(&mut journal, transaction, commit_time())?;
    writer.checkpoint(&mut journal, committed)?;
    journal::set_recover_flag(window, false)?;
    Ok(())
}

/// Stores inode `number`'s checksum in `inode`, on a volume with `metadata_csum`.
fn seal(target: &Target, number: u32, inode: &mut [u8]) {
    if let Some(seed) = target.checksum_seed {
        let checksum = inode_checksum(seed, number, inode);
        put_checksum(inode, checksum);
    }
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
    put_ids(target, number, inode, new)?;
    let mode = le16(inode, I_MODE);
    if owner.drop_set_id && mode & S_IFMT == S_IFREG {
        let mode = mode & !(S_ISUID | S_ISGID);
        inode[I_MODE..I_MODE + 2].copy_from_slice(&mode.to_le_bytes());
    }
    Ok(())
}

/// Stores the owner and group `ids` in the inode, with their high halves where the volume's
/// inodes have them.
fn put_ids(
    target: &Target,
    number: u32,
    inode: &mut [u8],
    ids: (u32, u32),
) -> Result<(), WriteError> {
    for (id, low, high) in [(ids.0, I_UID, I_UID_HIGH), (ids.1, I_GID, I_GID_HIGH)] {
        if target.high_ids {
            inode[high..high + 2].copy_from_slice(&((id >> 16) as u16).to_le_bytes());
        } else if id > 0xFFFF {
            return Err(WriteError::IdOutOfRange { number, id });
        }
        inode[low..low + 2].copy_from_slice(&(id as u16).to_le_bytes());
    }
    Ok(())
}

/// Writes `data` at byte `offset` of the regular file inode `number`, then records the write
/// in its inode (a new size if it grew, and `changed` as its modification and change time),
/// in one journal transaction.
///
/// As in ext4's default ordered mode, the data is not journaled: it is written to its
/// blocks and flushed before the inode's transaction is logged, so after a crash the inode
/// never claims a size or a block whose data is not on the disk. A crash before the commit
/// leaves the old size, blocks and times; new data may then sit in blocks that are still
/// free, or past the old end of the file, where nothing reads it.
///
/// Blocks of the file that the write touches and that it does not have yet (past its end or
/// in a hole) are allocated, as close after the file's last block as there is room, and
/// mapped in its extent tree, in the same transaction as the bitmaps, group descriptors,
/// superblock counts and inode that record them. The bytes of a newly allocated block that
/// the write does not cover are zero, and so are the bytes between the old end of the file
/// and `offset` in the block the old end is in. Blocks between that block and the write are
/// left as a hole. Everything is checked, and every block allocated, before any data is
/// written: a write that cannot be made writes nothing. An empty `data` writes nothing.
///
/// # Errors
///
/// Those of [`change`]; [`WriteError::NotAFile`] for anything but a regular file;
/// [`WriteError::FileFlags`] for an immutable or append-only file;
/// [`WriteError::UnsupportedFile`] for inline data, encryption, verity, journaled data or
/// (when blocks are allocated) huge-file block counts; [`WriteError::NotAllocated`] for a
/// block in an unwritten extent; [`WriteError::BadDataBlock`] for a block mapped outside
/// the volume, into the journal or into fixed metadata; [`WriteError::FileTooLarge`]. When blocks are allocated:
/// [`WriteError::NoExtents`] for a file without an extent tree,
/// [`WriteError::ExtentTreeFull`] when its leaf has no room for a new extent,
/// [`WriteError::NoSpace`], [`WriteError::QuotaTracked`] on a volume with `quota`,
/// [`WriteError::UnsupportedFeature`] on one with `bigalloc`, and
/// [`WriteError::BadExtentTree`], [`WriteError::BadBlock`], [`WriteError::GroupChecksum`],
/// [`WriteError::BitmapChecksum`] and [`WriteError::BlockChecksum`] for damaged metadata.
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
    let size = size_of(&inode)?;
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
    let journal = BlockMap::load(&target.volume, window)?;
    let span = Span { offset, end, size };
    let plan = plan_write(&*window, &target, &journal, number, &inode, span, written)?;
    put_data(window, &target, &plan.blocks, span, data)?;
    zero_gap(window, &target, &plan.gap, size)?;
    window.flush()?;
    match plan.transaction {
        None => change(window, number, &Change::Written(written)),
        Some(transaction) => commit(window, &target, &journal, transaction),
    }
}

/// The bytes a write covers (`offset..end`) and the file's size before it.
#[derive(Clone, Copy)]
struct Span {
    offset: u64,
    end: u64,
    size: u64,
}

/// The blocks one transaction writes: each volume block's new contents.
type Blocks = BTreeMap<u64, Vec<u8>>;

/// Where a block the write touches goes: a block the file has, or one allocated for it.
#[derive(Clone, Copy)]
struct Placed {
    logical: u64,
    physical: u64,
    new: bool,
}

/// What a write does, all of it checked before anything is written.
struct Plan {
    /// Every block the write touches.
    blocks: Vec<Placed>,
    /// The blocks the file keeps between its old end and the first block the write
    /// touches, whose bytes past the old end must be zeroed.
    gap: Vec<Placed>,
    /// The transaction, when blocks are allocated.
    transaction: Option<Blocks>,
}

/// Finds or allocates every block the write touches, and finds the blocks between the old
/// end and the write. When any is allocated, the inode's whole transaction (the extent
/// tree, bitmaps, descriptors, superblock and the inode with `written` applied) comes back
/// with them; otherwise the caller makes the inode change.
fn plan_write<R: ReadAt>(
    window: &Window<R>,
    target: &Target,
    journal: &BlockMap,
    number: u32,
    inode: &[u8],
    span: Span,
    written: Written,
) -> Result<Plan, WriteError> {
    let block_size = u64::from(target.volume.block_size);
    let range = span.offset / block_size..span.end.div_ceil(block_size);
    let mut txn = Txn::new(window, target, journal);
    let gap = gap_blocks(&mut txn, target, number, inode, span)?;
    let found = find_blocks(&mut txn, target, number, inode, range)?;
    if found.iter().all(|(_, physical)| physical.is_some()) {
        let blocks = found
            .into_iter()
            .map(|(logical, physical)| Placed {
                logical,
                physical: physical.unwrap_or_default(),
                new: false,
            })
            .collect();
        return Ok(Plan {
            blocks,
            gap,
            transaction: None,
        });
    }
    check_allocatable(target, number, inode)?;
    let mut inode = txn.inode(number)?;
    let mut goal = goal_for(&mut txn, target, number, &inode, found[0].0)?;
    let mut placed = Vec::with_capacity(found.len());
    let mut allocated = 0;
    for (logical, physical) in found {
        let (physical, new) = match physical {
            Some(physical) => (physical, false),
            None => {
                let physical = txn.allocate_block(goal)?;
                let block = u32::try_from(logical).map_err(|_| WriteError::FileTooLarge {
                    number,
                    size: span.end,
                })?;
                extents::map(&mut txn, number, &mut inode, block, physical)?;
                allocated += 1;
                (physical, true)
            }
        };
        goal = physical + 1;
        placed.push(Placed {
            logical,
            physical,
            new,
        });
    }
    apply(target, number, &mut inode, &Change::Written(written))?;
    add_blocks(&mut inode, block_size, allocated);
    seal(target, number, &mut inode);
    txn.put_inode(number, &inode)?;
    Ok(Plan {
        blocks: placed,
        gap,
        transaction: Some(txn.finish()?),
    })
}

/// Checks that file data of inode `number` may go to volume block `physical`: inside the
/// volume, and outside the journal and the fixed metadata.
fn data_block<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    target: &Target,
    number: u32,
    physical: u64,
) -> Result<u64, WriteError> {
    if physical >= target.volume.blocks || txn.reserved(physical)? {
        return Err(WriteError::BadDataBlock { number, physical });
    }
    Ok(physical)
}

/// The blocks inode `number` has from the one holding its old end up to (not including)
/// the first block the write touches. A file can keep blocks past its end (after a
/// shrink), and their old bytes must read as zeros once the new size covers them; holes
/// and unwritten extents already do.
fn gap_blocks<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    target: &Target,
    number: u32,
    inode: &[u8],
    span: Span,
) -> Result<Vec<Placed>, WriteError> {
    let block_size = u64::from(target.volume.block_size);
    let range = span.size / block_size..span.offset / block_size;
    if range.is_empty() {
        return Ok(Vec::new());
    }
    let mut mapped = Vec::new();
    if flags_of(inode)? & EXTENTS_FL == 0 {
        // A block map is read only up to the old size: of the gap, it can have just the
        // block holding the old end.
        let map = txn.block_map(number, inode)?;
        if let Some(physical) = u32::try_from(range.start)
            .ok()
            .and_then(|block| map.physical(block))
        {
            mapped.push((range.start, physical));
        }
    } else {
        for extent in extents::leaves(txn, number, inode)? {
            let start = u64::from(extent.logical);
            let first = start.max(range.start);
            let last = (start + u64::from(extent.len)).min(range.end);
            if extent.unwritten || first >= last {
                continue;
            }
            mapped
                .extend((first..last).map(|logical| (logical, extent.physical + logical - start)));
        }
    }
    mapped
        .into_iter()
        .map(|(logical, physical)| {
            Ok(Placed {
                logical,
                physical: data_block(txn, target, number, physical)?,
                new: false,
            })
        })
        .collect()
}

/// The volume block of each block in `range` of inode `number`, or `None` for one it does
/// not have yet (a hole or past its end).
fn find_blocks<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    target: &Target,
    number: u32,
    inode: &[u8],
    range: std::ops::Range<u64>,
) -> Result<Vec<(u64, Option<u64>)>, WriteError> {
    if flags_of(inode)? & EXTENTS_FL == 0 {
        // A block map: what it has is written, and a block it lacks needs an extent tree.
        let window_map = txn.block_map(number, inode)?;
        return range
            .map(|logical| {
                let physical = u32::try_from(logical)
                    .ok()
                    .and_then(|block| window_map.physical(block))
                    .ok_or(WriteError::NoExtents { number })?;
                Ok((logical, Some(data_block(txn, target, number, physical)?)))
            })
            .collect();
    }
    let extents = extents::leaves(txn, number, inode)?;
    range
        .map(|logical| {
            let covering = extents.iter().find(|extent| {
                let start = u64::from(extent.logical);
                (start..start + u64::from(extent.len)).contains(&logical)
            });
            match covering {
                None => Ok((logical, None)),
                Some(extent) if extent.unwritten => Err(WriteError::NotAllocated {
                    number,
                    block: logical,
                }),
                Some(extent) => {
                    let physical = extent.physical + (logical - u64::from(extent.logical));
                    Ok((logical, Some(data_block(txn, target, number, physical)?)))
                }
            }
        })
        .collect()
}

/// Checks that blocks can be allocated for inode `number` on this volume.
fn check_allocatable(target: &Target, number: u32, inode: &[u8]) -> Result<(), WriteError> {
    let volume = &target.volume;
    if volume.ro_compat & RO_COMPAT_QUOTA != 0 {
        return Err(WriteError::QuotaTracked { number });
    }
    if volume.ro_compat & RO_COMPAT_BIGALLOC != 0 {
        return Err(WriteError::UnsupportedFeature {
            field: "ro_compat",
            flags: RO_COMPAT_BIGALLOC,
        });
    }
    if flags_of(inode)? & HUGE_FILE_FL != 0 {
        return Err(WriteError::UnsupportedFile {
            number,
            flags: HUGE_FILE_FL,
        });
    }
    Ok(())
}

/// Where to look for the first new block of inode `number`, the one for block `logical`:
/// after the extent before it, or at the start of the inode's group.
fn goal_for<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    target: &Target,
    number: u32,
    inode: &[u8],
    logical: u64,
) -> Result<u64, WriteError> {
    let extents = extents::leaves(txn, number, inode)?;
    let before = extents
        .iter()
        .rev()
        .find(|extent| u64::from(extent.logical) < logical);
    Ok(match before {
        Some(extent) => extent.physical + u64::from(extent.len),
        None => {
            let volume = &target.volume;
            let group = u64::from((number - 1) / volume.inodes_per_group);
            u64::from(volume.first_data_block) + group * u64::from(volume.blocks_per_group)
        }
    })
}

/// Writes the data into its blocks: whole new blocks as they are, others read first, with
/// the bytes of a new block the write does not cover, and those between the old end of the
/// file and the write, zero.
fn put_data<R: WriteAt>(
    window: &mut Window<R>,
    target: &Target,
    blocks: &[Placed],
    span: Span,
    data: &[u8],
) -> Result<(), WriteError> {
    let block_size = u64::from(target.volume.block_size);
    let mut buf = vec![0; target.volume.block_size as usize];
    for block in blocks {
        let base = block.logical * block_size;
        // The part of this block the write covers, and where it is in `data`.
        let start = base.max(span.offset);
        let stop = (base + block_size).min(span.end);
        let (within, len) = ((start - base) as usize, (stop - start) as usize);
        let from = (start - span.offset) as usize;
        let piece = &data[from..from + len];
        let at = block.physical * block_size;
        if len == buf.len() {
            window.write_all_at(piece, at)?;
            continue;
        }
        if block.new {
            buf.fill(0);
        } else {
            window.read_exact_at(&mut buf, at)?;
            // Bytes past the old end that the write skips over must read as zeros.
            let gap_start = base.max(span.size);
            let gap_end = (base + block_size).min(span.offset);
            if gap_start < gap_end {
                buf[(gap_start - base) as usize..(gap_end - base) as usize].fill(0);
            }
        }
        buf[within..within + len].copy_from_slice(piece);
        window.write_all_at(&buf, at)?;
    }
    Ok(())
}

/// Zeroes the bytes of each gap block past the old end of the file, `size`.
fn zero_gap<R: WriteAt>(
    window: &mut Window<R>,
    target: &Target,
    gap: &[Placed],
    size: u64,
) -> Result<(), WriteError> {
    let block_size = u64::from(target.volume.block_size);
    let mut buf = vec![0; target.volume.block_size as usize];
    for block in gap {
        let base = block.logical * block_size;
        let at = block.physical * block_size;
        let within = (size.max(base) - base) as usize;
        if within > 0 {
            window.read_exact_at(&mut buf, at)?;
        }
        buf[within..].fill(0);
        window.write_all_at(&buf, at)?;
    }
    Ok(())
}

/// Zeroes the bytes of volume block `physical` past byte `size % block_size`: the end of a
/// file whose last block it is.
fn zero_tail<R: WriteAt>(
    window: &mut Window<R>,
    target: &Target,
    physical: u64,
    size: u64,
) -> Result<(), WriteError> {
    let block_size = u64::from(target.volume.block_size);
    let within = (size % block_size) as usize;
    let mut buf = vec![0; target.volume.block_size as usize];
    window.read_exact_at(&mut buf, physical * block_size)?;
    buf[within..].fill(0);
    window.write_all_at(&buf, physical * block_size)?;
    Ok(())
}

/// Sets the size of the regular file inode `number` to `size` (`ftruncate`, and `O_TRUNC`
/// with a size of 0), with `changed` as its modification and change time, in one journal
/// transaction.
///
/// A smaller size frees the blocks past the new end, and the extent tree nodes left empty;
/// the bytes past the new end in its last block are zeroed once the transaction is
/// committed. A larger size allocates nothing: the file reads as zeros up to it, and the
/// bytes past the old end in its last block are zeroed first, before the transaction.
///
/// # Errors
///
/// Those of [`write_data`] that do not concern data: the volume's and the inode's,
/// [`WriteError::NotAFile`], [`WriteError::FileFlags`], [`WriteError::UnsupportedFile`],
/// [`WriteError::FileTooLarge`] and [`WriteError::TimeOutOfRange`]; and, when blocks are
/// freed, [`WriteError::NoExtents`] for a file without an extent tree,
/// [`WriteError::QuotaTracked`] on a volume with `quota`, and the damaged-metadata errors.
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
/// // Empty inode 12, as O_TRUNC does.
/// write::truncate(&mut window, 12, 0, Timestamp::now())?;
/// # Ok(())
/// # }
/// ```
pub fn truncate<R: WriteAt>(
    window: &mut Window<R>,
    number: u32,
    size: u64,
    changed: Timestamp,
) -> Result<(), WriteError> {
    let target = writable_volume(window)?;
    let inode = target.volume.read_inode(window, number)?;
    check_inode(&target, number, &inode)?;
    check_data_writable(number, &inode)?;
    let written = Written {
        size: Some(size),
        changed,
    };
    apply(
        &target,
        number,
        &mut inode.clone(),
        &Change::Written(written),
    )?;
    let old = size_of(&inode)?;
    let journal = BlockMap::load(&target.volume, window)?;
    let map = BlockMap::of_inode(&target.volume, window, &inode, number)?;
    let block_size = u64::from(target.volume.block_size);
    // The block holding the end of the shorter of the two sizes, if it is not full.
    let short = old.min(size);
    let tail = (!short.is_multiple_of(block_size))
        .then(|| u32::try_from(short / block_size).ok())
        .flatten()
        .and_then(|block| map.physical(block));
    let transaction = {
        let mut txn = Txn::new(&*window, &target, &journal);
        let mut inode = txn.inode(number)?;
        if size < old {
            let keep = u32::try_from(size.div_ceil(block_size))
                .map_err(|_| WriteError::FileTooLarge { number, size })?;
            let freed = free_from(&mut txn, &target, number, &mut inode, &map, keep)?;
            remove_blocks(&mut inode, block_size, freed);
        }
        apply(&target, number, &mut inode, &Change::Written(written))?;
        seal(&target, number, &mut inode);
        txn.put_inode(number, &inode)?;
        txn.finish()?
    };
    if let Some(tail) = tail
        && size > old
    {
        zero_tail(window, &target, tail, old)?;
        window.flush()?;
    }
    commit(window, &target, &journal, transaction)?;
    if let Some(tail) = tail
        && size < old
    {
        zero_tail(window, &target, tail, size)?;
        window.flush()?;
    }
    Ok(())
}

/// Frees inode `number`'s blocks from block `keep` on; how many volume blocks were freed.
fn free_from<R: ReadAt>(
    txn: &mut Txn<'_, R>,
    target: &Target,
    number: u32,
    inode: &mut [u8],
    map: &BlockMap,
    keep: u32,
) -> Result<u64, WriteError> {
    if flags_of(inode)? & EXTENTS_FL == 0 {
        // Nothing to free from a block map is fine; anything else needs an extent tree.
        let any = (keep..map.len.max(keep)).any(|block| map.physical(block).is_some());
        return if any {
            Err(WriteError::NoExtents { number })
        } else {
            Ok(0)
        };
    }
    let extents = extents::leaves(txn, number, inode)?;
    if extents.iter().all(|extent| extent.end() <= u64::from(keep)) {
        return Ok(0);
    }
    check_allocatable(target, number, inode)?;
    extents::truncate(txn, number, inode, keep)
}

/// What a file [`create`] makes has: its permission bits, owner, group and times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewFile {
    /// The permission bits (`0o7777`), with the caller's umask already applied.
    pub permissions: u16,
    /// The owner.
    pub uid: u32,
    /// The group.
    pub gid: u32,
    /// The access, modification, change and creation time, which the directory also takes
    /// as its modification and change time.
    pub created: Timestamp,
}

/// Creates an empty regular file named `name` in directory `parent`, in one journal
/// transaction, and returns its inode number.
///
/// An inode is allocated, in the directory's group or the first one after it with a free
/// inode, and written as an empty file with an extent tree, one link, `file`'s mode, owner,
/// group and times, and (with `project`) the directory's project. The entry goes into the
/// first directory block with room for it; when none has, a block is allocated and added to
/// the directory. The directory's modification and change times become `file.created`.
///
/// # Errors
///
/// [`WriteError::InvalidName`], [`WriteError::NameTooLong`], [`WriteError::InvalidPermissions`],
/// [`WriteError::IdOutOfRange`] and [`WriteError::TimeOutOfRange`] for what cannot be
/// stored; [`WriteError::NotADirectory`] when `parent` is not one;
/// [`WriteError::Exists`] when it has the name already; [`WriteError::UnsupportedDirectory`]
/// for a hashed, encrypted, inline-data, case-insensitive or block-mapped directory;
/// [`WriteError::QuotaTracked`] on a volume with `quota`; [`WriteError::NoExtents`] on one
/// without extents; [`WriteError::NoSpace`]; [`WriteError::Directory`] when the directory
/// cannot be searched; and those of [`change`] for the volume and the directory's inode.
///
/// # Examples
///
/// ```no_run
/// use ext4d::device::Window;
/// use ext4d::lookup::ROOT_INODE;
/// use ext4d::write::{self, NewFile, Timestamp};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let image = std::fs::OpenOptions::new().read(true).write(true).open("disk.ext4")?;
/// let mut window = Window::whole(image)?;
/// let file = NewFile { permissions: 0o644, uid: 1000, gid: 100, created: Timestamp::now() };
/// let number = write::create(&mut window, ROOT_INODE, b"new.txt", &file)?;
/// # let _ = number;
/// # Ok(())
/// # }
/// ```
pub fn create<R: WriteAt>(
    window: &mut Window<R>,
    parent: u32,
    name: &[u8],
    file: &NewFile,
) -> Result<u32, WriteError> {
    if name.len() > NAME_MAX {
        return Err(WriteError::NameTooLong);
    }
    let valid = !name.is_empty()
        && name != b"."
        && name != b".."
        && !name.iter().any(|&b| b == b'/' || b == 0);
    if !valid {
        return Err(WriteError::InvalidName);
    }
    if file.permissions & !PERMISSION_BITS != 0 {
        return Err(WriteError::InvalidPermissions {
            bits: file.permissions,
        });
    }
    let target = writable_volume(window)?;
    let volume = &target.volume;
    if volume.ro_compat & RO_COMPAT_QUOTA != 0 {
        return Err(WriteError::QuotaTracked { number: parent });
    }
    if volume.incompat & INCOMPAT_EXTENTS == 0 {
        return Err(WriteError::NoExtents { number: parent });
    }
    let dir = volume.read_inode(window, parent)?;
    check_inode(&target, parent, &dir)?;
    if le16(&dir, I_MODE) & S_IFMT != S_IFDIR {
        return Err(WriteError::NotADirectory { number: parent });
    }
    let filetype = volume.incompat & crate::lookup::INCOMPAT_FILETYPE != 0;
    if crate::lookup::find(volume, window, parent, name, filetype)?.is_some() {
        return Err(WriteError::Exists { directory: parent });
    }
    let journal = BlockMap::load(volume, window)?;
    let (number, transaction) = {
        let mut txn = Txn::new(&*window, &target, &journal);
        let mut dir = txn.inode(parent)?;
        let number = txn.allocate_inode((parent - 1) / volume.inodes_per_group)?;
        let mut inode = new_inode(&target, number, &dir, file)?;
        let entry = dir::NewEntry {
            name,
            inode: number,
            file_type: if filetype { FT_REG_FILE } else { 0 },
            filetype,
        };
        dir::add_entry(&mut txn, parent, &mut dir, &entry)?;
        for field in [(I_MTIME, I_MTIME_EXTRA), (I_CTIME, I_CTIME_EXTRA)] {
            put_time_of(&mut dir, parent, field.0, field.1, Some(file.created))?;
        }
        seal(&target, parent, &mut dir);
        txn.put_inode(parent, &dir)?;
        seal(&target, number, &mut inode);
        txn.put_inode(number, &inode)?;
        (number, txn.finish()?)
    };
    commit(window, &target, &journal, transaction)?;
    Ok(number)
}

/// The bytes of new inode `number`: an empty regular file with `file`'s attributes, in
/// directory `dir`. Its checksum is left for the caller.
fn new_inode(
    target: &Target,
    number: u32,
    dir: &[u8],
    file: &NewFile,
) -> Result<Vec<u8>, WriteError> {
    let size = target.volume.inode_size as usize;
    let mut inode = vec![0; size];
    if size > GOOD_OLD_INODE_SIZE {
        let room = (size - GOOD_OLD_INODE_SIZE) as u16;
        let extra = NEW_EXTRA_ISIZE.max(target.min_extra_isize).min(room);
        inode[I_EXTRA_ISIZE..I_EXTRA_ISIZE + 2].copy_from_slice(&extra.to_le_bytes());
    }
    let mode = S_IFREG | file.permissions;
    inode[I_MODE..I_MODE + 2].copy_from_slice(&mode.to_le_bytes());
    put_ids(target, number, &mut inode, (file.uid, file.gid))?;
    for (field, extra) in [
        (I_ATIME, I_ATIME_EXTRA),
        (I_MTIME, I_MTIME_EXTRA),
        (I_CTIME, I_CTIME_EXTRA),
        (I_CRTIME, I_CRTIME_EXTRA),
    ] {
        if field == I_CRTIME && !has_extra(&inode, I_CRTIME_EXTRA + 4) {
            continue;
        }
        put_time_of(&mut inode, number, field, extra, Some(file.created))?;
    }
    inode[I_LINKS_COUNT..I_LINKS_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());
    Data::read(&inode)?.put_flags(&mut inode, EXTENTS_FL)?;
    extents::init_root(&mut inode);
    let generation =
        (file.created.nanoseconds ^ (file.created.seconds as u32)).rotate_left(7) ^ number;
    inode[I_GENERATION..I_GENERATION + 4].copy_from_slice(&generation.to_le_bytes());
    let projects = target.volume.ro_compat & RO_COMPAT_PROJECT != 0;
    if projects && has_extra(&inode, I_PROJID + 4) && has_extra(dir, I_PROJID + 4) {
        inode[I_PROJID..I_PROJID + 4].copy_from_slice(&dir[I_PROJID..I_PROJID + 4]);
    }
    Ok(inode)
}

/// The size in `i_size_lo` and `i_size_high`.
fn size_of(inode: &[u8]) -> Result<u64, WriteError> {
    Ok(Data::read(inode)?.size())
}

/// The flags in `i_flags`.
fn flags_of(inode: &[u8]) -> Result<u32, WriteError> {
    Ok(Data::read(inode)?.flags)
}

/// The blocks counted in `i_blocks_lo` and `l_i_blocks_high`, in 512-byte units.
fn blocks_of(inode: &[u8]) -> u64 {
    u64::from(le16(inode, I_BLOCKS_HIGH)) << 32 | u64::from(le32(inode, I_BLOCKS_LO))
}

fn put_blocks(inode: &mut [u8], blocks: u64) {
    inode[I_BLOCKS_LO..I_BLOCKS_LO + 4].copy_from_slice(&(blocks as u32).to_le_bytes());
    let high = (blocks >> 32) as u16;
    inode[I_BLOCKS_HIGH..I_BLOCKS_HIGH + 2].copy_from_slice(&high.to_le_bytes());
}

/// Counts `count` more volume blocks of `block_size` bytes in the inode's `i_blocks`.
fn add_blocks(inode: &mut [u8], block_size: u64, count: u64) {
    put_blocks(inode, blocks_of(inode) + count * (block_size / 512));
}

/// Counts `count` fewer volume blocks in the inode's `i_blocks`.
fn remove_blocks(inode: &mut [u8], block_size: u64, count: u64) {
    put_blocks(
        inode,
        blocks_of(inode).saturating_sub(count * (block_size / 512)),
    );
}

/// Checks that the data of inode `number` (`inode`) is one this module can write in place.
fn check_data_writable(number: u32, inode: &[u8]) -> Result<(), WriteError> {
    if le16(inode, I_MODE) & S_IFMT != S_IFREG {
        return Err(WriteError::NotAFile { number });
    }
    let flags = Data::read(inode)?.flags;
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

/// Stores `size` in `i_size_lo` and `i_size_high`.
fn put_size(target: &Target, number: u32, inode: &mut [u8], size: u64) -> Result<(), WriteError> {
    if size > i32::MAX as u64 && target.volume.ro_compat & RO_COMPAT_LARGE_FILE == 0 {
        return Err(WriteError::FileTooLarge { number, size });
    }
    store_size(inode, size)
}

/// Stores `size` in `i_size_lo` and `i_size_high`, unchecked: a directory's, which may use
/// both halves without `large_file`.
fn store_size(inode: &mut [u8], size: u64) -> Result<(), WriteError> {
    Data::read(inode)?.put_size(inode, size)?;
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
    /// Whether group descriptors have a CRC16 (`gdt_csum`, without `metadata_csum`).
    gdt_csum: bool,
    /// `s_uuid`, which the CRC16 starts with.
    uuid: [u8; 16],
    /// `s_min_extra_isize`: the least `i_extra_isize` a new inode may have.
    min_extra_isize: u16,
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
    let gdt_csum = seed.is_none() && volume.ro_compat & RO_COMPAT_GDT_CSUM != 0;
    let mut uuid = [0; 16];
    uuid.copy_from_slice(&sb[S_UUID..S_UUID + 16]);
    Ok(Target {
        volume,
        checksum_seed: seed,
        high_ids,
        gdt_csum,
        uuid,
        min_extra_isize: le16(&sb, S_MIN_EXTRA_ISIZE),
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
    const I_FLAGS: usize = 0x20;

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
