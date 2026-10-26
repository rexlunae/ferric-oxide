//! Allocation (`write`, features.toml `fs.ext4_write`): writes past a file's last block,
//! truncation and file creation on volumes made by e2fsprogs
//! (`tests/fixtures/make-fixtures.sh`): `alloc.ext4.gz` (`metadata_csum`, 64-byte group
//! descriptors) and `alloc-gdt.ext4.gz` (`gdt_csum`, 32-byte descriptors).
//!
//! Every change is read back through `ext4-view` on a fresh mount, its free block and inode
//! counts are compared with what it should have used, and, when `EXT4D_E2FSCK` names an
//! `e2fsck` binary (CI sets it), the volume it leaves is checked by `e2fsck -fn` as it is,
//! without a replay first. The crash tests stop the device after each write in turn: both
//! `e2fsck` and our own replay must then find the change made in full or not at all.

#![cfg(feature = "write")]

mod common;

use common::{Crashing, SharedImage, e2fsck, e2fsck_clean, inode_at, le16, le32, load};
use ext4d::Error;
use ext4d::device::Window;
use ext4d::fs::{Fs, OpenMode};
use ext4d::journal;
use ext4d::lookup;
use ext4d::write::{self, NewFile, Timestamp, WriteError};

/// The two allocation fixtures, which hold the same files.
const IMAGES: [&str; 2] = ["alloc.ext4.gz", "alloc-gdt.ext4.gz"];

const CHANGED: Timestamp = Timestamp {
    seconds: 0x7000_0002,
    nanoseconds: 0,
};

const NEW_FILE: NewFile = NewFile {
    permissions: 0o640,
    uid: 1000,
    gid: 100,
    created: CHANGED,
};

const READ_WRITE: OpenMode = OpenMode {
    write: true,
    ..OpenMode::read()
};

/// The volume's free block and inode counts, from its superblock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Free {
    blocks: u64,
    inodes: u32,
}

fn free(image: &[u8]) -> Free {
    let sb = &image[1024..2048];
    // s_free_blocks_count_hi only counts with 64bit (s_feature_incompat 0x80).
    let hi = if le32(sb, 0x60) & 0x80 != 0 {
        u64::from(le32(sb, 0x158))
    } else {
        0
    };
    Free {
        blocks: hi << 32 | u64::from(le32(sb, 0x0C)),
        inodes: le32(sb, 0x10),
    }
}

fn number(image: &[u8], path: &str) -> u32 {
    let window = Window::whole(image.to_vec()).expect("its length");
    lookup::inode_of(&window, path).expect("the path")
}

/// The whole of `path` as `ext4-view` reads it on a fresh mount of `image`.
fn contents(image: &[u8], path: &str) -> Vec<u8> {
    let mut fs = Fs::new(Window::whole(image.to_vec()).expect("its length")).expect("ext4");
    let id = fs
        .open_as(None, path, OpenMode::read(), 0, 0)
        .expect("the path");
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

/// `i_blocks` (in 512-byte units) of inode `number`.
fn i_blocks(image: &[u8], number: u32) -> u32 {
    le32(image, inode_at(image, number) + 0x1C)
}

fn write_data(image: &mut Vec<u8>, path: &str, offset: u64, data: &[u8]) -> Result<(), WriteError> {
    let number = number(image, path);
    let mut window = Window::whole(std::mem::take(image)).expect("its length");
    let result = write::write_data(&mut window, number, offset, data, CHANGED);
    *image = window.into_inner();
    result
}

fn truncate(image: &mut Vec<u8>, path: &str, size: u64) -> Result<(), WriteError> {
    let number = number(image, path);
    let mut window = Window::whole(std::mem::take(image)).expect("its length");
    let result = write::truncate(&mut window, number, size, CHANGED);
    *image = window.into_inner();
    result
}

fn create(image: &mut Vec<u8>, directory: &str, name: &[u8]) -> Result<u32, WriteError> {
    let parent = number(image, directory);
    let mut window = Window::whole(std::mem::take(image)).expect("its length");
    let result = write::create(&mut window, parent, name, &NEW_FILE);
    *image = window.into_inner();
    result
}

/// `data` at `offset` in `old`, with zeroes in any gap between them.
fn written(old: &[u8], offset: usize, data: &[u8]) -> Vec<u8> {
    let mut new = old.to_vec();
    if new.len() < offset + data.len() {
        new.resize(offset + data.len(), 0);
    }
    new[offset..offset + data.len()].copy_from_slice(data);
    new
}

#[test]
fn an_append_past_the_last_block_allocates_blocks() {
    for name in IMAGES {
        let mut image = load(name);
        let before = free(&image);
        let old = contents(&image, "/small.txt");
        // 6 bytes in block 0, then 3000 more: blocks 1 to 2 are new.
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        write_data(&mut image, "/small.txt", 6, &data).expect("written");
        assert_eq!(
            contents(&image, "/small.txt"),
            written(&old, 6, &data),
            "{name}"
        );
        assert_eq!(
            free(&image),
            Free {
                blocks: before.blocks - 2,
                ..before
            },
            "{name}"
        );
        let small = number(&image, "/small.txt");
        assert_eq!(i_blocks(&image, small), 6, "{name}: three 1 KiB blocks");
        e2fsck_clean(&format!("append-{name}"), &image);
    }
}

#[test]
fn a_write_past_the_end_zeroes_the_gap_and_the_old_tail() {
    for name in IMAGES {
        let mut image = load(name);
        let old = contents(&image, "/small.txt");
        // Block 0 keeps "small\n" and zeroes after it; blocks 1 and 2 are a hole, block 3
        // holds the data.
        write_data(&mut image, "/small.txt", 3 * 1024 + 10, b"far").expect("written");
        let expected = written(&old, 3 * 1024 + 10, b"far");
        assert_eq!(contents(&image, "/small.txt"), expected, "{name}");
        let small = number(&image, "/small.txt");
        assert_eq!(i_blocks(&image, small), 4, "{name}: blocks 0 and 3");
        e2fsck_clean(&format!("gap-{name}"), &image);
        // Growing over the hole later reads zeroes up to the new bytes.
        write_data(&mut image, "/small.txt", 1024 + 1, b"mid").expect("written");
        let expected = written(&expected, 1024 + 1, b"mid");
        assert_eq!(contents(&image, "/small.txt"), expected, "{name}");
        e2fsck_clean(&format!("gap-fill-{name}"), &image);
    }
}

#[test]
fn a_write_into_a_hole_allocates_its_block() {
    let mut image = load("write-data.ext4.gz");
    let before = free(&image);
    let old = contents(&image, "/sparse.bin");
    write_data(&mut image, "/sparse.bin", 3 * 1024 + 100, b"filled").expect("written");
    assert_eq!(
        contents(&image, "/sparse.bin"),
        written(&old, 3 * 1024 + 100, b"filled")
    );
    assert_eq!(free(&image).blocks, before.blocks - 1);
    e2fsck_clean("hole", &image);
}

#[test]
fn a_full_extent_root_takes_a_block_that_extends_an_extent_but_not_a_new_one() {
    for name in IMAGES {
        // islands.bin: four one-block extents fill the root. Block 1 sits between two of
        // them and needs a fifth.
        let mut image = load(name);
        let untouched = image.clone();
        let err = write_data(&mut image, "/islands.bin", 1024, b"x").expect_err("no room");
        assert!(
            matches!(err, WriteError::ExtentTreeFull { .. }),
            "{name}: {err:?}"
        );
        assert!(image == untouched, "{name}: nothing written");
        // Block 7 follows block 6's extent. When the block after 6's is small.txt's (as
        // mke2fs 1.47 lays them out), it too needs a fifth extent, until small.txt lets its
        // block go; then block 7 extends block 6's extent into it.
        let islands = number(&image, "/islands.bin");
        let small = number(&image, "/small.txt");
        if root_extent_start(&image, islands, 3) + 1 == root_extent_start(&image, small, 0) {
            let err = write_data(&mut image, "/islands.bin", 7 * 1024, b"seven").expect_err("full");
            assert!(
                matches!(err, WriteError::ExtentTreeFull { .. }),
                "{name}: {err:?}"
            );
            assert!(image == untouched, "{name}: nothing written");
            truncate(&mut image, "/small.txt", 0).expect("truncated");
        }
        write_data(&mut image, "/islands.bin", 7 * 1024, b"seven").expect("written");
        let old = contents(&untouched, "/islands.bin");
        assert_eq!(
            contents(&image, "/islands.bin"),
            written(&old, 7 * 1024, b"seven"),
            "{name}"
        );
        // Still four extents.
        assert_eq!(
            le16(&image, inode_at(&image, islands) + 0x28 + 2),
            4,
            "{name}"
        );
        e2fsck_clean(&format!("islands-{name}"), &image);
    }
}

/// The first volume block of extent `index` in inode `number`'s root, which must be a leaf
/// (`ee_start_lo` at 8 in each 12-byte entry after the 12-byte header, "Extent Tree" in
/// "Ext4 Disk Layout").
fn root_extent_start(image: &[u8], number: u32, index: usize) -> u32 {
    let root = inode_at(image, number) + 0x28;
    assert_eq!(le16(image, root + 6), 0, "a leaf root");
    le32(image, root + 12 + index * 12 + 8)
}

#[test]
fn a_block_after_the_last_extent_extends_it_when_its_neighbour_is_free() {
    for name in IMAGES {
        // small.txt's block 0 has free blocks after it, so a write to block 1 extends its one
        // extent rather than adding a second: three more go the same way.
        let mut image = load(name);
        for block in 1..5u64 {
            write_data(&mut image, "/small.txt", block * 1024, b"more").expect("written");
        }
        let small = number(&image, "/small.txt");
        let at = inode_at(&image, small) + 0x28;
        // The extent header's entry count (`eh_entries`), at the start of i_block.
        assert_eq!(le16(&image, at + 2), 1, "{name}: one extent");
        e2fsck_clean(&format!("extend-{name}"), &image);
    }
}

#[test]
fn a_leaf_under_the_root_takes_new_extents() {
    for name in IMAGES {
        let mut image = load(name);
        let old = contents(&image, "/deep.bin");
        let mut expected = old.clone();
        for block in [1u64, 5, 9, 12] {
            let offset = usize::try_from(block * 1024).expect("fits");
            write_data(&mut image, "/deep.bin", block * 1024, b"inserted").expect("written");
            expected = written(&expected, offset, b"inserted");
            assert_eq!(
                contents(&image, "/deep.bin"),
                expected,
                "{name}: block {block}"
            );
        }
        e2fsck_clean(&format!("deep-{name}"), &image);
    }
}

#[test]
fn a_write_needing_more_blocks_than_are_free_is_refused() {
    for name in IMAGES {
        let mut image = load(name);
        let untouched = image.clone();
        let blocks = usize::try_from(free(&image).blocks).expect("fits");
        let err = write_data(
            &mut image,
            "/small.txt",
            1024,
            &vec![1; (blocks + 1) * 1024],
        )
        .expect_err("no space");
        assert!(matches!(err, WriteError::NoSpace), "{name}: {err:?}");
        assert_eq!(err_errno(err), 28);
        assert!(image == untouched, "{name}: nothing written");
    }
}

fn err_errno(err: WriteError) -> i32 {
    Error::Write(err).errno()
}

#[test]
fn allocation_on_a_volume_with_quotas_is_refused() {
    let mut image = load("quota.ext4.gz");
    let untouched = image.clone();
    let err = write_data(&mut image, "/hello.txt", 4096, b"x").expect_err("refused");
    assert!(matches!(err, WriteError::QuotaTracked { .. }), "{err:?}");
    let err = create(&mut image, "/", b"new.txt").expect_err("refused");
    assert!(matches!(err, WriteError::QuotaTracked { .. }), "{err:?}");
    assert!(image == untouched);
    // A write inside the blocks it has needs no allocation.
    write_data(&mut image, "/hello.txt", 0, b"H").expect("written");
}

#[test]
fn truncating_frees_the_blocks_past_the_new_end() {
    for name in IMAGES {
        let mut image = load(name);
        let before = free(&image);
        let old = contents(&image, "/deep.bin");
        // deep.bin has blocks 0, 2, 4, 6, 8 and 10; 3 KiB keeps 0 and 2.
        truncate(&mut image, "/deep.bin", 3 * 1024).expect("truncated");
        assert_eq!(contents(&image, "/deep.bin"), old[..3 * 1024], "{name}");
        assert_eq!(free(&image).blocks, before.blocks + 4, "{name}");
        e2fsck_clean(&format!("truncate-deep-{name}"), &image);
        // To nothing: the last two blocks go, and the leaf block with them.
        truncate(&mut image, "/deep.bin", 0).expect("truncated");
        assert_eq!(contents(&image, "/deep.bin"), b"", "{name}");
        assert_eq!(free(&image).blocks, before.blocks + 7, "{name}");
        let deep = number(&image, "/deep.bin");
        assert_eq!(i_blocks(&image, deep), 0, "{name}");
        e2fsck_clean(&format!("truncate-empty-{name}"), &image);
        // Its blocks can be had again.
        write_data(&mut image, "/deep.bin", 0, b"again").expect("written");
        assert_eq!(contents(&image, "/deep.bin"), b"again", "{name}");
        e2fsck_clean(&format!("truncate-again-{name}"), &image);
    }
}

#[test]
fn truncating_inside_a_block_zeroes_its_tail() {
    for name in IMAGES {
        let mut image = load(name);
        let old = contents(&image, "/islands.bin");
        // "island 0" in block 0; cut after "isl", then grow again: the bytes after it read
        // as zeroes, not as "and 0".
        truncate(&mut image, "/islands.bin", 3).expect("truncated");
        assert_eq!(contents(&image, "/islands.bin"), b"isl", "{name}");
        truncate(&mut image, "/islands.bin", 2048).expect("grown");
        let mut expected = old[..3].to_vec();
        expected.resize(2048, 0);
        assert_eq!(contents(&image, "/islands.bin"), expected, "{name}");
        e2fsck_clean(&format!("truncate-tail-{name}"), &image);
    }
}

#[test]
fn growing_by_truncation_allocates_nothing() {
    for name in IMAGES {
        let mut image = load(name);
        let before = free(&image);
        truncate(&mut image, "/small.txt", 1 << 20).expect("grown");
        let mut expected = b"small\n".to_vec();
        expected.resize(1 << 20, 0);
        assert_eq!(contents(&image, "/small.txt"), expected, "{name}");
        assert_eq!(free(&image), before, "{name}");
        e2fsck_clean(&format!("truncate-grow-{name}"), &image);
    }
}

#[test]
fn create_adds_an_empty_file_to_the_directory() {
    for name in IMAGES {
        let mut image = load(name);
        let before = free(&image);
        let new = create(&mut image, "/", b"new.txt").expect("created");
        assert_eq!(number(&image, "/new.txt"), new, "{name}");
        assert_eq!(contents(&image, "/new.txt"), b"", "{name}");
        assert_eq!(
            free(&image),
            Free {
                inodes: before.inodes - 1,
                ..before
            },
            "{name}"
        );
        let mut fs = Fs::new(Window::whole(image.clone()).expect("its length")).expect("ext4");
        let id = fs
            .open_as(None, "/new.txt", OpenMode::read(), 0, 0)
            .expect("the path");
        let attributes = fs.attributes(id).expect("open");
        assert_eq!(attributes.mode & 0o7777, 0o640, "{name}");
        assert_eq!((attributes.uid, attributes.gid), (1000, 100), "{name}");
        assert_eq!(attributes.modified.0, CHANGED.seconds, "{name}");
        e2fsck_clean(&format!("create-{name}"), &image);
        // It takes data like any other file.
        write_data(&mut image, "/new.txt", 0, b"contents\n").expect("written");
        assert_eq!(contents(&image, "/new.txt"), b"contents\n", "{name}");
        e2fsck_clean(&format!("create-write-{name}"), &image);
    }
}

#[test]
fn create_in_a_full_directory_adds_a_block_to_it() {
    for name in IMAGES {
        let mut image = load(name);
        let before = free(&image);
        let long = [b'n'; 40];
        create(&mut image, "/full", &long).expect("created");
        let path = format!("/full/{}", String::from_utf8_lossy(&long));
        assert_eq!(contents(&image, &path), b"", "{name}");
        // The old entries are still there.
        assert_eq!(
            contents(&image, "/full/entry-00000000000000"),
            b"",
            "{name}"
        );
        assert_eq!(
            contents(&image, "/full/entry-00000000000034"),
            b"",
            "{name}"
        );
        assert_eq!(free(&image).blocks, before.blocks - 1, "{name}");
        let full = number(&image, "/full");
        assert_eq!(le32(&image, inode_at(&image, full) + 0x4), 2048, "{name}");
        e2fsck_clean(&format!("create-full-{name}"), &image);
    }
}

#[test]
fn create_fills_the_slack_of_a_block_first() {
    for name in IMAGES {
        let mut image = load(name);
        let before = free(&image);
        // Room for many short names in the root's one block.
        for i in 0..20 {
            create(&mut image, "/", format!("f{i}").as_bytes()).expect("created");
        }
        assert_eq!(free(&image).blocks, before.blocks, "{name}");
        assert_eq!(free(&image).inodes, before.inodes - 20, "{name}");
        e2fsck_clean(&format!("create-many-{name}"), &image);
    }
}

#[test]
fn what_create_cannot_do_is_refused_before_anything_is_written() {
    let mut image = load("alloc.ext4.gz");
    let untouched = image.clone();
    let cases: [(&str, &[u8]); 7] = [
        ("/", b"small.txt"),
        ("/", b""),
        ("/", b"."),
        ("/", b".."),
        ("/", b"a/b"),
        ("/", &[b'x'; 256]),
        ("/small.txt", b"inside"),
    ];
    for (directory, name) in cases {
        let err = create(&mut image, directory, name).expect_err("refused");
        let expected = match name {
            b"small.txt" => matches!(err, WriteError::Exists { .. }),
            n if n.len() > 255 => matches!(err, WriteError::NameTooLong),
            b"inside" => matches!(err, WriteError::NotADirectory { .. }),
            _ => matches!(err, WriteError::InvalidName),
        };
        assert!(expected, "{directory} {name:?}: {err:?}");
        assert!(image == untouched);
    }
}

#[test]
fn create_in_a_hashed_directory_is_refused() {
    // No fixture has a hashed (htree) directory, so full/ is given the flag on the gdt_csum
    // volume, whose inodes have no checksum to fix up.
    let mut image = load("alloc-gdt.ext4.gz");
    let full = number(&image, "/full");
    let flags = inode_at(&image, full) + 0x20;
    let indexed = le32(&image, flags) | 0x1000;
    image[flags..flags + 4].copy_from_slice(&indexed.to_le_bytes());
    let untouched = image.clone();
    let err = create(&mut image, "/full", b"new").expect_err("refused");
    assert!(
        matches!(err, WriteError::UnsupportedDirectory { .. }),
        "{err:?}"
    );
    assert!(image == untouched);
}

#[test]
fn create_on_a_volume_without_extents_is_refused() {
    let mut image = load("journal-ext3.replayed.ext4.gz");
    let untouched = image.clone();
    let err = create(&mut image, "/", b"new").expect_err("refused");
    assert!(matches!(err, WriteError::NoExtents { .. }), "{err:?}");
    assert!(image == untouched);
}

/// Runs `change` on `before` with the device stopping after each write in turn, until it
/// gets through, and checks every crashed volume with `e2fsck` and with our replay: each must
/// hold `old` or `new` by `check`, and once one holds `new`, every later one does.
fn crash_each_write(
    label: &str,
    before: &[u8],
    change: impl Fn(&mut Window<Crashing>) -> Result<(), WriteError>,
    check: impl Fn(&[u8]) -> State,
) {
    let mut committed = false;
    for writes_left in 0.. {
        let mut window = Window::whole(Crashing {
            bytes: before.to_vec(),
            writes_left,
        })
        .expect("its length");
        let done = change(&mut window).is_ok();
        let crashed = window.into_inner().bytes;
        if done {
            assert!(writes_left > 2, "{label}: some writes");
            assert_eq!(check(&crashed), State::New, "{label}: finished");
            e2fsck_clean(&format!("{label}-done"), &crashed);
            return;
        }
        let theirs = e2fsck(&format!("{label}-{writes_left}"), crashed.clone());
        let mut window = Window::whole(crashed).expect("its length");
        journal::replay(&mut window).expect("replayed");
        let ours = window.into_inner();
        let state = check(&ours);
        assert_eq!(
            check(&theirs),
            state,
            "{label} {writes_left}: e2fsck and ours"
        );
        e2fsck_clean(&format!("{label}-{writes_left}-replayed"), &ours);
        if committed {
            assert_eq!(state, State::New, "{label} {writes_left}: stays committed");
        }
        committed = state == State::New;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Old,
    New,
}

fn state(is_old: bool, is_new: bool) -> State {
    match (is_old, is_new) {
        (true, false) => State::Old,
        (false, true) => State::New,
        _ => panic!("neither the old state nor the new one, or both"),
    }
}

#[test]
fn a_crash_during_an_append_leaves_the_old_file_or_the_new_one() {
    for name in IMAGES {
        let before = load(name);
        let small = number(&before, "/small.txt");
        let old = contents(&before, "/small.txt");
        let data = vec![0xA5; 2500];
        let new = written(&old, 6, &data);
        let free_before = free(&before);
        crash_each_write(
            &format!("crash-append-{name}"),
            &before,
            |window| write::write_data(window, small, 6, &data, CHANGED),
            |image| {
                let got = contents(image, "/small.txt");
                let s = state(got == old, got == new);
                let blocks = match s {
                    State::Old => free_before.blocks,
                    State::New => free_before.blocks - 2,
                };
                assert_eq!(free(image).blocks, blocks);
                s
            },
        );
    }
}

#[test]
fn a_crash_during_a_truncate_leaves_the_old_file_or_the_new_one() {
    for name in IMAGES {
        let before = load(name);
        let deep = number(&before, "/deep.bin");
        let old = contents(&before, "/deep.bin");
        crash_each_write(
            &format!("crash-truncate-{name}"),
            &before,
            |window| write::truncate(window, deep, 1500, CHANGED),
            |image| {
                let got = contents(image, "/deep.bin");
                state(got == old, got == old[..1500])
            },
        );
    }
}

#[test]
fn a_crash_during_a_create_leaves_no_file_or_the_new_one() {
    for name in IMAGES {
        let before = load(name);
        let full = number(&before, "/full");
        let path = "/full/a-name-that-needs-a-new-block";
        crash_each_write(
            &format!("crash-create-{name}"),
            &before,
            |window| write::create(window, full, &path.as_bytes()[6..], &NEW_FILE).map(|_| ()),
            |image| {
                let window = Window::whole(image.to_vec()).expect("its length");
                let found = lookup::inode_of(&window, path).is_ok();
                state(!found, found)
            },
        );
    }
}

fn mount_writable(image: Vec<u8>) -> (Fs, SharedImage) {
    let shared = SharedImage::new(image);
    let fs = Fs::new_writable(Window::whole(shared.clone()).expect("its length"))
        .expect("a writable volume");
    (fs, shared)
}

const CREATE: OpenMode = OpenMode {
    write: true,
    create: true,
    permissions: 0o600,
    ..OpenMode::read()
};

#[test]
fn o_creat_makes_a_file_owned_by_the_caller() {
    // The root directory is root's and 0755, so only root can add to it; the new file is
    // owned by root's group 50 as given, not the directory's group 0.
    let (mut fs, shared) = mount_writable(load("alloc.ext4.gz"));
    let id = fs.open_as(None, "/made", CREATE, 0, 50).expect("created");
    assert_eq!(fs.write(id, 0, b"by o_creat\n").expect("written"), 11);
    let attributes = fs.attributes(id).expect("open");
    assert_eq!(attributes.mode & 0o7777, 0o600);
    assert_eq!((attributes.uid, attributes.gid), (0, 50));
    assert_eq!(read_all(&mut fs, id), b"by o_creat\n");
    // Opened again with O_CREAT, it is the same file.
    let again = fs.open_as(None, "/made", CREATE, 0, 50).expect("opened");
    assert_eq!(read_all(&mut fs, again), b"by o_creat\n");
    let image = shared.bytes();
    assert_eq!(contents(&image, "/made"), b"by o_creat\n");
    e2fsck_clean("o-creat", &image);
}

#[test]
fn o_excl_refuses_a_file_that_exists() {
    let (mut fs, shared) = mount_writable(load("alloc.ext4.gz"));
    let exclusive = OpenMode {
        exclusive: true,
        ..CREATE
    };
    let untouched = shared.bytes();
    let err = fs
        .open_as(None, "/small.txt", exclusive, 0, 0)
        .expect_err("exists");
    assert!(matches!(err, Error::Exists), "{err:?}");
    assert_eq!(err.errno(), 17);
    assert!(shared.bytes() == untouched);
    fs.open_as(None, "/fresh", exclusive, 0, 0)
        .expect("created");
}

#[test]
fn o_creat_needs_write_and_search_permission_on_the_directory() {
    // full/ is root's, mode 0755: another user cannot add to it.
    let (mut fs, shared) = mount_writable(load("alloc.ext4.gz"));
    let untouched = shared.bytes();
    let err = fs
        .open_as(None, "/full/mine", CREATE, 1000, 100)
        .expect_err("refused");
    assert_eq!(err.errno(), 13, "{err:?}");
    assert!(shared.bytes() == untouched);
}

#[test]
fn o_trunc_empties_the_file_and_ftruncate_sets_its_size() {
    let (mut fs, shared) = mount_writable(load("alloc.ext4.gz"));
    let before = free(&shared.bytes());
    let mode = OpenMode {
        truncate: true,
        ..READ_WRITE
    };
    let id = fs.open_as(None, "/deep.bin", mode, 0, 0).expect("opened");
    assert_eq!(fs.attributes(id).expect("open").size, 0);
    assert_eq!(read_all(&mut fs, id), b"");
    assert_eq!(free(&shared.bytes()).blocks, before.blocks + 7);
    fs.truncate(id, 5000).expect("grown");
    assert_eq!(fs.attributes(id).expect("open").size, 5000);
    assert_eq!(read_all(&mut fs, id), vec![0; 5000]);
    // A handle opened for reading only cannot set the size.
    let reader = fs
        .open_as(None, "/small.txt", OpenMode::read(), 0, 0)
        .expect("opened");
    assert!(matches!(
        fs.truncate(reader, 0),
        Err(Error::BadHandle { .. })
    ));
    e2fsck_clean("o-trunc", &shared.bytes());
}
