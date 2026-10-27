//! Read-only mounts of volumes that need recovery, without `journal-replay`: the journal
//! fixtures (`tests/fixtures/make-fixtures.sh`), each against `NAME.replayed.ext4.gz`, the
//! same volume after `e2fsck -E journal_only`.
//!
//! `ext4-view` 1.0.0's own view of the journal refuses a journal with an escaped block and
//! does not follow a log that wraps round, serving the blocks as they were before it. ext4d
//! therefore gives it the volume as a replay would leave it (`journal::replayed_blocks`),
//! so every file, its contents and all of its metadata come from the committed journal.

mod common;

use common::{le32, load, reseal};
use ext4d::device::Window;
use ext4d::fs::{Attributes, Fs, Kind, OpenMode};
use ext4d::journal;

const NAMES: [&str; 8] = [
    "journal-v3",
    "journal-v2",
    "journal-v2-64",
    "journal-ext3",
    "journal-4k",
    "journal-wrap",
    "journal-wrap-inode",
    "journal-wrap-space",
];

fn mount(image: Vec<u8>) -> Fs {
    Fs::new(Window::whole(image).expect("its length")).expect("an ext4 volume")
}

/// Every file on the volume, by path: its attributes and, for a regular file, its contents.
fn tree(fs: &mut Fs) -> Vec<(String, Attributes, Vec<u8>)> {
    let mut out = Vec::new();
    let root = fs.root().expect("the root");
    walk(fs, root, "", &mut out);
    out
}

fn walk(fs: &mut Fs, dir: usize, path: &str, out: &mut Vec<(String, Attributes, Vec<u8>)>) {
    let entries = fs.entries(dir).expect("a directory").to_vec();
    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let path = format!("{path}/{}", entry.name);
        let mode = OpenMode {
            stat: true,
            no_follow: true,
            ..OpenMode::read()
        };
        let id = fs.open(None, &path, mode).expect("the entry");
        let attributes = fs.attributes(id).expect("open");
        let mut contents = Vec::new();
        match entry.kind {
            Kind::Directory => {
                let sub = fs.open(None, &path, OpenMode::read()).expect("a directory");
                walk(fs, sub, &path, out);
            }
            Kind::File => {
                let file = fs.open(None, &path, OpenMode::read()).expect("a file");
                let mut buf = vec![0; 1 << 16];
                loop {
                    let offset = u64::try_from(contents.len()).expect("fits");
                    let n = fs.read(file, offset, &mut buf).expect("readable");
                    if n == 0 {
                        break;
                    }
                    contents.extend_from_slice(&buf[..n]);
                }
            }
            _ => {}
        }
        out.push((path, attributes, contents));
    }
}

#[test]
fn a_read_only_mount_serves_what_a_replay_leaves() {
    for name in NAMES {
        let mut ours = mount(load(&format!("{name}.ext4.gz")));
        assert!(ours.needs_recovery(), "{name}");
        let mut theirs = mount(load(&format!("{name}.replayed.ext4.gz")));
        assert!(!theirs.needs_recovery(), "{name}");
        assert_eq!(
            ours.geometry(),
            theirs.geometry(),
            "{name}: recovered geometry"
        );
        let (ours, theirs) = (tree(&mut ours), tree(&mut theirs));
        assert!(!theirs.is_empty(), "{name}");
        assert_eq!(ours, theirs, "{name}");
    }
}

#[test]
fn wrapped_recovery_uses_journaled_free_space_and_keeps_the_device_dirty() {
    let image = load("journal-wrap-space.ext4.gz");
    let physical =
        ext4d::superblock::Geometry::parse(image[1024..2048].try_into().unwrap()).unwrap();
    let expected_image = load("journal-wrap-space.replayed.ext4.gz");
    let expected =
        ext4d::superblock::Geometry::parse(expected_image[1024..2048].try_into().unwrap()).unwrap();
    assert_ne!(
        physical.free_blocks, expected.free_blocks,
        "fixture logs changed counts"
    );
    assert_ne!(
        physical.free_inodes, expected.free_inodes,
        "fixture logs an inode allocation"
    );
    let shared = common::SharedImage::new(image.clone());
    let fs = Fs::new(Window::whole(shared.clone()).unwrap()).unwrap();
    assert_eq!(fs.geometry(), expected);
    assert!(fs.needs_recovery());
    assert_eq!(shared.bytes(), image);
}

#[test]
fn the_journaled_contents_are_served() {
    for name in &NAMES[..6] {
        let mut fs = mount(load(&format!("{name}.ext4.gz")));
        let hello = fs
            .open(None, "/hello.txt", OpenMode::read())
            .expect("hello.txt");
        let mut buf = [0; 64];
        let n = fs.read(hello, 0, &mut buf).expect("readable");
        assert_eq!(&buf[..n], b"journaled hello\n", "{name}");
    }
}

/// Why ext4d does not leave the journal to `ext4-view`: as of 1.0.0 it refuses every
/// volume here that logs an escaped block, and serves journal-wrap's hello.txt and
/// journal-wrap-inode's times as they were before the journal.
#[test]
fn ext4_view_alone_cannot_serve_these_volumes() {
    for name in NAMES {
        let result = ext4_view::Ext4::load(Box::new(load(&format!("{name}.ext4.gz"))));
        match name {
            "journal-wrap" => {
                let ext4 = result.expect("loads");
                assert_eq!(
                    ext4.read("/hello.txt").expect("readable"),
                    b"hello from ext4\n",
                    "{name}: stale"
                );
            }
            "journal-wrap-inode" => {
                let ext4 = result.expect("loads");
                let modified = ext4.metadata("/hello.txt").expect("hello.txt").modified();
                assert_eq!(modified.seconds(), 0x6955_B900, "{name}: stale");
            }
            "journal-wrap-space" => {
                let ext4 = result.expect("loads");
                assert!(ext4.metadata("/allocated.txt").is_err(), "stale file tree");
            }
            _ => assert!(result.is_err(), "{name}"),
        }
    }
}

#[test]
fn a_wrapped_journal_does_not_mix_two_versions_of_an_inode() {
    let journaled = ((0x7000_0000, 0), (0x7000_0001, 0), (0x7000_0002, 0));
    for name in ["journal-wrap-inode.replayed", "journal-wrap-inode"] {
        let mut fs = mount(load(&format!("{name}.ext4.gz")));
        let file = fs
            .open(None, "/hello.txt", OpenMode::read())
            .expect("hello.txt");
        let a = fs.attributes(file).expect("open");
        assert_eq!((a.accessed, a.modified, a.changed), journaled, "{name}");
    }
}

/// An empty journal still needs a clean superblock in the reader's overlay. Otherwise
/// ext4-view would take its own journal path even though there are no committed writes.
#[test]
fn an_empty_journal_overlays_only_the_volume_superblock() {
    for name in ["journal-v3", "journal-4k"] {
        let clean = load(&format!("{name}.replayed.ext4.gz"));
        let mut dirty = clean.clone();
        // s_feature_incompat's low byte: INCOMPAT_RECOVER, per Ext4 Disk Layout.
        dirty[1024 + 0x60] |= 0x4;
        reseal(&mut dirty);
        let window = Window::whole(dirty.clone()).expect("its length");
        assert!(
            journal::committed_blocks(&window)
                .expect("empty journal")
                .is_empty()
        );
        let blocks = journal::replayed_blocks(&window).expect("readable journal");
        let block_size = 1024usize << le32(&dirty, 1024 + 0x18);
        let home = 1024 / block_size;
        assert_eq!(blocks.len(), 1, "{name}: the clean superblock is required");
        assert_eq!(
            blocks[&(home as u64)],
            clean[home * block_size..(home + 1) * block_size]
        );
        assert_eq!(window.into_inner(), dirty, "the device was not changed");
        let mut ours = mount(dirty);
        assert!(
            ours.needs_recovery(),
            "the underlying volume is still dirty"
        );
        assert_eq!(tree(&mut ours), tree(&mut mount(clean)), "{name}");
    }
}
