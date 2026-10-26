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
//! The one operation so far is [`set_times`], which sets an existing inode's access,
//! modification and change times. The scheme's `futimens` reaches it through
//! [`crate::fs::Fs::set_times`], which finds the inode with [`crate::lookup`].
//! [`check_writable`] tells, at mount time, whether a volume can be served read-write.
//!
//! # Specification
//!
//! Written from "Ext4 Disk Layout" on the ext4 wiki at kernel.org: "The Super Block"
//! (feature flags, `s_checksum_seed`), "Inode Table" (the fields and `i_extra_isize`),
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

/// `s_feature_incompat` bits that writing an inode's times is safe under: `filetype`,
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
    let (volume, checksum_seed) = writable_volume(window)?;
    let map = BlockMap::load(&volume, window)?;
    let (_, raw, _) = open_journal(&*window, &map, &volume)?;
    let mut writer = Writer::new(&raw)?;

    let block_size = u64::from(volume.block_size);
    let at = volume.inode_offset(window, number)?;
    let (target, within) = (at / block_size, (at % block_size) as usize);
    let mut block = vec![0; volume.block_size as usize];
    window.read_exact_at(&mut block, target * block_size)?;
    let inode = &mut block[within..within + volume.inode_size as usize];
    if le16(inode, I_MODE) == 0 {
        return Err(WriteError::FreeInode { number });
    }
    if let Some(seed) = checksum_seed {
        let (stored, computed) = (stored_checksum(inode), inode_checksum(seed, number, inode));
        if stored != computed {
            return Err(WriteError::InodeChecksum {
                number,
                stored,
                computed,
            });
        }
    }
    let changes = [
        (I_ATIME, I_ATIME_EXTRA, times.accessed),
        (I_MTIME, I_MTIME_EXTRA, times.modified),
        (I_CTIME, I_CTIME_EXTRA, Some(times.changed)),
    ];
    for (field, extra, time) in changes {
        if let Some(time) = time {
            put_time(inode, field, extra, time).ok_or(WriteError::TimeOutOfRange {
                number,
                seconds: time.seconds,
                nanoseconds: time.nanoseconds,
            })?;
        }
    }
    if let Some(seed) = checksum_seed {
        let checksum = inode_checksum(seed, number, inode);
        put_checksum(inode, checksum);
    }

    let mut transaction = writer.transaction();
    transaction.log(target, &block)?;
    journal::set_recover_flag(window, true)?;
    let mut journal = Journal {
        window: &mut *window,
        map: &map,
        volume: &volume,
    };
    let committed = writer.commit(&mut journal, transaction, commit_time())?;
    writer.checkpoint(&mut journal, committed)?;
    journal::set_recover_flag(window, false)?;
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
    let (volume, _) = writable_volume(window)?;
    let map = BlockMap::load(&volume, window)?;
    let (_, raw, _) = open_journal(window, &map, &volume)?;
    Writer::new(&raw)?;
    Ok(())
}

/// The volume's superblock fields, once it is known that this module may write to it, and
/// what its metadata checksums start from (with `metadata_csum`): `s_checksum_seed`, or the
/// CRC32C of the UUID.
fn writable_volume<R: ReadAt>(window: &Window<R>) -> Result<(Volume, Option<u32>), WriteError> {
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
    Ok((volume, seed))
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
