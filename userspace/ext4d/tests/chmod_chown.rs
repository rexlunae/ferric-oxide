//! `fchmod` and `fchown` through the file system layer (`write`, features.toml
//! `fs.ext4_write`): [`Fs::set_mode`] and [`Fs::set_owner`] on volumes made by e2fsprogs
//! (`tests/fixtures/make-fixtures.sh`), checked byte for byte in the inode, through
//! `ext4-view` (which checks the inode checksum) and, when `EXT4D_E2FSCK` names an `e2fsck`
//! binary (CI sets it), by `e2fsck`.

#![cfg(feature = "write")]

mod common;

use common::{Crashing, SharedImage, e2fsck, inode_at, le16, load, load_raw, reseal};
use ext4d::Error;
use ext4d::device::Window;
use ext4d::fs::{Attributes, Fs, OpenMode};
use ext4d::journal;
use ext4d::lookup;
use ext4d::write::{self, Change, Mode, Timestamp, WriteError};

/// `i_mode`, `i_uid`, `i_gid`, and the high halves of the owner and group (Linux).
const I_MODE: usize = 0x0;
const I_UID: usize = 0x2;
const I_GID: usize = 0x18;
const I_UID_HIGH: usize = 0x78;
const I_GID_HIGH: usize = 0x7A;
/// Where the volume superblock keeps `s_creator_os`, from the start of the image.
const S_CREATOR_OS: usize = 1024 + 0x48;
const S_IFREG: u16 = 0o100_000;

fn basic() -> Vec<u8> {
    load_raw("basic.ext4")
}

/// A volume mounted for changes, and the device it was given.
fn mount_writable(image: Vec<u8>) -> (Fs, SharedImage) {
    let shared = SharedImage::new(image);
    let fs = Fs::new_writable(Window::whole(shared.clone()).expect("its length"))
        .expect("a writable volume");
    (fs, shared)
}

/// Opens `path` from the root as UID 0, with `mode`.
fn open_with(fs: &mut Fs, path: &str, mode: OpenMode) -> usize {
    let root = fs.root().expect("the root");
    fs.open_as(Some(root), path, mode, 0, 0).expect("the path")
}

fn open(fs: &mut Fs, path: &str) -> usize {
    open_with(fs, path, OpenMode::read())
}

/// The attributes `ext4-view` reads at `path` on a fresh mount of `image`.
fn attributes_at(image: Vec<u8>, path: &str) -> Attributes {
    let mut fs = Fs::new(Window::whole(image).expect("its length")).expect("an ext4 volume");
    let id = open(&mut fs, path);
    fs.attributes(id).expect("open")
}

/// The bytes of the inode `path` names.
fn inode(image: &[u8], path: &str) -> Vec<u8> {
    let window = Window::whole(image.to_vec()).expect("its length");
    let number = lookup::inode_of(&window, path).expect("the path");
    let at = inode_at(image, number);
    let size = usize::from(le16(image, 1024 + 0x58));
    image[at..at + size].to_vec()
}

#[test]
fn fchmod_sets_the_permission_bits_and_the_change_time() {
    let (mut fs, shared) = mount_writable(basic());
    let hello = open(&mut fs, "hello.txt");
    let before = fs.attributes(hello).expect("open");
    let start = Timestamp::now().seconds;
    // The file type bits of the argument are ignored; the inode's are kept.
    fs.set_mode(hello, 0o040_604, 0, 0).expect("set");
    let after = fs.attributes(hello).expect("open");
    assert_eq!(after.mode, S_IFREG | 0o604);
    assert!(after.changed.0 >= start, "{:?}", after.changed);
    assert_eq!(
        (after.accessed, after.modified),
        (before.accessed, before.modified)
    );
    let image = shared.bytes();
    assert_eq!(le16(&inode(&image, "/hello.txt"), I_MODE), S_IFREG | 0o604);
    let image = e2fsck("fchmod", image);
    let fresh = attributes_at(image, "/hello.txt");
    assert_eq!(
        (fresh.mode, fresh.changed),
        (S_IFREG | 0o604, after.changed)
    );
}

#[test]
fn only_the_owner_or_uid_0_changes_the_mode() {
    // dir/private.txt: mode 0640, owner 1000, group 100.
    let (mut fs, shared) = mount_writable(basic());
    let private = open(&mut fs, "dir/private.txt");
    let untouched = shared.bytes();
    assert!(matches!(
        fs.set_mode(private, 0o777, 1001, 100),
        Err(Error::NotOwner)
    ));
    assert!(shared.bytes() == untouched);
    fs.set_mode(private, 0o600, 1000, 100).expect("the owner");
    assert_eq!(fs.attributes(private).expect("open").mode, S_IFREG | 0o600);
    fs.set_mode(private, 0o4644, 0, 0).expect("UID 0");
    assert_eq!(fs.attributes(private).expect("open").mode, S_IFREG | 0o4644);
    e2fsck("fchmod-owner", shared.bytes());
}

#[test]
fn an_owner_outside_the_files_group_cannot_set_set_group_id() {
    let (mut fs, shared) = mount_writable(basic());
    let private = open(&mut fs, "dir/private.txt");
    let mode = |fs: &Fs| fs.attributes(private).expect("open").mode;
    fs.set_mode(private, 0o2640, 1000, 7).expect("the owner");
    assert_eq!(mode(&fs), S_IFREG | 0o640, "not in group 100");
    fs.set_mode(private, 0o2640, 1000, 100).expect("the owner");
    assert_eq!(mode(&fs), S_IFREG | 0o2640, "in group 100");
    fs.set_mode(private, 0o2750, 0, 0).expect("UID 0");
    assert_eq!(mode(&fs), S_IFREG | 0o2750);
    // A directory's set-group-ID bit is kept: it says what group new entries get.
    let dir = open(&mut fs, "dir/private-dir");
    fs.set_mode(dir, 0o2700, 1000, 7).expect("the owner");
    assert_eq!(fs.attributes(dir).expect("open").mode & 0o7777, 0o2700);
    e2fsck("fchmod-sgid", shared.bytes());
}

#[test]
fn a_symbolic_link_opened_itself_has_no_mode_to_change() {
    let (mut fs, shared) = mount_writable(basic());
    let untouched = shared.bytes();
    let link = open_with(
        &mut fs,
        "link",
        OpenMode {
            symlink: true,
            ..OpenMode::read()
        },
    );
    assert!(matches!(
        fs.set_mode(link, 0o600, 0, 0),
        Err(Error::SymlinkMode)
    ));
    assert!(shared.bytes() == untouched);
    // Through the link, the target's mode changes.
    let target = open(&mut fs, "link");
    fs.set_mode(target, 0o600, 0, 0).expect("set");
    assert_eq!(
        attributes_at(shared.bytes(), "/hello.txt").mode,
        S_IFREG | 0o600
    );
}

#[test]
fn uid_0_sets_any_owner_and_group_with_their_high_halves() {
    let (mut fs, shared) = mount_writable(basic());
    let hello = open(&mut fs, "hello.txt");
    fs.set_owner(hello, Some(70_000), Some(0x1234_5678), 0, 0)
        .expect("set");
    let a = fs.attributes(hello).expect("open");
    assert_eq!((a.uid, a.gid), (70_000, 0x1234_5678));
    let image = shared.bytes();
    let raw = inode(&image, "/hello.txt");
    assert_eq!(
        [
            le16(&raw, I_UID),
            le16(&raw, I_UID_HIGH),
            le16(&raw, I_GID),
            le16(&raw, I_GID_HIGH)
        ],
        [
            (70_000 & 0xFFFF) as u16,
            (70_000_u32 >> 16) as u16,
            0x5678,
            0x1234
        ]
    );
    let image = e2fsck("fchown-high", image);
    let fresh = attributes_at(image, "/hello.txt");
    assert_eq!((fresh.uid, fresh.gid), (70_000, 0x1234_5678));
}

#[test]
fn an_id_left_out_is_kept() {
    let (mut fs, _) = mount_writable(basic());
    let private = open(&mut fs, "dir/private.txt");
    fs.set_owner(private, None, Some(5), 0, 0).expect("set");
    let a = fs.attributes(private).expect("open");
    assert_eq!((a.uid, a.gid), (1000, 5));
    fs.set_owner(private, Some(6), None, 0, 0).expect("set");
    let a = fs.attributes(private).expect("open");
    assert_eq!((a.uid, a.gid), (6, 5));
    // Neither: only the change time moves.
    let start = Timestamp::now().seconds;
    fs.set_owner(private, None, None, 0, 0).expect("set");
    let after = fs.attributes(private).expect("open");
    assert_eq!((after.uid, after.gid), (6, 5));
    assert!(after.changed.0 >= start);
}

#[test]
fn other_callers_cannot_give_a_file_away() {
    // dir/private.txt: owner 1000, group 100.
    let (mut fs, shared) = mount_writable(basic());
    let private = open(&mut fs, "dir/private.txt");
    let untouched = shared.bytes();
    assert!(matches!(
        fs.set_owner(private, None, Some(100), 1001, 100),
        Err(Error::NotOwner)
    ));
    assert!(matches!(
        fs.set_owner(private, Some(1001), None, 1000, 100),
        Err(Error::ChownRestricted)
    ));
    assert!(matches!(
        fs.set_owner(private, None, Some(200), 1000, 7),
        Err(Error::ChownRestricted)
    ));
    assert!(shared.bytes() == untouched);
    // Its own owner, its own group or the caller's group are allowed.
    fs.set_owner(private, Some(1000), Some(100), 1000, 7)
        .expect("unchanged");
    fs.set_owner(private, None, Some(7), 1000, 7)
        .expect("the caller's group");
    let a = fs.attributes(private).expect("open");
    assert_eq!((a.uid, a.gid), (1000, 7));
    e2fsck("fchown-restricted", shared.bytes());
}

#[test]
fn an_unprivileged_fchown_clears_set_id_bits_of_a_regular_file() {
    let (mut fs, shared) = mount_writable(basic());
    let private = open(&mut fs, "dir/private.txt");
    let mode = |fs: &Fs| fs.attributes(private).expect("open").mode & 0o7777;
    fs.set_mode(private, 0o6750, 0, 0).expect("UID 0");
    fs.set_owner(private, None, Some(100), 0, 0).expect("UID 0");
    assert_eq!(mode(&fs), 0o6750, "UID 0 keeps them");
    fs.set_owner(private, None, Some(100), 1000, 100)
        .expect("the owner");
    assert_eq!(mode(&fs), 0o750);
    fs.set_mode(private, 0o6640, 0, 0).expect("UID 0");
    fs.set_owner(private, None, None, 1000, 100)
        .expect("the owner");
    assert_eq!(mode(&fs), 0o640, "with or without an execute bit");
    // A directory keeps its set-group-ID bit.
    let dir = open(&mut fs, "dir/private-dir");
    fs.set_mode(dir, 0o2700, 0, 0).expect("UID 0");
    fs.set_owner(dir, None, None, 1000, 100).expect("the owner");
    assert_eq!(fs.attributes(dir).expect("open").mode & 0o7777, 0o2700);
    e2fsck("fchown-set-id", shared.bytes());
}

#[test]
fn a_symbolic_link_opened_itself_gets_its_own_owner() {
    let (mut fs, shared) = mount_writable(basic());
    let link = open_with(
        &mut fs,
        "link",
        OpenMode {
            symlink: true,
            ..OpenMode::read()
        },
    );
    fs.set_owner(link, Some(42), Some(43), 0, 0).expect("set");
    let image = shared.bytes();
    let raw = inode(&image, "/link");
    assert_eq!((le16(&raw, I_UID), le16(&raw, I_GID)), (42, 43));
    assert!(inode(&image, "/hello.txt") == inode(&basic(), "/hello.txt"));
    e2fsck("fchown-symlink", image);
}

#[test]
fn a_quota_volume_takes_a_new_mode_but_not_a_new_owner() {
    let (mut fs, shared) = mount_writable(load("quota.ext4.gz"));
    let hello = open(&mut fs, "hello.txt");
    let untouched = shared.bytes();
    for (uid, gid) in [(Some(5), None), (None, Some(5))] {
        assert!(matches!(
            fs.set_owner(hello, uid, gid, 0, 0),
            Err(Error::Write(WriteError::QuotaTracked { number: 12 }))
        ));
    }
    assert!(shared.bytes() == untouched);
    assert_eq!(
        Error::Write(WriteError::QuotaTracked { number: 12 }).errno(),
        95
    );
    // The same owner and group move no usage.
    fs.set_owner(hello, Some(0), Some(0), 0, 0)
        .expect("unchanged");
    fs.set_mode(hello, 0o600, 0, 0)
        .expect("a mode is not counted");
    e2fsck("quota", shared.bytes());
}

#[test]
fn ids_above_65535_need_the_inodes_high_halves() {
    // s_creator_os 3 (FreeBSD): its inodes keep other fields where Linux keeps the high
    // halves.
    let mut image = basic();
    image[S_CREATOR_OS..S_CREATOR_OS + 4].copy_from_slice(&3u32.to_le_bytes());
    reseal(&mut image);
    let before = inode(&image, "/hello.txt");
    let (mut fs, shared) = mount_writable(image);
    let hello = open(&mut fs, "hello.txt");
    let number = lookup::inode_of(
        &Window::whole(shared.bytes()).expect("its length"),
        "/hello.txt",
    )
    .expect("the path");
    for (uid, gid, id) in [(Some(65_536), None, 65_536), (None, Some(70_000), 70_000)] {
        let result = fs.set_owner(hello, uid, gid, 0, 0);
        assert!(
            matches!(
                result,
                Err(Error::Write(WriteError::IdOutOfRange { number: n, id: got }))
                    if (n, got) == (number, id)
            ),
            "{result:?}"
        );
    }
    fs.set_owner(hello, Some(65_535), Some(500), 0, 0)
        .expect("fits");
    let after = inode(&shared.bytes(), "/hello.txt");
    assert_eq!((le16(&after, I_UID), le16(&after, I_GID)), (65_535, 500));
    assert_eq!(
        after[I_UID_HIGH..I_GID_HIGH + 2],
        before[I_UID_HIGH..I_GID_HIGH + 2]
    );
}

#[test]
fn a_read_only_mount_refuses_both() {
    let mut fs = Fs::new(Window::whole(basic()).expect("its length")).expect("an ext4 volume");
    let hello = open(&mut fs, "hello.txt");
    assert!(matches!(
        fs.set_mode(hello, 0o600, 0, 0),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(
        fs.set_owner(hello, Some(1), None, 0, 0),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(
        fs.set_mode(99, 0o600, 0, 0),
        Err(Error::BadHandle { handle: 99 })
    ));
}

#[test]
fn mode_bits_outside_the_permissions_are_refused() {
    let mut window = Window::whole(load("journal-v3.replayed.ext4.gz")).expect("its length");
    let mode = Mode {
        permissions: 0o10_000,
        changed: Timestamp::now(),
    };
    assert!(matches!(
        write::change(&mut window, 12, &Change::Mode(mode)),
        Err(WriteError::InvalidPermissions { bits: 0o10_000 })
    ));
}

#[test]
fn a_crash_at_any_write_leaves_the_mode_changed_or_not() {
    let before = load("journal-v3.replayed.ext4.gz");
    let mode = Mode {
        permissions: 0o4711,
        changed: Timestamp {
            seconds: 0x7000_0002,
            nanoseconds: 0,
        },
    };
    let mode_of = |image: Vec<u8>| attributes_at(image, "/hello.txt").mode;
    let old = mode_of(before.clone());
    // As for futimens (tests/write.rs): the superblock flag, the descriptor, copy and
    // commit block, the journal superblock, the block home, the journal superblock again,
    // the flag again.
    for writes_left in 0..8 {
        let mut window = Window::whole(Crashing {
            bytes: before.clone(),
            writes_left,
        })
        .expect("its length");
        write::change(&mut window, 12, &Change::Mode(mode)).expect_err("the device stops");
        let crashed = window.into_inner().bytes;
        let expected = if writes_left < 5 {
            old
        } else {
            S_IFREG | 0o4711
        };
        let theirs = e2fsck(&format!("chmod-crash-{writes_left}"), crashed.clone());
        assert_eq!(mode_of(theirs), expected, "{writes_left}: e2fsck");
        let mut window = Window::whole(crashed).expect("its length");
        journal::replay(&mut window).expect("replayed");
        assert_eq!(mode_of(window.into_inner()), expected, "{writes_left}");
    }
}
