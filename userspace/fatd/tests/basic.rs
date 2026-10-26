//! `fatd`'s file system layer on the FAT12, FAT16 and FAT32 images `make-fixtures.sh` builds
//! with `mkfs.fat` and `mtools` (see that script for what they hold).

use std::io::Read;
use std::path::PathBuf;

use fatd::Error;
use fatd::fs::{Fs, Kind, OpenMode, Volume, mount};
use ferric_blocklayer::{BlockDevice, Memory, Slice};
use syscall::error::{EBADF, EISDIR, ENOENT, ENOTDIR, EROFS};

type Image = Memory<Vec<u8>>;

const IMAGES: [(&str, u8); 3] = [
    ("fat12.img.gz", 12),
    ("fat16.img.gz", 16),
    ("fat32.img.gz", 32),
];

/// 2026-01-01 12:34:56 UTC, when every fixture file was last written.
const STAMP: i64 = 1_767_270_896;

/// The gzipped fixture `file`, unpacked: from `tests/fixtures/`, or from the directory
/// `FATD_FIXTURES` names (CI makes fresh images there with `make-fixtures.sh`).
fn load(file: &str) -> Vec<u8> {
    let dir = std::env::var_os("FATD_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        PathBuf::from,
    );
    let gz = std::fs::File::open(dir.join(file)).expect("the fixture image");
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut bytes)
        .expect("a gzip file");
    bytes
}

fn volumes() -> impl Iterator<Item = (u8, Volume<Image>)> {
    IMAGES.into_iter().map(|(file, bits)| {
        let volume = mount(Memory::new(load(file))).expect("a FAT volume");
        (bits, volume)
    })
}

fn read_all<D: fatd::disk::Device>(fs: &mut Fs<'_, D>, path: &str) -> Vec<u8> {
    let id = fs
        .open(None, path, OpenMode::read())
        .expect("the file opens");
    let mut out = Vec::new();
    let mut buf = [0; 3000];
    loop {
        let n = fs.read(id, out.len() as u64, &mut buf).expect("a read");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    fs.close(id);
    out
}

fn names<D: fatd::disk::Device>(fs: &mut Fs<'_, D>, path: &str) -> Vec<String> {
    let id = fs
        .open(None, path, OpenMode::read())
        .expect("the directory opens");
    let mut names: Vec<String> = fs
        .entries(id)
        .expect("a listing")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    names.sort();
    names
}

fn big() -> Vec<u8> {
    (0..200 * 1024)
        .map(|i: usize| ((i * 7 + i / 4096) % 251) as u8)
        .collect()
}

#[test]
fn each_fat_type_is_recognized() {
    for (bits, volume) in volumes() {
        let fs = Fs::new(&volume);
        assert_eq!(fs.fat_bits(), bits);
        assert_eq!(fs.label(), "FERRIC");
    }
}

#[test]
fn the_root_lists_long_and_short_names() {
    for (bits, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        assert_eq!(
            names(&mut fs, "/"),
            [
                "A Long File Name.txt",
                "EFI",
                "big.bin",
                "dir",
                "empty",
                "hello.txt",
                "many",
                "Ünïcødé.txt",
            ],
            "FAT{bits}"
        );
        assert!(names(&mut fs, "/empty").is_empty());
    }
}

#[test]
fn files_read_back() {
    for (bits, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        assert_eq!(read_all(&mut fs, "/hello.txt"), b"hello from FAT\n");
        assert_eq!(read_all(&mut fs, "/A Long File Name.txt"), b"long\n");
        assert_eq!(read_all(&mut fs, "/Ünïcødé.txt"), b"unicode\n");
        assert_eq!(read_all(&mut fs, "/dir/nested.txt"), b"nested\n");
        let efi = read_all(&mut fs, "/EFI/BOOT/BOOTX64.EFI");
        assert_eq!((efi.len(), &efi[..2]), (512, &b"MZ"[..]));
        assert_eq!(read_all(&mut fs, "/big.bin"), big(), "FAT{bits}");
    }
}

#[test]
fn reads_at_offsets_follow_the_cluster_chain() {
    let want = big();
    for (bits, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let id = fs.open(None, "big.bin", OpenMode::read()).unwrap();
        // Backwards, across clusters, and past the end.
        for offset in [150_000_u64, 1023, 0, 204_700, 4096, 99_999] {
            let mut buf = [0; 777];
            let n = fs.read(id, offset, &mut buf).unwrap();
            let start = offset as usize;
            let end = (start + 777).min(want.len());
            assert_eq!(&buf[..n], &want[start..end], "FAT{bits} at {offset}");
        }
        assert_eq!(fs.read(id, 300_000, &mut [0; 8]).unwrap(), 0);
    }
}

#[test]
fn a_directory_over_several_clusters_lists_every_entry() {
    for (_, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let want: Vec<String> = (0..200).map(|i| format!("f{i:03}")).collect();
        assert_eq!(names(&mut fs, "/many"), want);
        assert_eq!(read_all(&mut fs, "/many/f123"), b"f123\n");
        assert_eq!(read_all(&mut fs, "/many/f199"), b"f199\n");
    }
}

#[test]
fn names_match_without_case_and_by_short_name() {
    for (_, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let id = fs
            .open(None, "/efi/boot/bootx64.efi", OpenMode::read())
            .unwrap();
        assert_eq!(fs.path(id).unwrap(), "/EFI/BOOT/BOOTX64.EFI");
        let canonical = fs
            .open(None, "/EFI/BOOT/BOOTX64.EFI", OpenMode::read())
            .unwrap();
        assert_eq!(
            fs.attributes(id).unwrap().ino,
            fs.attributes(canonical).unwrap().ino
        );
        let short = fs.open(None, "/ALONGF~1.TXT", OpenMode::read()).unwrap();
        assert_eq!(fs.path(short).unwrap(), "/A Long File Name.txt");
        assert_eq!(read_all(&mut fs, "/HELLO.TXT"), b"hello from FAT\n");
        assert_eq!(read_all(&mut fs, "/üNÏcødÉ.TXT"), b"unicode\n");
    }
}

#[test]
fn relative_opens_and_dot_dot_resolve() {
    for (_, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let root = fs.root().unwrap();
        let dir = fs.open(Some(root), "dir", OpenMode::read()).unwrap();
        let nested = fs.open(Some(dir), "nested.txt", OpenMode::read()).unwrap();
        assert_eq!(fs.path(nested).unwrap(), "/dir/nested.txt");
        let up = fs
            .open(Some(dir), "../EFI/./BOOT/../BOOT", OpenMode::read())
            .unwrap();
        assert_eq!(fs.path(up).unwrap(), "/EFI/BOOT");
        let top = fs.open(Some(dir), "../../..", OpenMode::read()).unwrap();
        assert_eq!(fs.path(top).unwrap(), "/");
        // An absolute path ignores the base.
        let abs = fs.open(Some(dir), "/hello.txt", OpenMode::read()).unwrap();
        assert_eq!(fs.path(abs).unwrap(), "/hello.txt");
        // A relative open from a file is refused.
        assert_eq!(
            fs.open(Some(nested), "x", OpenMode::read())
                .unwrap_err()
                .errno(),
            ENOTDIR
        );
    }
}

#[test]
fn attributes_describe_files_and_directories() {
    for (bits, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let cluster = if bits == 12 { 1024 } else { 512 };
        let file = fs.open(None, "/big.bin", OpenMode::read()).unwrap();
        let a = fs.attributes(file).unwrap();
        assert_eq!(
            (a.kind, a.size, a.cluster_size),
            (Kind::File, 204_800, cluster)
        );
        assert_eq!(a.modified, Some((STAMP, 0)), "FAT{bits}");
        let st = a.to_stat();
        assert_eq!((st.st_mode, st.st_uid, st.st_gid), (0o100_444, 0, 0));
        assert_eq!(st.st_mtime, STAMP as u64);
        // FAT keeps only the access date.
        let accessed = a.accessed.expect("an access date").0;
        assert_eq!(accessed % 86_400, 0);
        let dir = fs.open(None, "/dir", OpenMode::read()).unwrap();
        let d = fs.attributes(dir).unwrap();
        assert_eq!((d.kind, d.to_stat().st_mode), (Kind::Directory, 0o040_555));
        assert_eq!(d.modified, Some((STAMP, 0)));
        let root = fs.root().unwrap();
        assert_eq!(fs.attributes(root).unwrap().to_stat().st_ino, 2);
        let listed = fs.entries(root).unwrap();
        let entry = listed.iter().find(|e| e.name == "big.bin").unwrap();
        assert_eq!((entry.kind, entry.ino), (Kind::File, a.ino));
    }
}

#[test]
fn geometry_counts_clusters() {
    for (bits, volume) in volumes() {
        let fs = Fs::new(&volume);
        let g = fs.geometry().unwrap();
        let used = g.clusters - g.free_clusters;
        // big.bin alone takes 200 or 400 clusters; the rest take a few dozen more.
        let big = 204_800 / u64::from(g.cluster_size);
        assert!(used > big && used < big + 300, "FAT{bits}: {g:?}");
        assert_eq!(g.to_statvfs().f_bavail, g.free_clusters);
    }
}

#[test]
fn missing_paths_and_wrong_kinds_are_refused() {
    for (_, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let read = OpenMode::read();
        let errno = |r: Result<usize, Error>| r.unwrap_err().errno();
        assert_eq!(errno(fs.open(None, "/nope", read)), ENOENT);
        assert_eq!(errno(fs.open(None, "/dir/nope", read)), ENOENT);
        assert_eq!(errno(fs.open(None, "/hello.txt/x", read)), ENOTDIR);
        let dir_only = OpenMode {
            directory: true,
            ..read
        };
        assert_eq!(errno(fs.open(None, "/hello.txt", dir_only)), ENOTDIR);
        // Deleted entries are not found.
        assert_eq!(errno(fs.open(None, "/gap.bin", read)), ENOENT);
        assert_eq!(errno(fs.open(None, "/after.bin", read)), ENOENT);
        let dir = fs.open(None, "/dir", read).unwrap();
        assert_eq!(fs.read(dir, 0, &mut [0; 4]).unwrap_err().errno(), EISDIR);
        let file = fs.open(None, "/hello.txt", read).unwrap();
        assert_eq!(fs.entries(file).unwrap_err().errno(), ENOTDIR);
        fs.close(file);
        assert_eq!(fs.attributes(file).unwrap_err().errno(), EBADF);
    }
}

#[test]
fn nothing_can_change_the_volume() {
    for (_, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let read = OpenMode::read();
        for mode in [
            OpenMode {
                write: true,
                ..read
            },
            OpenMode {
                create_or_truncate: true,
                ..read
            },
        ] {
            let err = fs.open(None, "/hello.txt", mode).unwrap_err();
            assert_eq!(err.errno(), EROFS);
            assert_eq!(fs.open(None, "/new.txt", mode).unwrap_err().errno(), EROFS);
        }
        let file = fs.open(None, "/hello.txt", read).unwrap();
        assert_eq!(fs.refuse_change(file).unwrap_err().errno(), EROFS);
        assert_eq!(fs.refuse_change(999).unwrap_err().errno(), EBADF);
    }
}

#[test]
fn stat_only_opens_do_not_read() {
    for (_, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let stat = OpenMode {
            stat: true,
            ..OpenMode::read()
        };
        let id = fs.open(None, "/hello.txt", stat).unwrap();
        assert_eq!(fs.attributes(id).unwrap().size, 15);
        assert_eq!(fs.read(id, 0, &mut [0; 4]).unwrap_err().errno(), EBADF);
    }
}

#[test]
fn a_partition_is_read_through_a_slice() {
    // The FAT16 volume 1 MiB into a bigger disk, as a partition would be.
    let volume_bytes = load("fat16.img.gz");
    let offset = 1 << 20;
    let mut disk = vec![0xee; offset];
    disk.extend_from_slice(&volume_bytes);
    disk.extend_from_slice(&[0xee; 4096]);
    let partition = Slice::new(Memory::new(disk), offset as u64, volume_bytes.len() as u64)
        .expect("the window fits");
    assert_eq!(partition.size(), volume_bytes.len() as u64);
    let volume = mount(partition).expect("a FAT volume");
    let mut fs = Fs::new(&volume);
    assert_eq!(read_all(&mut fs, "/dir/nested.txt"), b"nested\n");
}

#[test]
fn other_bytes_are_not_mounted() {
    let err = mount(Memory::new(vec![0; 1 << 20]))
        .err()
        .expect("no volume");
    assert!(matches!(err, Error::Mount(_)));
    let unsigned = load("fat16.img.gz")
        .into_iter()
        .enumerate()
        .map(|(i, b)| if i == 510 { 0 } else { b })
        .collect::<Vec<u8>>();
    // Without the boot signature, the boot sector is refused.
    assert!(matches!(mount(Memory::new(unsigned)), Err(Error::Mount(_))));
}

#[test]
fn components_resolve_in_order() {
    for (_, volume) in volumes() {
        let mut fs = Fs::new(&volume);
        let read = OpenMode::read();
        let errno = |r: Result<usize, Error>| r.unwrap_err().errno();
        // `..` after a file, or after a name that is not there, is not skipped as text.
        assert_eq!(errno(fs.open(None, "/hello.txt/../EFI", read)), ENOTDIR);
        assert_eq!(errno(fs.open(None, "/hello.txt/.", read)), ENOTDIR);
        assert_eq!(errno(fs.open(None, "/hello.txt/", read)), ENOTDIR);
        assert_eq!(errno(fs.open(None, "/missing/../EFI", read)), ENOENT);
        // A `..` after a directory goes back up.
        let up = fs.open(None, "/dir/../EFI/BOOT", read).unwrap();
        assert_eq!(fs.path(up).unwrap(), "/EFI/BOOT");
        let dir = fs.open(None, "/dir", read).unwrap();
        let file = fs.open(None, "/hello.txt", read).unwrap();
        // A relative open from a file's handle is refused before any lookup.
        assert_eq!(errno(fs.open(Some(file), "../EFI", read)), ENOTDIR);
        let back = fs.open(Some(dir), "../dir/./nested.txt", read).unwrap();
        assert_eq!(fs.path(back).unwrap(), "/dir/nested.txt");
    }
}

#[test]
fn the_length_limit_counts_the_base_directory() {
    use fatd::fs::PATH_MAX;
    use syscall::error::ENAMETOOLONG;
    let volume = mount(Memory::new(load("fat16.img.gz"))).unwrap();
    let mut fs = Fs::new(&volume);
    let read = OpenMode::read();
    // 4092 bytes of `./`: within the limit from the root, over it from `/dir`.
    let relative = "./".repeat(2046);
    let root = fs.root().unwrap();
    let dir = fs.open(None, "/dir", read).unwrap();
    assert!(1 + 1 + relative.len() <= PATH_MAX);
    let same = fs.open(Some(root), &relative, read).unwrap();
    assert_eq!(fs.path(same).unwrap(), "/");
    assert!("/dir".len() + 1 + relative.len() > PATH_MAX);
    let err = fs.open(Some(dir), &relative, read).unwrap_err();
    assert_eq!(err.errno(), ENAMETOOLONG);
    let absolute = format!("/{}", "x".repeat(PATH_MAX));
    assert_eq!(
        fs.open(Some(dir), &absolute, read).unwrap_err().errno(),
        ENAMETOOLONG
    );
}
