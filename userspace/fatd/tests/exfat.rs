//! `fatd`'s file system layer on the exFAT images of `crates/exfat/tests/fixtures/`, which
//! hold the same tree as the FAT images (see `make-fixtures.sh` there). Without the `exfat`
//! feature, only the refusal to mount them is tested.

use std::io::Read;
use std::path::PathBuf;

use fatd::Error;
use fatd::fs::mount;
use ferric_blocklayer::Memory;

/// The gzipped exFAT fixture `file`, unpacked: from `crates/exfat/tests/fixtures/`, or from
/// the directory `EXFAT_FIXTURES` names (CI makes fresh images there).
fn load(file: &str) -> Vec<u8> {
    let dir = std::env::var_os("EXFAT_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/exfat/tests/fixtures"),
        PathBuf::from,
    );
    let gz = std::fs::File::open(dir.join(file)).expect("the fixture image");
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut bytes)
        .expect("a gzip file");
    bytes
}

#[cfg(not(feature = "exfat"))]
#[test]
fn exfat_needs_the_feature() {
    use syscall::error::EOPNOTSUPP;
    let err = mount(Memory::new(load("exfat-4k.img.gz")))
        .err()
        .expect("no volume");
    assert!(matches!(err, Error::ExFatDisabled));
    assert_eq!(err.errno(), EOPNOTSUPP);
}

#[cfg(feature = "exfat")]
mod with_exfat {
    use super::*;

    use fatd::disk::Device;
    use fatd::fs::{Format, Fs, Kind, OpenMode, Volume};
    use syscall::error::{EBADF, EINVAL, EISDIR, ENOENT, ENOTDIR, EROFS};

    type Image = Memory<Vec<u8>>;

    const IMAGES: [(&str, u32); 2] = [("exfat-4k.img.gz", 4096), ("exfat-512.img.gz", 512)];

    /// 2026-01-01 12:34:56 UTC, when every fixture file was last written.
    const STAMP: i64 = 1_767_270_896;

    fn volumes() -> impl Iterator<Item = (u32, Volume<Image>)> {
        IMAGES.into_iter().map(|(file, cluster)| {
            let volume = mount(Memory::new(load(file))).expect("an exFAT volume");
            (cluster, volume)
        })
    }

    fn read_all<D: Device>(fs: &mut Fs<'_, D>, path: &str) -> Vec<u8> {
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

    fn names<D: Device>(fs: &mut Fs<'_, D>, path: &str) -> Vec<String> {
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
    fn the_volume_mounts_as_exfat() {
        for (cluster, volume) in volumes() {
            let fs = Fs::new(&volume);
            assert_eq!(fs.format(), Format::ExFat);
            assert_eq!(fs.format().name(), "exFAT");
            assert_eq!(fs.label(), "FERRIC");
            let g = fs.geometry().unwrap();
            assert_eq!(g.cluster_size, cluster);
            let used = (g.clusters - g.free_clusters) * u64::from(cluster);
            // big.bin and the 200 small files, a cluster each, and a few more.
            assert!(used > 204_800 && used < 204_800 + 230 * u64::from(cluster) + 65_536);
        }
    }

    #[test]
    fn the_tree_lists_and_reads_back() {
        for (cluster, volume) in volumes() {
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
                "{cluster}"
            );
            assert!(names(&mut fs, "/empty").is_empty());
            let want: Vec<String> = (0..200).map(|i| format!("f{i:03}")).collect();
            assert_eq!(names(&mut fs, "/many"), want);
            assert_eq!(read_all(&mut fs, "/hello.txt"), b"hello from exFAT\n");
            assert_eq!(read_all(&mut fs, "/A Long File Name.txt"), b"long\n");
            assert_eq!(read_all(&mut fs, "/Ünïcødé.txt"), b"unicode\n");
            assert_eq!(read_all(&mut fs, "/dir/nested.txt"), b"nested\n");
            assert_eq!(read_all(&mut fs, "/many/f199"), b"f199\n");
            let efi = read_all(&mut fs, "/EFI/BOOT/BOOTX64.EFI");
            assert_eq!((efi.len(), &efi[..2]), (512, &b"MZ"[..]));
            assert_eq!(read_all(&mut fs, "/big.bin"), big(), "{cluster}");
        }
    }

    #[test]
    fn reads_at_offsets_follow_the_cluster_chain() {
        let want = big();
        for (cluster, volume) in volumes() {
            let mut fs = Fs::new(&volume);
            let id = fs.open(None, "big.bin", OpenMode::read()).unwrap();
            for offset in [150_000_u64, 1023, 0, 204_700, 4096, 99_999, 102_400] {
                let mut buf = [0; 777];
                let n = fs.read(id, offset, &mut buf).unwrap();
                let start = offset as usize;
                let end = (start + 777).min(want.len());
                assert_eq!(&buf[..n], &want[start..end], "{cluster} at {offset}");
            }
            assert_eq!(fs.read(id, 300_000, &mut [0; 8]).unwrap(), 0);
        }
    }

    #[test]
    fn names_match_through_the_upcase_table() {
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
            assert_eq!(read_all(&mut fs, "/HELLO.TXT"), b"hello from exFAT\n");
            assert_eq!(read_all(&mut fs, "/üNÏcødÉ.TXT"), b"unicode\n");
        }
    }

    #[test]
    fn components_resolve_in_order() {
        for (_, volume) in volumes() {
            let mut fs = Fs::new(&volume);
            let read = OpenMode::read();
            let errno = |r: Result<usize, Error>| r.unwrap_err().errno();
            assert_eq!(errno(fs.open(None, "/hello.txt/../EFI", read)), ENOTDIR);
            assert_eq!(errno(fs.open(None, "/missing/../EFI", read)), ENOENT);
            // Deleted entries are not found.
            assert_eq!(errno(fs.open(None, "/deleted.txt", read)), ENOENT);
            assert_eq!(errno(fs.open(None, "/between.bin", read)), ENOENT);
            let dir = fs.open(None, "/dir", read).unwrap();
            let up = fs.open(Some(dir), "../EFI/./BOOT", read).unwrap();
            assert_eq!(fs.path(up).unwrap(), "/EFI/BOOT");
            let top = fs.open(Some(dir), "../../..", read).unwrap();
            assert_eq!(fs.path(top).unwrap(), "/");
        }
    }

    #[test]
    fn attributes_carry_the_full_times() {
        for (cluster, volume) in volumes() {
            let mut fs = Fs::new(&volume);
            let file = fs.open(None, "/big.bin", OpenMode::read()).unwrap();
            let a = fs.attributes(file).unwrap();
            assert_eq!(
                (a.kind, a.size, a.cluster_size),
                (Kind::File, 204_800, cluster)
            );
            assert_eq!(a.modified, Some((STAMP, 0)));
            // exFAT keeps the access time, not only the date.
            assert_eq!(a.accessed, Some((STAMP, 0)));
            let st = a.to_stat();
            assert_eq!((st.st_mode, st.st_mtime), (0o100_444, STAMP as u64));
            let dir = fs.open(None, "/dir", OpenMode::read()).unwrap();
            let d = fs.attributes(dir).unwrap();
            assert_eq!((d.kind, d.to_stat().st_mode), (Kind::Directory, 0o040_555));
            assert_eq!(d.size % u64::from(cluster), 0);
            let root = fs.root().unwrap();
            let listed = fs.entries(root).unwrap();
            let entry = listed.iter().find(|e| e.name == "big.bin").unwrap();
            assert_eq!((entry.kind, entry.ino), (Kind::File, a.ino));
        }
    }

    #[test]
    fn nothing_can_change_the_volume() {
        for (_, volume) in volumes() {
            let mut fs = Fs::new(&volume);
            let read = OpenMode::read();
            let write = OpenMode {
                write: true,
                ..read
            };
            assert_eq!(
                fs.open(None, "/hello.txt", write).unwrap_err().errno(),
                EROFS
            );
            let file = fs.open(None, "/hello.txt", read).unwrap();
            assert_eq!(fs.refuse_change(file).unwrap_err().errno(), EROFS);
            let dir = fs.open(None, "/dir", read).unwrap();
            assert_eq!(fs.read(dir, 0, &mut [0; 4]).unwrap_err().errno(), EISDIR);
            assert_eq!(fs.entries(file).unwrap_err().errno(), ENOTDIR);
            fs.close(file);
            assert_eq!(fs.attributes(file).unwrap_err().errno(), EBADF);
        }
    }

    #[test]
    fn a_damaged_boot_region_is_refused() {
        let mut bytes = load("exfat-4k.img.gz");
        // A byte the boot checksum covers.
        bytes[120] ^= 0xFF;
        let err = mount(Memory::new(bytes)).err().expect("no volume");
        assert!(matches!(err, Error::ExFat(_)));
        assert_eq!(err.errno(), EINVAL);
    }
}
