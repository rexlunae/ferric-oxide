//! `ext4d`'s file system layer on `basic.ext4`, an image made by `mke2fs`
//! (`tests/fixtures/make-fixtures.sh`, which lists its contents).
//!
//! The committed image is used unless `EXT4D_FIXTURES` names a directory holding a fresh
//! `basic.ext4` (CI builds one with the script and runs these tests on it too).

use std::path::PathBuf;

use ext4d::Error;
use ext4d::device::Window;
use ext4d::fs::{Fs, Kind, OpenMode};

fn fixture() -> PathBuf {
    let dir = std::env::var_os("EXT4D_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        PathBuf::from,
    );
    dir.join("basic.ext4")
}

fn mount() -> Fs {
    let image = std::fs::File::open(fixture()).expect("the fixture image");
    Fs::new(Window::whole(image).expect("its length")).expect("an ext4 volume")
}

fn read_all(fs: &mut Fs, id: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0; 7000];
    loop {
        let offset = u64::try_from(out.len()).expect("fits");
        let n = fs.read(id, offset, &mut buf).expect("readable");
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

fn big_bin() -> Vec<u8> {
    (0..200 * 1024)
        .map(|i: u32| u8::try_from((i * 7 + i / 4096) % 251).expect("below 251"))
        .collect()
}

#[test]
fn the_superblock_gives_the_geometry() {
    let fs = mount();
    let g = fs.geometry();
    assert_eq!(g.block_size, 1024);
    assert_eq!(g.blocks, 4096);
    assert_eq!(g.inodes, 1024);
    assert!(g.free_blocks > 0 && g.free_blocks < g.blocks);
    assert!(g.free_inodes > 0 && g.free_inodes < g.inodes);
    assert_eq!(fs.label(), "ferric-basic");
}

#[test]
fn the_root_lists_its_entries() {
    let mut fs = mount();
    let root = fs.root().expect("the root");
    let mut names: Vec<(String, Kind)> = fs
        .entries(root)
        .expect("a directory")
        .iter()
        .map(|e| (e.name.clone(), e.kind))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        names,
        [
            ("big.bin".into(), Kind::File),
            ("dir".into(), Kind::Directory),
            ("empty".into(), Kind::Directory),
            ("hello.txt".into(), Kind::File),
            ("link".into(), Kind::Symlink),
            ("lost+found".into(), Kind::Directory),
            ("many".into(), Kind::Directory),
        ]
    );
}

#[test]
fn a_hashed_directory_lists_every_entry() {
    let mut fs = mount();
    let many = fs
        .open(None, "/many", OpenMode::read())
        .expect("a directory");
    let mut names: Vec<String> = fs
        .entries(many)
        .expect("a directory")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    names.sort();
    let want: Vec<String> = (0..200).map(|i| format!("f{i:03}")).collect();
    assert_eq!(names, want);
    // A lookup in it goes through the hash tree.
    let f = fs
        .open(Some(many), "f137", OpenMode::read())
        .expect("a file");
    assert_eq!(read_all(&mut fs, f), b"f137\n");
}

#[test]
fn an_empty_directory_has_no_entries() {
    let mut fs = mount();
    let empty = fs
        .open(None, "empty", OpenMode::read())
        .expect("a directory");
    assert!(fs.entries(empty).expect("a directory").is_empty());
}

#[test]
fn files_read_whole_and_at_offsets() {
    let mut fs = mount();
    let hello = fs
        .open(None, "hello.txt", OpenMode::read())
        .expect("a file");
    assert_eq!(read_all(&mut fs, hello), b"hello from ext4\n");
    let mut buf = [0; 4];
    assert_eq!(fs.read(hello, 6, &mut buf).expect("readable"), 4);
    assert_eq!(&buf, b"from");
    // At and past the end, nothing.
    assert_eq!(fs.read(hello, 16, &mut buf).expect("readable"), 0);
    assert_eq!(fs.read(hello, 1 << 40, &mut buf).expect("readable"), 0);

    let big = fs.open(None, "big.bin", OpenMode::read()).expect("a file");
    let want = big_bin();
    assert_eq!(read_all(&mut fs, big), want);
    // Across a block boundary, from the middle.
    let mut buf = vec![0; 3000];
    assert_eq!(fs.read(big, 100_000, &mut buf).expect("readable"), 3000);
    assert_eq!(buf, want[100_000..103_000]);
}

#[test]
fn relative_opens_start_at_their_directory() {
    let mut fs = mount();
    let dir = fs.open(None, "dir", OpenMode::read()).expect("a directory");
    let nested = fs
        .open(Some(dir), "nested.txt", OpenMode::read())
        .expect("a file");
    assert_eq!(read_all(&mut fs, nested), b"nested\n");
    assert_eq!(fs.path(nested).expect("open"), "/dir/nested.txt");
    let up = fs
        .open(Some(dir), "../hello.txt", OpenMode::read())
        .expect("a file");
    assert_eq!(fs.path(up).expect("open"), "/hello.txt");
    // An absolute path ignores the directory.
    let abs = fs
        .open(Some(dir), "/hello.txt", OpenMode::read())
        .expect("a file");
    assert_eq!(read_all(&mut fs, abs), b"hello from ext4\n");
}

#[test]
fn symbolic_links_are_followed_or_read() {
    let mut fs = mount();
    let followed = fs.open(None, "link", OpenMode::read()).expect("the target");
    assert_eq!(read_all(&mut fs, followed), b"hello from ext4\n");
    assert_eq!(fs.attributes(followed).expect("open").kind, Kind::File);

    let mode = OpenMode {
        symlink: true,
        no_follow: true,
        ..OpenMode::read()
    };
    let link = fs.open(None, "link", mode).expect("the link");
    assert_eq!(read_all(&mut fs, link), b"hello.txt");
    assert_eq!(fs.attributes(link).expect("open").kind, Kind::Symlink);

    let no_follow = OpenMode {
        no_follow: true,
        ..OpenMode::read()
    };
    let err = fs.open(None, "link", no_follow).expect_err("a link");
    assert!(matches!(err, Error::SymlinkNotFollowed));
}

#[test]
fn attributes_come_from_the_inode() {
    let mut fs = mount();
    let big = fs.open(None, "big.bin", OpenMode::read()).expect("a file");
    let a = fs.attributes(big).expect("open");
    assert_eq!(a.size, 200 * 1024);
    assert_eq!(a.kind, Kind::File);
    assert_eq!(a.mode & 0o170_000, 0o100_000);
    // mke2fs -d copies the source files' permissions: 0644 for a file made by the shell.
    assert_eq!(a.mode & 0o777, 0o644);
    assert_eq!(a.block_size, 1024);
    // make-fixtures.sh sets every time to 2026-01-01 00:00:00 UTC.
    assert_eq!(a.modified, (1_767_225_600, 0));
    let root = fs.root().expect("the root");
    let r = fs.attributes(root).expect("open");
    assert_eq!((r.kind, r.ino, r.uid, r.gid), (Kind::Directory, 2, 0, 0));
    assert_eq!(r.mode & 0o170_000, 0o040_000);
}

#[test]
fn missing_paths_and_wrong_kinds_are_refused() {
    let mut fs = mount();
    let err = fs
        .open(None, "nope", OpenMode::read())
        .expect_err("missing");
    assert_eq!(err.errno(), syscall::error::ENOENT);
    let dir_only = OpenMode {
        directory: true,
        ..OpenMode::read()
    };
    let err = fs.open(None, "hello.txt", dir_only).expect_err("a file");
    assert_eq!(err.errno(), syscall::error::ENOTDIR);
    let err = fs
        .open(None, "hello.txt/x", OpenMode::read())
        .expect_err("a file");
    assert_eq!(err.errno(), syscall::error::ENOTDIR);
    let dir = fs.open(None, "dir", OpenMode::read()).expect("a directory");
    let err = fs.read(dir, 0, &mut [0; 4]).expect_err("a directory");
    assert_eq!(err.errno(), syscall::error::EISDIR);
    let hello = fs
        .open(None, "hello.txt", OpenMode::read())
        .expect("a file");
    let err = fs.entries(hello).expect_err("a file");
    assert_eq!(err.errno(), syscall::error::ENOTDIR);
}

#[test]
fn every_change_is_erofs() {
    let mut fs = mount();
    let writes = [
        OpenMode {
            write: true,
            ..OpenMode::read()
        },
        OpenMode {
            create_or_truncate: true,
            ..OpenMode::read()
        },
    ];
    for mode in writes {
        for path in ["hello.txt", "new.txt", "dir"] {
            let err = fs.open(None, path, mode).expect_err("read-only");
            assert_eq!(err.errno(), syscall::error::EROFS, "{path}");
        }
    }
    let hello = fs
        .open(None, "hello.txt", OpenMode::read())
        .expect("a file");
    assert_eq!(
        fs.refuse_change(hello).expect_err("read-only").errno(),
        syscall::error::EROFS
    );
    // A bad handle is still a bad handle.
    assert_eq!(
        fs.refuse_change(9999).expect_err("unknown").errno(),
        syscall::error::EBADF
    );
}

#[test]
fn closed_handles_are_gone() {
    let mut fs = mount();
    let hello = fs
        .open(None, "hello.txt", OpenMode::read())
        .expect("a file");
    fs.close(hello);
    let err = fs.read(hello, 0, &mut [0; 4]).expect_err("closed");
    assert!(matches!(err, Error::BadHandle { handle } if handle == hello));
}

#[test]
fn other_volumes_are_refused() {
    let zeros = vec![0u8; 64 * 1024];
    let err = Fs::new(Window::whole(zeros).expect("its length"))
        .err()
        .expect("not ext4");
    assert!(matches!(err, Error::Superblock(_)));
    assert_eq!(err.errno(), syscall::error::EINVAL);
    let tiny = vec![0u8; 100];
    let err = Fs::new(Window::whole(tiny).expect("its length"))
        .err()
        .expect("too short");
    assert!(matches!(err, Error::Device(_)));
}

#[test]
fn a_partition_is_read_through_its_window() {
    // The fixture behind 1 MiB of something else, as a partition on a disk.
    let image = std::fs::read(fixture()).expect("the fixture image");
    let mut disk = vec![0xa5; 1 << 20];
    disk.extend_from_slice(&image);
    let window = Window::range(disk, 1 << 20, None).expect("fits");
    let mut fs = Fs::new(window).expect("an ext4 volume");
    let hello = fs
        .open(None, "hello.txt", OpenMode::read())
        .expect("a file");
    assert_eq!(read_all(&mut fs, hello), b"hello from ext4\n");
}

#[test]
fn metadata_links_and_relative_file_bases_obey_open_flags() {
    let mut fs = mount();
    let stat = OpenMode::from_flags(syscall::O_STAT | syscall::O_NOFOLLOW);
    let link = fs.open(None, "link", stat).expect("lstat the link");
    assert_eq!(fs.attributes(link).unwrap().kind, Kind::Symlink);
    assert_eq!(fs.attributes(link).unwrap().size, 9);
    assert_eq!(
        fs.read(link, 0, &mut [0; 16]).unwrap_err().errno(),
        syscall::EBADF
    );
    let mode = OpenMode {
        symlink: true,
        ..OpenMode::read()
    };
    assert_eq!(
        fs.open(None, "hello.txt", mode).unwrap_err().errno(),
        syscall::EINVAL
    );
    let file = fs.open(None, "dir/nested.txt", OpenMode::read()).unwrap();
    for path in ["../nested.txt", ".", ""] {
        assert_eq!(
            fs.open(Some(file), path, OpenMode::read())
                .unwrap_err()
                .errno(),
            syscall::ENOTDIR
        );
    }
    assert!(fs.open(Some(file), "/hello.txt", OpenMode::read()).is_ok());
    assert_eq!(
        fs.open(None, "hello.txt/../hello.txt", OpenMode::read())
            .unwrap_err()
            .errno(),
        syscall::ENOTDIR
    );
}

#[test]
fn unprivileged_opens_check_directory_search_and_final_read_permissions() {
    let mut fs = mount();
    assert!(
        fs.open_as(None, "hello.txt", OpenMode::read(), 2000, 2000)
            .is_ok()
    );
    for path in ["lost+found", "lost+found/../hello.txt", "lost+found/."] {
        assert_eq!(
            fs.open_as(None, path, OpenMode::read(), 2000, 2000)
                .unwrap_err()
                .errno(),
            syscall::EACCES
        );
    }
    // Metadata-only opens require search on parents, but no read on the final inode.
    let stat = OpenMode::from_flags(syscall::O_STAT);
    assert!(fs.open_as(None, "lost+found", stat, 2000, 2000).is_ok());
    assert_eq!(
        fs.open_as(None, "lost+found/../hello.txt", stat, 2000, 2000)
            .unwrap_err()
            .errno(),
        syscall::EACCES
    );
    assert!(
        fs.open_as(None, "dir/private.txt", OpenMode::read(), 1000, 2000)
            .is_ok()
    );
    assert!(
        fs.open_as(None, "dir/private.txt", OpenMode::read(), 2000, 100)
            .is_ok()
    );
    for path in [
        "dir/private.txt",
        "dir/private-link",
        "dir/private-dir/secret",
        "dir/secret-link",
        "dir/private-dir/../nested.txt",
    ] {
        assert_eq!(
            fs.open_as(None, path, OpenMode::read(), 2000, 2000)
                .unwrap_err()
                .errno(),
            syscall::EACCES,
            "{path}"
        );
        assert!(
            fs.open_as(None, path, OpenMode::read(), 0, 0).is_ok(),
            "root: {path}"
        );
    }
    // A symlink followed by .. must resolve relative to the target directory.
    let through = fs
        .open_as(
            None,
            "dir/root-link/hello.txt",
            OpenMode::read(),
            2000,
            2000,
        )
        .unwrap();
    assert_eq!(fs.path(through).unwrap(), "/hello.txt");
    let link_stat = OpenMode::from_flags(syscall::O_STAT | syscall::O_NOFOLLOW);
    assert!(
        fs.open_as(None, "dir/private-link", link_stat, 2000, 2000)
            .is_ok()
    );
}
