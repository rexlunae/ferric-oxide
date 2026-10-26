//! Writing file data in place (`write`, features.toml `fs.ext4_write`): [`Fs::write`] and
//! `write::write_data` on volumes made by e2fsprogs (`tests/fixtures/make-fixtures.sh`),
//! read back through `ext4-view` and, when `EXT4D_E2FSCK` names an `e2fsck` binary (CI sets
//! it), checked by `e2fsck`.

#![cfg(feature = "write")]

mod common;

use common::{Crashing, SharedImage, e2fsck, inode_at, le32, load, load_raw};
use ext4d::Error;
use ext4d::device::Window;
use ext4d::fs::{Fs, OpenMode};
use ext4d::journal;
use ext4d::lookup;
use ext4d::write::{self, Timestamp, WriteError};

const READ_WRITE: OpenMode = OpenMode {
    write: true,
    ..OpenMode::read()
};

fn basic() -> Vec<u8> {
    load_raw("basic.ext4")
}

fn mount_writable(image: Vec<u8>) -> (Fs, SharedImage) {
    let shared = SharedImage::new(image);
    let fs = Fs::new_writable(Window::whole(shared.clone()).expect("its length"))
        .expect("a writable volume");
    (fs, shared)
}

fn open(fs: &mut Fs, path: &str, mode: OpenMode) -> usize {
    fs.open_as(None, path, mode, 0, 0).expect("the path")
}

/// The whole of `path` as `ext4-view` reads it on a fresh mount of `image`.
fn contents(image: Vec<u8>, path: &str) -> Vec<u8> {
    let mut fs = Fs::new(Window::whole(image).expect("its length")).expect("an ext4 volume");
    let id = open(&mut fs, path, OpenMode::read());
    read_all(&mut fs, id)
}

fn read_all(fs: &mut Fs, id: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let offset = u64::try_from(out.len()).expect("fits");
        let n = fs.read(id, offset, &mut buf).expect("readable");
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// `i_size_lo` of the inode `path` names.
fn size_field(image: &[u8], path: &str) -> u32 {
    let window = Window::whole(image.to_vec()).expect("its length");
    let number = lookup::inode_of(&window, path).expect("the path");
    le32(image, inode_at(image, number) + 0x4)
}

#[test]
fn a_write_inside_a_file_changes_its_bytes_and_times() {
    let (mut fs, shared) = mount_writable(basic());
    let reader = open(&mut fs, "/hello.txt", OpenMode::read());
    let before = fs.attributes(reader).expect("open");
    let start = Timestamp::now().seconds;
    let hello = open(&mut fs, "/hello.txt", READ_WRITE);
    assert_eq!(fs.write(hello, 6, b"FROM").expect("written"), 4);
    let after = fs.attributes(hello).expect("open");
    assert_eq!(after.size, 16);
    assert!(
        after.modified.0 >= start && after.changed.0 >= start,
        "{after:?}"
    );
    assert_eq!(after.accessed, before.accessed);
    // The handle that wrote, and a fresh mount, read the new bytes.
    assert_eq!(read_all(&mut fs, hello), b"hello FROM ext4\n");
    let image = e2fsck("write-inside", shared.bytes());
    assert_eq!(contents(image, "/hello.txt"), b"hello FROM ext4\n");
}

#[test]
fn a_write_past_the_end_inside_the_last_block_grows_the_file() {
    let (mut fs, shared) = mount_writable(basic());
    let hello = open(&mut fs, "/hello.txt", READ_WRITE);
    fs.write(hello, 16, b"and more\n").expect("written");
    assert_eq!(fs.attributes(hello).expect("open").size, 25);
    let image = shared.bytes();
    assert_eq!(size_field(&image, "/hello.txt"), 25);
    let image = e2fsck("write-grow", image);
    assert_eq!(
        contents(image, "/hello.txt"),
        b"hello from ext4\nand more\n"
    );
}

#[test]
fn a_write_across_blocks_changes_only_its_bytes() {
    // big.bin: 200 KiB in 1 KiB blocks. 3000 bytes from 1000 touch blocks 0 to 3, the
    // first and last of them in part; 2048..3072 is one whole block.
    let (mut fs, shared) = mount_writable(basic());
    let big = open(&mut fs, "/big.bin", READ_WRITE);
    let mut expected = read_all(&mut fs, big);
    let data: Vec<u8> = (0..3000u32).map(|i| (i % 253) as u8 ^ 0x5A).collect();
    fs.write(big, 1000, &data).expect("written");
    expected[1000..4000].copy_from_slice(&data);
    fs.write(big, 2048, &[0xEE; 1024]).expect("written");
    expected[2048..3072].fill(0xEE);
    assert!(read_all(&mut fs, big) == expected);
    let image = e2fsck("write-across", shared.bytes());
    assert!(contents(image, "/big.bin") == expected);
}

#[test]
fn other_handles_read_what_was_written() {
    let (mut fs, _) = mount_writable(basic());
    let reader = open(&mut fs, "/dir/nested.txt", OpenMode::read());
    assert_eq!(read_all(&mut fs, reader), b"nested\n");
    let writer = open(&mut fs, "/dir/nested.txt", READ_WRITE);
    fs.write(writer, 0, b"NEST").expect("written");
    assert_eq!(read_all(&mut fs, reader), b"NESTed\n");
}

#[test]
fn a_write_that_needs_a_new_block_writes_nothing() {
    let (mut fs, shared) = mount_writable(basic());
    let untouched = shared.bytes();
    let hello = open(&mut fs, "/hello.txt", READ_WRITE);
    // Block 1 of hello.txt does not exist; 1020..1030 starts in block 0, which does.
    for offset in [1024, 1020] {
        let err = fs.write(hello, offset, b"0123456789").expect_err("refused");
        assert!(
            matches!(err, Error::Write(WriteError::NotAllocated { block: 1, .. })),
            "{err:?}"
        );
        assert_eq!(err.errno(), 95);
    }
    assert!(shared.bytes() == untouched);
    // A hole and an unwritten extent need a block too.
    let (mut fs, shared) = mount_writable(load("write-data.ext4.gz"));
    let untouched = shared.bytes();
    for (path, block) in [("/sparse.bin", 3), ("/unwritten.bin", 1)] {
        let id = open(&mut fs, path, READ_WRITE);
        let err = fs.write(id, block * 1024, b"x").expect_err("refused");
        assert!(
            matches!(err, Error::Write(WriteError::NotAllocated { block: b, .. }) if b == block),
            "{path}: {err:?}"
        );
    }
    assert!(shared.bytes() == untouched);
    // Around them, the blocks the files have are written.
    let sparse = open(&mut fs, "/sparse.bin", READ_WRITE);
    fs.write(sparse, 0, b"HEAD").expect("block 0");
    fs.write(sparse, 8192, b"TAIL").expect("block 8");
    let image = e2fsck("write-sparse", shared.bytes());
    let data = contents(image, "/sparse.bin");
    assert_eq!((&data[..4], &data[8192..]), (&b"HEAD"[..], &b"TAIL"[..]));
    assert!(data[4..8192].iter().all(|&b| b == 0));
}

#[test]
fn a_time_the_inode_cannot_hold_is_refused_before_any_data_is_written() {
    let image = basic();
    let window = Window::whole(image.clone()).expect("its length");
    let number = lookup::inode_of(&window, "/hello.txt").expect("hello.txt");
    // Past 2446 (or 2038 without the extra fields), and nanoseconds of a whole second: the
    // inode's new times would not fit, so the data, which goes home first, must not either.
    for changed in [
        Timestamp {
            seconds: 1 << 40,
            nanoseconds: 0,
        },
        Timestamp {
            seconds: 0,
            nanoseconds: 1_000_000_000,
        },
    ] {
        let mut window = Window::whole(image.clone()).expect("its length");
        // Inside the file's one block, and past its end, so a grown size is checked too.
        for offset in [0, 10] {
            let err = write::write_data(&mut window, number, offset, b"0123456789", changed)
                .expect_err("refused");
            assert!(
                matches!(err, WriteError::TimeOutOfRange { number: n, .. } if n == number),
                "{changed:?}: {err:?}"
            );
        }
        assert!(window.into_inner() == image, "{changed:?}: nothing written");
    }
}

#[test]
fn immutable_and_append_only_files_are_refused() {
    let (mut fs, shared) = mount_writable(load("write-data.ext4.gz"));
    let untouched = shared.bytes();
    for (path, flag) in [("/immutable.txt", 0x10), ("/append.txt", 0x20)] {
        let id = open(&mut fs, path, READ_WRITE);
        let err = fs.write(id, 0, b"x").expect_err("refused");
        assert!(
            matches!(err, Error::Write(WriteError::FileFlags { flags, .. }) if flags == flag),
            "{path}: {err:?}"
        );
        assert_eq!(err.errno(), 1, "{path}: EPERM");
    }
    assert!(shared.bytes() == untouched);
}

#[test]
fn opens_for_writing_check_write_permission() {
    // dir/private.txt: mode 0640, owner 1000, group 100.
    let (mut fs, _) = mount_writable(basic());
    let write = |fs: &mut Fs, uid, gid, mode| fs.open_as(None, "/dir/private.txt", mode, uid, gid);
    assert!(matches!(
        write(&mut fs, 1001, 100, READ_WRITE),
        Err(Error::PermissionDenied)
    ));
    let owner = write(&mut fs, 1000, 100, READ_WRITE).expect("the owner");
    fs.write(owner, 0, b"P").expect("written");
    // O_WRONLY needs write permission only.
    let id = open(&mut fs, "/dir/private.txt", OpenMode::read());
    fs.set_mode(id, 0o200, 0, 0).expect("chmod");
    assert!(matches!(
        write(&mut fs, 1000, 100, READ_WRITE),
        Err(Error::PermissionDenied)
    ));
    let write_only = OpenMode {
        write_only: true,
        ..READ_WRITE
    };
    let id = write(&mut fs, 1000, 100, write_only).expect("write permission is enough");
    fs.write(id, 1, b"R").expect("written");
    // It did not check read permission, so it cannot read: EBADF, as for any handle not
    // opened for reading, and so for root's write-only handles too.
    let mut buf = [0; 4];
    let err = fs
        .read(id, 0, &mut buf)
        .expect_err("not opened for reading");
    assert!(
        matches!(err, Error::BadHandle { handle } if handle == id),
        "{err:?}"
    );
    assert_eq!(err.errno(), syscall::error::EBADF);
    let root = write(&mut fs, 0, 0, write_only).expect("root");
    assert!(matches!(
        fs.read(root, 0, &mut buf),
        Err(Error::BadHandle { .. })
    ));
}

#[test]
fn what_cannot_be_written_is_refused_at_open_or_write() {
    let (mut fs, _) = mount_writable(basic());
    let create = OpenMode {
        create_or_truncate: true,
        ..READ_WRITE
    };
    let err = fs
        .open_as(None, "/hello.txt", create, 0, 0)
        .expect_err("refused");
    assert!(matches!(err, Error::CreateOrTruncate));
    assert_eq!(err.errno(), 95);
    assert!(matches!(
        fs.open_as(None, "/dir", READ_WRITE, 0, 0),
        Err(Error::IsADirectory)
    ));
    // A handle opened for reading does not write.
    let reader = open(&mut fs, "/hello.txt", OpenMode::read());
    assert!(matches!(
        fs.write(reader, 0, b"x"),
        Err(Error::BadHandle { .. })
    ));
    // Nothing to write is no change at all.
    let hello = open(&mut fs, "/hello.txt", READ_WRITE);
    let before = fs.attributes(hello).expect("open");
    assert_eq!(fs.write(hello, 0, b"").expect("nothing"), 0);
    assert_eq!(fs.attributes(hello).expect("open"), before);
    // A read-only mount refuses the open.
    let mut ro = Fs::new(Window::whole(basic()).expect("its length")).expect("an ext4 volume");
    assert!(matches!(
        ro.open_as(None, "/hello.txt", READ_WRITE, 0, 0),
        Err(Error::ReadOnly)
    ));
}

#[test]
fn a_crash_at_any_write_leaves_the_old_size_or_the_new_one() {
    let before = load("journal-v3.replayed.ext4.gz");
    let window = Window::whole(before.clone()).expect("its length");
    let number = lookup::inode_of(&window, "/hello.txt").expect("hello.txt");
    let old = contents(before.clone(), "/hello.txt");
    let mut new = old.clone();
    new.extend_from_slice(b"grown\n");
    let offset = u64::try_from(old.len()).expect("fits");
    let changed = Timestamp {
        seconds: 0x7000_0002,
        nanoseconds: 0,
    };
    // The data block, then the eight writes of the inode's transaction (tests/write.rs).
    for writes_left in 0..9 {
        let mut window = Window::whole(Crashing {
            bytes: before.clone(),
            writes_left,
        })
        .expect("its length");
        write::write_data(&mut window, number, offset, b"grown\n", changed)
            .expect_err("the device stops");
        let crashed = window.into_inner().bytes;
        // The data is home from the first write on, but past the old size until the
        // transaction is committed (from the sixth write on).
        let expected = if writes_left < 6 { &old } else { &new };
        let theirs = e2fsck(&format!("data-crash-{writes_left}"), crashed.clone());
        assert_eq!(
            &contents(theirs, "/hello.txt"),
            expected,
            "{writes_left}: e2fsck"
        );
        let mut window = Window::whole(crashed).expect("its length");
        journal::replay(&mut window).expect("replayed");
        assert_eq!(
            &contents(window.into_inner(), "/hello.txt"),
            expected,
            "{writes_left}"
        );
    }
}
