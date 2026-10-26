//! The reader on exFAT images `tests/fixtures/make-fixtures.sh` builds with `mkfs.exfat`
//! (see that script for what they hold).

use std::io::Read;
use std::path::PathBuf;

use ferric_blocklayer::Memory;
use ferric_exfat::time::Timestamp;
use ferric_exfat::{Cursor, Error, ExFat, Stream};

type Volume = ExFat<Memory<Vec<u8>>>;

const IMAGES: [(&str, u64); 2] = [("exfat-4k.img.gz", 4096), ("exfat-512.img.gz", 512)];

/// 2026-01-01 12:34:56 UTC, when every fixture file was last written.
const STAMP: i64 = 1_767_270_896;

/// The gzipped fixture `file`, unpacked: from `tests/fixtures/`, or from the directory
/// `EXFAT_FIXTURES` names (CI makes fresh images there with `make-fixtures.sh`).
fn load(file: &str) -> Vec<u8> {
    let dir = std::env::var_os("EXFAT_FIXTURES").map_or_else(
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

fn volumes() -> impl Iterator<Item = (u64, Volume)> {
    IMAGES.into_iter().map(|(file, cluster)| {
        let volume = ExFat::open(Memory::new(load(file))).expect("an exFAT volume");
        (cluster, volume)
    })
}

fn walk(volume: &Volume, path: &str) -> Stream {
    let mut stream = volume.root();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        stream = volume
            .find(&stream, part)
            .expect("a directory read")
            .unwrap_or_else(|| panic!("{part} in {path}"))
            .stream;
    }
    stream
}

fn read_all(volume: &Volume, path: &str) -> Vec<u8> {
    let stream = walk(volume, path);
    let mut buf = vec![0; stream.length as usize];
    let n = volume
        .read(&stream, &mut Cursor::default(), 0, &mut buf)
        .unwrap();
    buf.truncate(n);
    buf
}

fn names(volume: &Volume, path: &str) -> Vec<String> {
    let mut names: Vec<String> = volume
        .list(&walk(volume, path))
        .unwrap()
        .iter()
        .map(|e| e.name_lossy())
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
fn the_volume_mounts() {
    for (cluster, volume) in volumes() {
        assert_eq!(volume.boot_sector().cluster_size(), cluster);
        assert_eq!(volume.label(), "FERRIC");
        let free = volume.free_clusters().unwrap();
        let total = u64::from(volume.boot_sector().cluster_count);
        // big.bin alone takes 200 KiB; the rest a few hundred clusters more at most.
        let used = total - free;
        assert!(
            used * cluster > 200 * 1024 && used < 200 * 1024 / cluster + 400,
            "{used}"
        );
    }
}

#[test]
fn directories_list() {
    for (_, volume) in volumes() {
        assert_eq!(
            names(&volume, "/"),
            [
                "A Long File Name.txt",
                "EFI",
                "big.bin",
                "dir",
                "empty",
                "hello.txt",
                "many",
                "Ünïcødé.txt",
            ]
        );
        assert!(names(&volume, "/empty").is_empty());
        let many: Vec<String> = (0..200).map(|i| format!("f{i:03}")).collect();
        assert_eq!(names(&volume, "/many"), many);
    }
}

#[test]
fn files_read_back() {
    for (cluster, volume) in volumes() {
        assert_eq!(read_all(&volume, "/hello.txt"), b"hello from exFAT\n");
        assert_eq!(read_all(&volume, "/A Long File Name.txt"), b"long\n");
        assert_eq!(read_all(&volume, "/Ünïcødé.txt"), b"unicode\n");
        assert_eq!(read_all(&volume, "/dir/nested.txt"), b"nested\n");
        assert_eq!(read_all(&volume, "/many/f137"), b"f137\n");
        let efi = read_all(&volume, "/EFI/BOOT/BOOTX64.EFI");
        assert_eq!((efi.len(), &efi[..2]), (512, &b"MZ"[..]));
        assert_eq!(
            read_all(&volume, "/big.bin"),
            big(),
            "{cluster}-byte clusters"
        );
        assert!(
            !walk(&volume, "/big.bin").contiguous,
            "big.bin has a FAT chain"
        );
    }
}

#[test]
fn reads_at_offsets_follow_the_chain_with_a_cursor() {
    let want = big();
    for (_, volume) in volumes() {
        let stream = walk(&volume, "/big.bin");
        let mut cursor = Cursor::default();
        for offset in [150_000_u64, 1023, 0, 204_700, 4096, 99_999, 100_000] {
            let mut buf = [0; 777];
            let n = volume.read(&stream, &mut cursor, offset, &mut buf).unwrap();
            let start = offset as usize;
            let end = (start + 777).min(want.len());
            assert_eq!(&buf[..n], &want[start..end], "at {offset}");
        }
        assert_eq!(
            volume
                .read(&stream, &mut cursor, 300_000, &mut [0; 8])
                .unwrap(),
            0
        );
    }
}

#[test]
fn names_match_through_the_upcase_table() {
    for (_, volume) in volumes() {
        assert_eq!(read_all(&volume, "/efi/boot/bootx64.efi").len(), 512);
        assert_eq!(read_all(&volume, "/HELLO.TXT"), b"hello from exFAT\n");
        assert_eq!(read_all(&volume, "/üNÏCØDÉ.TXT"), b"unicode\n");
        let root = volume.root();
        assert!(volume.find(&root, "deleted.txt").unwrap().is_none());
        assert!(volume.find(&root, "between.bin").unwrap().is_none());
        assert!(volume.find(&root, "hello.tx").unwrap().is_none());
    }
}

#[test]
fn entries_carry_kinds_and_times() {
    for (_, volume) in volumes() {
        let root = volume.list(&volume.root()).unwrap();
        let by_name = |n: &str| root.iter().find(|e| e.name_lossy() == n).unwrap();
        assert!(by_name("dir").is_dir() && !by_name("hello.txt").is_dir());
        let hello = by_name("hello.txt");
        assert_eq!(hello.stream.length, 17);
        assert_eq!(hello.modified.to_unix().map(|t| t.0), Some(STAMP));
        assert_eq!(by_name("dir").modified.to_unix().map(|t| t.0), Some(STAMP));
        // The name hash the writer recorded matches the up-case table's.
        assert_eq!(volume.upcase().name_hash(&hello.name), hello.name_hash);
        let _: Timestamp = hello.accessed;
    }
}

#[test]
fn damage_is_detected() {
    let bytes = load("exfat-4k.img.gz");
    // A changed byte in the boot code no longer matches the boot checksum.
    let mut boot = bytes.clone();
    boot[200] ^= 1;
    assert!(matches!(
        ExFat::open(Memory::new(boot)),
        Err(Error::BootChecksum { .. })
    ));
    // Changing VolumeFlags, which the checksum skips, is fine.
    let mut flags = bytes.clone();
    flags[106] |= 2;
    let dirty = ExFat::open(Memory::new(flags)).unwrap();
    assert!(dirty.boot_sector().dirty);
    // A FAT image is not exFAT.
    let mut fat = bytes.clone();
    fat[3..11].copy_from_slice(b"MSDOS5.0");
    assert!(matches!(
        ExFat::open(Memory::new(fat)),
        Err(Error::BootSector(ferric_exfat::BootSectorError::NotExfat))
    ));
    // A device shorter than the volume.
    let short = bytes[..bytes.len() / 2].to_vec();
    assert!(matches!(
        ExFat::open(Memory::new(short)),
        Err(Error::Truncated { .. })
    ));
}
