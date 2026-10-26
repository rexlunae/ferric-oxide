//! Writing through the journal (`write`, features.toml `fs.ext4_write`) on volumes made by
//! e2fsprogs: `NAME.replayed.ext4.gz` (clean volumes with every journal checksum kind, ext3
//! and 4 KiB blocks, `tests/fixtures/make-fixtures.sh`), and one of them given a
//! `metadata_csum_seed`.
//!
//! Each change is read back through `ext4-view`, which checks the inode checksum. When
//! `EXT4D_E2FSCK` names an `e2fsck` binary (CI sets it), every volume a test leaves, clean or
//! crashed part-way through a change, is also given to `e2fsck`: it must replay whatever the
//! journal holds, find nothing to fix, and leave the change made in full or not at all.

#![cfg(feature = "write")]

mod common;

use common::{Crashing, e2fsck, le32, load, reseal};
use ext4d::device::Window;
use ext4d::fs::{Fs, OpenMode};
use ext4d::journal::{self, JournalError, Outcome};
use ext4d::write::{self, Times, Timestamp, WriteError};

/// The clean volumes with a journal and `hello.txt` as inode 12.
const NAMES: [&str; 5] = [
    "journal-v3",
    "journal-v2",
    "journal-v2-64",
    "journal-ext3",
    "journal-4k",
];
/// `hello.txt`, the first inode `mke2fs -d` makes.
const HELLO: u32 = 12;

const ACCESSED: Timestamp = Timestamp {
    seconds: 0x7000_0000,
    nanoseconds: 123_456_789,
};
const MODIFIED: Timestamp = Timestamp {
    seconds: 0x7000_0001,
    nanoseconds: 5,
};
const CHANGED: Timestamp = Timestamp {
    seconds: 0x7000_0002,
    nanoseconds: 0,
};
const TIMES: Times = Times {
    accessed: Some(ACCESSED),
    modified: Some(MODIFIED),
    changed: CHANGED,
};

fn clean(name: &str) -> Vec<u8> {
    load(&format!("{name}.replayed.ext4.gz"))
}

fn set_times(image: Vec<u8>, inode: u32, times: &Times) -> (Result<(), WriteError>, Vec<u8>) {
    let mut window = Window::whole(image).expect("its length");
    let result = write::set_times(&mut window, inode, times);
    (result, window.into_inner())
}

/// The access and modification times of `path`, read through `ext4-view`.
fn times_of(image: Vec<u8>, path: &str) -> ((i64, u32), (i64, u32)) {
    let mut fs = Fs::new(Window::whole(image).expect("its length")).expect("an ext4 volume");
    let root = fs.root().expect("the root");
    let id = if path == "/" {
        root
    } else {
        fs.open_as(Some(root), path, OpenMode::read(), 0, 0)
            .expect("the file")
    };
    let attributes = fs.attributes(id).expect("open");
    (attributes.accessed, attributes.modified)
}

fn hello_contents(image: Vec<u8>) -> Vec<u8> {
    let mut fs = Fs::new(Window::whole(image).expect("its length")).expect("an ext4 volume");
    let root = fs.root().expect("the root");
    let file = fs
        .open_as(Some(root), "hello.txt", OpenMode::read(), 0, 0)
        .expect("hello.txt");
    let mut buf = [0; 64];
    let n = fs.read(file, 0, &mut buf).expect("readable");
    buf[..n].to_vec()
}

const NEW: ((i64, u32), (i64, u32)) = ((0x7000_0000, 123_456_789), (0x7000_0001, 5));

/// The 1 KiB blocks of the two images that differ.
fn differing(a: &[u8], b: &[u8]) -> Vec<usize> {
    a.chunks(1024)
        .zip(b.chunks(1024))
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn sets_an_inodes_times_through_the_journal() {
    for name in NAMES {
        let before = clean(name);
        let (result, after) = set_times(before.clone(), HELLO, &TIMES);
        result.unwrap_or_else(|err| panic!("{name}: {err:?}"));
        assert_eq!(times_of(after.clone(), "hello.txt"), NEW, "{name}");
        assert_eq!(
            hello_contents(after.clone()),
            b"journaled hello\n",
            "{name}"
        );
        // The volume superblock is as it was: needs_recovery was set and cleared again.
        assert_eq!(after[1024..2048], before[1024..2048], "{name}");
        let checked = e2fsck(name, after.clone());
        assert_eq!(times_of(checked, "hello.txt"), NEW, "{name}: after e2fsck");
    }
}

/// The byte offset of inode `number` in a volume whose inode is in group 0, from the
/// superblock and the first group descriptor ("Finding an Inode" in "Ext4 Disk Layout").
fn inode_at(image: &[u8], number: u32) -> usize {
    let sb = &image[1024..2048];
    let block_size = 1024usize << le32(sb, 0x18);
    let first_data_block = le32(sb, 0x14) as usize;
    let inode_size = usize::from(u16::from_le_bytes([sb[0x58], sb[0x59]]));
    let descriptor = (first_data_block + 1) * block_size;
    let table = le32(image, descriptor + 0x8) as usize;
    table * block_size + (number as usize - 1) * inode_size
}

#[test]
fn only_the_inode_table_block_and_the_journal_change() {
    for name in NAMES {
        let before = clean(name);
        let (result, after) = set_times(before.clone(), HELLO, &TIMES);
        result.expect("written");
        let block_size = 1024 << le32(&before, 1024 + 0x18);
        let home = inode_at(&before, HELLO) / block_size;
        // The inode table block, and in the journal its superblock and the descriptor, copy
        // and commit block at the start of the log.
        let changed: Vec<usize> = differing(&before, &after)
            .into_iter()
            .map(|kib| kib * 1024 / block_size)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(changed.len(), 5, "{name}: {changed:?}");
        assert!(changed.contains(&home), "{name}");
        let home_bytes = &after[home * block_size..(home + 1) * block_size];
        let copies = changed
            .iter()
            .filter(|&&b| b != home && &after[b * block_size..(b + 1) * block_size] == home_bytes)
            .count();
        assert_eq!(copies, 1, "{name}: the log holds one copy of the new block");
    }
}

#[test]
fn keeps_the_times_it_is_not_given() {
    let before = clean("journal-v3");
    let (accessed, _) = times_of(before.clone(), "hello.txt");
    let times = Times {
        accessed: None,
        modified: Some(MODIFIED),
        changed: CHANGED,
    };
    let (result, after) = set_times(before, HELLO, &times);
    result.expect("written");
    assert_eq!(times_of(after, "hello.txt"), (accessed, (0x7000_0001, 5)));
}

/// `journal-v3` as `tune2fs -O metadata_csum_seed -U` would leave it: the old UUID's
/// checksum seed kept in `s_checksum_seed`, and a new UUID that no checksum uses.
fn with_checksum_seed(mut image: Vec<u8>) -> Vec<u8> {
    let sb = 1024;
    let seed = ferric_jbd2::crc32c(&[&image[sb + 0x68..sb + 0x78]]);
    image[sb + 0x270..sb + 0x274].copy_from_slice(&seed.to_le_bytes());
    let incompat = le32(&image, sb + 0x60) | 0x2000;
    image[sb + 0x60..sb + 0x64].copy_from_slice(&incompat.to_le_bytes());
    image[sb + 0x68..sb + 0x78].copy_from_slice(&[0x42; 16]);
    reseal(&mut image);
    image
}

#[test]
fn writes_under_metadata_csum_seed() {
    let before = with_checksum_seed(clean("journal-v3"));
    assert_eq!(hello_contents(before.clone()), b"journaled hello\n");
    let (result, after) = set_times(before, HELLO, &TIMES);
    result.expect("written");
    assert_eq!(times_of(after.clone(), "hello.txt"), NEW);
    let checked = e2fsck("csum-seed", after);
    assert_eq!(times_of(checked, "hello.txt"), NEW);
}

#[test]
fn times_past_2038_use_the_epoch_bits() {
    let late = Timestamp {
        seconds: 3 * (1 << 32) + 7,
        nanoseconds: 999_999_999,
    };
    let times = Times {
        accessed: Some(late),
        modified: Some(late),
        changed: late,
    };
    let (result, after) = set_times(clean("journal-v3"), HELLO, &times);
    result.expect("written");
    let expected = (late.seconds, late.nanoseconds);
    assert_eq!(times_of(after.clone(), "hello.txt"), (expected, expected));
    e2fsck("late", after);
}

/// A crash after any number of writes leaves the old times or the new ones, never a mix,
/// for ext4d's replay and for e2fsck's.
#[test]
fn a_crash_at_any_write_leaves_the_change_made_or_not() {
    let before = clean("journal-v3");
    let old = times_of(before.clone(), "hello.txt");
    // The superblock flag, the descriptor, copy and commit block, the journal superblock,
    // the block home, the journal superblock again, the flag again.
    for writes_left in 0..8 {
        let mut window = Window::whole(Crashing {
            bytes: before.clone(),
            writes_left,
        })
        .expect("its length");
        let err = write::set_times(&mut window, HELLO, &TIMES).expect_err("the device stops");
        assert!(
            matches!(err, WriteError::Journal(_) | WriteError::Transaction(_)),
            "{writes_left}: {err:?}"
        );
        let crashed = window.into_inner().bytes;
        // From the fifth write on, the journal points at the committed transaction; after
        // exactly five the block is not home yet, so only a replay of the transaction this
        // wrote gives the new times.
        let expected = if writes_left < 5 { old } else { NEW };
        let theirs = e2fsck(&format!("crash-{writes_left}"), crashed.clone());
        assert_eq!(
            times_of(theirs, "hello.txt"),
            expected,
            "{writes_left}: e2fsck"
        );
        let mut window = Window::whole(crashed).expect("its length");
        journal::replay(&mut window).expect("replayed");
        let ours = window.into_inner();
        assert_eq!(
            times_of(ours.clone(), "hello.txt"),
            expected,
            "{writes_left}"
        );
        // Once replayed, the volume takes writes again.
        let (result, again) = set_times(ours, HELLO, &TIMES);
        result.expect("written after the replay");
        assert_eq!(times_of(again, "hello.txt"), NEW, "{writes_left}");
    }
}

/// On a volume whose journal holds a committed change that is not home yet, a read-only
/// mount reads the change time as `ext4-view` reads the other times: with the journal's
/// blocks in place of their home blocks, not from the device as it is.
#[test]
fn an_unreplayed_change_time_is_read_through_the_journal() {
    let before = clean("journal-v3");
    // After five writes the transaction is committed and the journal points at it, but the
    // inode table block is not home yet.
    let mut window = Window::whole(Crashing {
        bytes: before.clone(),
        writes_left: 5,
    })
    .expect("its length");
    write::set_times(&mut window, HELLO, &TIMES).expect_err("the device stops");
    let crashed = window.into_inner().bytes;
    assert!(
        journal::needs_replay(&Window::whole(crashed.clone()).expect("its length"))
            .expect("readable")
    );
    // The device still holds the old inode.
    let at = inode_at(&crashed, HELLO);
    assert_eq!(crashed[at..at + 256], before[at..at + 256]);

    let mut fs = Fs::new(Window::whole(crashed).expect("its length")).expect("an ext4 volume");
    assert!(fs.needs_recovery());
    let root = fs.root().expect("the root");
    let hello = fs
        .open_as(Some(root), "hello.txt", OpenMode::read(), 0, 0)
        .expect("hello.txt");
    let attributes = fs.attributes(hello).expect("open");
    assert_eq!((attributes.accessed, attributes.modified), NEW);
    assert_eq!(attributes.changed, (CHANGED.seconds, CHANGED.nanoseconds));
}

#[test]
fn a_volume_that_needs_recovery_is_replayed_first() {
    let image = load("journal-v3.ext4.gz");
    let (result, after) = set_times(image.clone(), HELLO, &TIMES);
    assert!(matches!(result, Err(WriteError::NeedsReplay)), "{result:?}");
    assert_eq!(after, image, "nothing was written");
    let mut window = Window::whole(image).expect("its length");
    let outcome = journal::replay(&mut window).expect("replayed");
    assert!(matches!(outcome, Outcome::Replayed { .. }));
    let (result, after) = set_times(window.into_inner(), HELLO, &TIMES);
    result.expect("written");
    assert_eq!(times_of(after.clone(), "hello.txt"), NEW);
    assert_eq!(hello_contents(after), b"journaled hello\n");
}

fn refused(image: Vec<u8>, inode: u32, times: &Times) -> WriteError {
    let (result, after) = set_times(image.clone(), inode, times);
    assert_eq!(
        differing(&after, &image),
        [] as [usize; 0],
        "nothing was written"
    );
    result.expect_err("refused")
}

#[test]
fn free_and_out_of_range_inodes_are_refused() {
    let image = clean("journal-v3");
    let inodes = le32(&image, 1024);
    assert!(matches!(
        refused(image.clone(), HELLO + 1, &TIMES),
        WriteError::FreeInode { number: 13 }
    ));
    for number in [0, inodes + 1] {
        let err = refused(image.clone(), number, &TIMES);
        assert!(
            matches!(err, WriteError::Journal(JournalError::BadInode { number: n }) if n == number),
            "{number}: {err:?}"
        );
    }
}

#[test]
fn a_damaged_inode_is_not_rewritten() {
    let mut image = clean("journal-v3");
    // Flip a bit in its i_size.
    let inode_at = inode_at(&image, HELLO);
    image[inode_at + 0x4] ^= 1;
    assert!(matches!(
        refused(image, HELLO, &TIMES),
        WriteError::InodeChecksum { number: 12, .. }
    ));
}

#[test]
fn times_that_do_not_fit_are_refused() {
    for seconds in [1 << 34, i64::from(i32::MIN) - 1] {
        let times = Times {
            accessed: Some(Timestamp {
                seconds,
                nanoseconds: 0,
            }),
            modified: None,
            changed: CHANGED,
        };
        assert!(matches!(
            refused(clean("journal-v3"), HELLO, &times),
            WriteError::TimeOutOfRange { number: 12, .. }
        ));
    }
}

#[test]
fn volumes_this_cannot_write_are_refused() {
    // RO_COMPAT_READONLY: the volume asks not to be written.
    let mut readonly = clean("journal-v3");
    let at = 1024 + 0x64;
    let ro_compat = le32(&readonly, at) | 0x1000;
    readonly[at..at + 4].copy_from_slice(&ro_compat.to_le_bytes());
    reseal(&mut readonly);
    assert!(matches!(
        refused(readonly, HELLO, &TIMES),
        WriteError::UnsupportedFeature {
            field: "ro_compat",
            flags: 0x1000
        }
    ));
    // INCOMPAT_MMP: another host may have the volume mounted.
    let mut mmp = clean("journal-v3");
    let at = 1024 + 0x60;
    let incompat = le32(&mmp, at) | 0x100;
    mmp[at..at + 4].copy_from_slice(&incompat.to_le_bytes());
    reseal(&mut mmp);
    assert!(matches!(
        refused(mmp, HELLO, &TIMES),
        WriteError::UnsupportedFeature {
            field: "incompat",
            flags: 0x100
        }
    ));
    // INCOMPAT_META_BG: refused up front, as the journal map cannot be checked on it yet.
    let mut meta_bg = clean("journal-v3");
    let at = 1024 + 0x60;
    let incompat = le32(&meta_bg, at) | 0x10;
    meta_bg[at..at + 4].copy_from_slice(&incompat.to_le_bytes());
    reseal(&mut meta_bg);
    assert!(matches!(
        refused(meta_bg, HELLO, &TIMES),
        WriteError::UnsupportedFeature {
            field: "incompat",
            flags: 0x10
        }
    ));
    // No journal (COMPAT_HAS_JOURNAL clear).
    let mut unjournaled = clean("journal-v3");
    let at = 1024 + 0x5C;
    let compat = le32(&unjournaled, at) & !0x4;
    unjournaled[at..at + 4].copy_from_slice(&compat.to_le_bytes());
    reseal(&mut unjournaled);
    assert!(matches!(
        refused(unjournaled, HELLO, &TIMES),
        WriteError::NoJournal
    ));
}
