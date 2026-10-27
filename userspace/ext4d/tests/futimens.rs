//! `futimens` through the file system layer (`write`, features.toml `fs.ext4_write`):
//! [`ext4d::lookup::inode_of`] finding the inode a path names, and [`Fs::set_times`]
//! changing it, on volumes made by e2fsprogs (`tests/fixtures/make-fixtures.sh`).
//!
//! Each lookup is checked by what it leads to: the times set on the inode it returns must be
//! the ones `ext4-view` then reads at that path, and nowhere else. When `EXT4D_E2FSCK` names
//! an `e2fsck` binary (CI sets it), the volumes the changes leave are also checked by it.

#![cfg(feature = "write")]

mod common;

use common::{SharedImage, e2fsck, load, load_raw};
use ext4d::Error;
use ext4d::device::Window;
use ext4d::fs::{Fs, OpenMode, ReadOnlyReason, TimeChange};
use ext4d::lookup::{self, LookupError, ROOT_INODE};
use ext4d::write::{self, Times, Timestamp, WriteError};

/// The clean volumes with a journal, with `hello.txt` as inode 12 (as `tests/write.rs`
/// relies on): extent-mapped directories, and indirect-mapped ones on ext3.
const JOURNALED: [&str; 5] = [
    "journal-v3",
    "journal-v2",
    "journal-v2-64",
    "journal-ext3",
    "journal-4k",
];

const A_TIME: TimeChange = TimeChange::At {
    seconds: 0x7000_0000,
    nanoseconds: 123_456_789,
};
const M_TIME: TimeChange = TimeChange::At {
    seconds: 0x7000_0001,
    nanoseconds: 5,
};

fn basic() -> Vec<u8> {
    load_raw("basic.ext4")
}

fn volume(image: Vec<u8>) -> Window<Vec<u8>> {
    Window::whole(image).expect("its length")
}

/// A volume mounted for changes, and the device it was given.
fn mount_writable(image: Vec<u8>) -> (Fs, SharedImage) {
    let shared = SharedImage::new(image);
    let fs = Fs::new_writable(Window::whole(shared.clone()).expect("its length"))
        .expect("a writable volume");
    (fs, shared)
}

/// Opens `path` from the root as UID 0, with `mode`.
fn open(fs: &mut Fs, path: &str, mode: OpenMode) -> usize {
    let root = fs.root().expect("the root");
    fs.open_as(Some(root), path, mode, 0, 0).expect("the path")
}

/// The access and modification times `ext4-view` reads at `path` (the link itself, for a
/// symbolic link).
fn times_at(image: Vec<u8>, path: &str) -> ((i64, u32), (i64, u32)) {
    let mut fs = Fs::new(volume(image)).expect("an ext4 volume");
    let id = open(
        &mut fs,
        path,
        OpenMode {
            stat: true,
            no_follow: true,
            ..OpenMode::read()
        },
    );
    let attributes = fs.attributes(id).expect("open");
    (attributes.accessed, attributes.modified)
}

/// A time with seconds `seconds`, for `write::set_times`.
fn at(seconds: i64) -> Timestamp {
    Timestamp {
        seconds,
        nanoseconds: 0,
    }
}

#[test]
fn lookups_find_the_inode_a_path_names() {
    let window = volume(basic());
    assert_eq!(
        lookup::inode_of(&window, "/").expect("the root"),
        ROOT_INODE
    );
    assert_eq!(
        lookup::inode_of(&window, "//").expect("the root"),
        ROOT_INODE
    );
    let mut found = Vec::new();
    // Plain files, nested ones, a directory, a symbolic link (its own inode) and files in a
    // hashed directory, on either side of its index.
    for path in [
        "/hello.txt",
        "/dir",
        "/dir/nested.txt",
        "/dir/private-dir/secret",
        "/link",
        "/many/f000",
        "/many/f150",
        "/many/f199",
    ] {
        let number = lookup::inode_of(&window, path).expect(path);
        assert!(number > ROOT_INODE, "{path}: {number}");
        found.push((path, number));
    }
    assert_eq!(
        lookup::inode_of(&window, "/dir/").expect("trailing slash"),
        found[1].1
    );
    // Each one is the inode ext4-view reads at its path: times set on it are read there,
    // and only there.
    for (index, &(path, number)) in found.iter().enumerate() {
        let seconds = 0x6000_0000 + index as i64;
        let times = Times {
            accessed: Some(at(seconds)),
            modified: Some(at(seconds)),
            changed: at(seconds),
        };
        let mut changed = volume(basic());
        write::set_times(&mut changed, number, &times).expect(path);
        let image = changed.into_inner();
        assert_eq!(
            times_at(image.clone(), path),
            ((seconds, 0), (seconds, 0)),
            "{path}"
        );
        for &(other, _) in found.iter().filter(|&&(other, _)| other != path) {
            assert_ne!(
                times_at(image.clone(), other).0,
                (seconds, 0),
                "{path} and {other}"
            );
        }
    }
}

#[test]
fn every_entry_of_a_hashed_directory_is_found() {
    let window = volume(basic());
    let mut numbers: Vec<u32> = (0..200)
        .map(|i| lookup::inode_of(&window, &format!("/many/f{i:03}")).expect("an entry"))
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), 200);
}

#[test]
fn lookups_work_on_every_journal_kind_and_block_map() {
    for name in JOURNALED {
        let window = volume(load(&format!("{name}.replayed.ext4.gz")));
        assert_eq!(
            lookup::inode_of(&window, "/hello.txt").expect(name),
            12,
            "{name}"
        );
    }
}

#[test]
fn an_entry_spanning_a_whole_64k_block_is_found() {
    // dir-64k.ext4.gz: d/b's entry is alone in d's second 64 KiB block, its length stored
    // as 0xFFFF; it is a second link to d/a.
    let image = load("dir-64k.ext4.gz");
    let window = volume(image.clone());
    let a = lookup::inode_of(&window, "/d/a").expect("d/a");
    assert_eq!(lookup::inode_of(&window, "/d/b").expect("d/b"), a);
    assert!(matches!(
        lookup::inode_of(&window, "/d/c"),
        Err(LookupError::NotFound { .. })
    ));
    // ext4-view reads the same file there.
    let mut fs = Fs::new(volume(image)).expect("an ext4 volume");
    let b = open(&mut fs, "d/b", OpenMode::read());
    let mut buf = [0; 64];
    let n = fs.read(b, 0, &mut buf).expect("readable");
    assert_eq!(&buf[..n], b"linked from a whole 64 KiB block\n");
}

#[test]
fn paths_that_name_nothing_are_refused() {
    let window = volume(basic());
    assert!(matches!(
        lookup::inode_of(&window, "/missing"),
        Err(LookupError::NotFound { directory: ROOT_INODE, ref name }) if name == "missing"
    ));
    assert!(matches!(
        lookup::inode_of(&window, "/many/f200"),
        Err(LookupError::NotFound { .. })
    ));
    assert!(matches!(
        lookup::inode_of(&window, "/hello.txt/x"),
        Err(LookupError::NotADirectory { .. })
    ));
    for path in ["hello.txt", "/dir/../hello.txt", "/./hello.txt"] {
        assert!(
            matches!(
                lookup::inode_of(&window, path),
                Err(LookupError::NotNormalized)
            ),
            "{path}"
        );
    }
}

#[test]
fn futimens_sets_the_times_and_the_change_shows_at_once() {
    let (mut fs, shared) = mount_writable(basic());
    assert!(fs.is_writable());
    let start = Timestamp::now().seconds;
    let hello = open(&mut fs, "hello.txt", OpenMode::read());
    let again = open(&mut fs, "/hello.txt", OpenMode::read());
    fs.set_times(hello, A_TIME, M_TIME, 0, 0).expect("set");
    let new = ((0x7000_0000, 123_456_789), (0x7000_0001, 5));
    for id in [hello, again] {
        let attributes = fs.attributes(id).expect("open");
        assert_eq!((attributes.accessed, attributes.modified), new);
    }
    // The contents are as they were, through the old handle and a new one.
    let mut buf = [0; 64];
    let n = fs.read(hello, 0, &mut buf).expect("readable");
    assert_eq!(&buf[..n], b"hello from ext4\n");
    let fresh = open(&mut fs, "hello.txt", OpenMode::read());
    assert_eq!(fs.read(fresh, 0, &mut buf).expect("readable"), n);
    // Setting times is itself a change to the inode: its change time is now, not the
    // modification time it was given.
    let changed = fs.attributes(hello).expect("open").changed;
    assert!(changed.0 >= start && changed != new.1, "{changed:?}");
    let image = e2fsck("futimens", shared.bytes());
    assert_eq!(times_at(image.clone(), "/hello.txt"), new);
    // A fresh mount reads the same change time from the volume.
    let mut fresh_fs = Fs::new(volume(image)).expect("an ext4 volume");
    let id = open(&mut fresh_fs, "hello.txt", OpenMode::read());
    assert_eq!(fresh_fs.attributes(id).expect("open").changed, changed);
}

#[test]
fn utime_now_and_omit() {
    let (mut fs, shared) = mount_writable(basic());
    let before = times_at(basic(), "/dir/nested.txt");
    let nested = open(&mut fs, "dir/nested.txt", OpenMode::read());
    let start = Timestamp::now().seconds;
    // UTIME_OMIT keeps the access time; UTIME_NOW sets the modification time to now.
    fs.set_times(nested, TimeChange::Omit, TimeChange::Now, 0, 0)
        .expect("set");
    let attributes = fs.attributes(nested).expect("open");
    assert_eq!(attributes.accessed, before.0);
    assert!(attributes.modified.0 >= start, "{:?}", attributes.modified);
    // The change time is now as well, and fstat reports it.
    assert!(attributes.changed.0 >= start, "{:?}", attributes.changed);
    assert_eq!(attributes.to_stat().st_ctime, attributes.changed.0 as u64);
    // Both omitted: nothing is written at all.
    let image = shared.bytes();
    fs.set_times(nested, TimeChange::Omit, TimeChange::Omit, 1234, 1234)
        .expect("nothing to do");
    assert!(shared.bytes() == image);
    e2fsck("utime-now", image);
}

#[test]
fn only_the_owner_sets_explicit_times() {
    // dir/private.txt: mode 0640, owner 1000, group 100.
    let (mut fs, shared) = mount_writable(basic());
    let private = open(&mut fs, "dir/private.txt", OpenMode::read());
    let untouched = shared.bytes();
    assert!(matches!(
        fs.set_times(private, A_TIME, M_TIME, 1001, 100),
        Err(Error::NotOwner)
    ));
    assert!(matches!(
        fs.set_times(private, TimeChange::Now, TimeChange::Omit, 1001, 100),
        Err(Error::NotOwner)
    ));
    // UTIME_NOW for both needs only write permission, which group 100 does not have.
    assert!(matches!(
        fs.set_times(private, TimeChange::Now, TimeChange::Now, 1001, 100),
        Err(Error::PermissionDenied)
    ));
    assert!(shared.bytes() == untouched);
    fs.set_times(private, A_TIME, M_TIME, 1000, 7)
        .expect("the owner");
    fs.set_times(private, TimeChange::Now, TimeChange::Now, 1000, 7)
        .expect("the owner");
    e2fsck("owner", shared.bytes());
}

#[test]
fn a_symbolic_link_opened_itself_gets_its_own_times() {
    let (mut fs, shared) = mount_writable(basic());
    let target_before = times_at(basic(), "/hello.txt");
    let link = open(
        &mut fs,
        "link",
        OpenMode {
            symlink: true,
            ..OpenMode::read()
        },
    );
    fs.set_times(link, A_TIME, M_TIME, 0, 0).expect("set");
    let image = shared.bytes();
    assert_eq!(
        times_at(image.clone(), "/link"),
        ((0x7000_0000, 123_456_789), (0x7000_0001, 5))
    );
    assert_eq!(times_at(image.clone(), "/hello.txt"), target_before);
    e2fsck("symlink", image);
}

#[test]
fn a_read_only_mount_refuses() {
    let mut fs = Fs::new(volume(basic())).expect("an ext4 volume");
    assert!(!fs.is_writable());
    let hello = open(&mut fs, "hello.txt", OpenMode::read());
    assert!(matches!(
        fs.set_times(hello, A_TIME, M_TIME, 0, 0),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(
        fs.set_times(99, A_TIME, M_TIME, 0, 0),
        Err(Error::BadHandle { handle: 99 })
    ));
}

#[test]
fn a_volume_that_cannot_be_written_is_not_mounted_for_changes() {
    // journal-v3.ext4.gz needs recovery: it is replayed before it is written.
    let image = load("journal-v3.ext4.gz");
    assert!(matches!(
        write::check_writable(&volume(image.clone())),
        Err(WriteError::NeedsReplay)
    ));
    assert!(matches!(
        Fs::new_writable(volume(image)),
        Err(Error::Write(WriteError::NeedsReplay))
    ));
    write::check_writable(&volume(basic())).expect("basic.ext4 can be written");
}

#[test]
fn a_device_that_will_not_open_for_writing_is_served_read_only() {
    let denied = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    // The volume can be written, but the device does not open for writing.
    let (mut fs, reason) =
        Fs::mount_preferring_write(volume(basic()), || Err::<Window<Vec<u8>>, _>(denied()))
            .expect("mounted");
    assert!(!fs.is_writable());
    assert!(
        matches!(reason, Some(ReadOnlyReason::Device(ref err)) if err.kind() == denied().kind())
    );
    let hello = open(&mut fs, "hello.txt", OpenMode::read());
    let mut buf = [0; 64];
    let n = fs.read(hello, 0, &mut buf).expect("readable");
    assert_eq!(&buf[..n], b"hello from ext4\n");
    assert!(matches!(
        fs.set_times(hello, A_TIME, M_TIME, 0, 0),
        Err(Error::ReadOnly)
    ));

    // A volume that cannot be written (dir-64k has no journal) is not opened for writing at
    // all.
    let (fs, reason) = Fs::mount_preferring_write(volume(load("dir-64k.ext4.gz")), || {
        panic!("the device is not opened for writing");
        #[allow(unreachable_code)]
        Err::<Window<Vec<u8>>, std::io::Error>(denied())
    })
    .expect("mounted");
    assert!(!fs.is_writable());
    assert!(matches!(
        reason,
        Some(ReadOnlyReason::Volume(WriteError::NoJournal))
    ));

    // Both can: the volume is mounted for changes.
    let shared = SharedImage::new(basic());
    let (fs, reason) = Fs::mount_preferring_write(volume(basic()), || {
        Ok::<_, std::io::Error>(Window::whole(shared.clone()).expect("its length"))
    })
    .expect("mounted");
    assert!(fs.is_writable());
    assert!(reason.is_none());
}
