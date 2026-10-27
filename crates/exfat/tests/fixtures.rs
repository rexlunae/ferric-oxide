//! The reader on exFAT images `tests/fixtures/make-fixtures.sh` builds with `mkfs.exfat`
//! (see that script for what they hold).

mod support;

use simple_endian::{FromSlice, LittleEndian};
use std::io::Read;
use std::path::PathBuf;
use support::encode;

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

/// Writes `value` as the FAT entry of `cluster` in `bytes`, the image `volume` was read from.
fn set_fat(bytes: &mut [u8], volume: &Volume, cluster: u32, value: u32) {
    let at = (volume.boot_sector().fat_start() + u64::from(cluster) * 4) as usize;
    bytes[at..at + 4].copy_from_slice(&encode(LittleEndian::from(value)));
}

#[test]
fn chains_that_loop_or_end_early_are_errors() {
    let bytes = load("exfat-4k.img.gz");
    let volume = ExFat::open(Memory::new(bytes.clone())).unwrap();
    let big = walk(&volume, "/big.bin");
    assert!(!big.contiguous);
    let first = big.first_cluster;
    let second = {
        let mut cursor = Cursor::default();
        let mut one = [0];
        volume.read(&big, &mut cursor, 4096, &mut one).unwrap();
        // The second cluster's first byte is the pattern's, so the chain was followed.
        assert_eq!(one[0], big_byte(4096));
        let at = (volume.boot_sector().fat_start() + u64::from(first) * 4) as usize;
        LittleEndian::<u32>::read_from_slice(&bytes[at..at + 4])
            .unwrap()
            .to_native()
    };
    let broken = |change: &dyn Fn(&mut Vec<u8>)| {
        let mut image = bytes.clone();
        change(&mut image);
        ExFat::open(Memory::new(image)).unwrap()
    };
    // The first cluster points at itself, or the second back at the first: either way the
    // chain never ends, and no read returns the repeated cluster.
    for (cluster, next) in [(first, first), (second, first)] {
        let looped = broken(&|image| set_fat(image, &volume, cluster, next));
        for offset in [0, 4096, 8192] {
            let err = looped
                .read(&big, &mut Cursor::default(), offset, &mut [0; 16])
                .unwrap_err();
            assert!(
                matches!(err, Error::Chain { first: f } if f == first),
                "{err:?}"
            );
        }
    }
    // The chain ends after one cluster, shorter than the file.
    let short = broken(&|image| set_fat(image, &volume, first, 0xFFFF_FFFF));
    let err = short
        .read(&big, &mut Cursor::default(), 0, &mut [0; 16])
        .unwrap_err();
    assert!(matches!(err, Error::Chain { .. }), "{err:?}");
}

fn big_byte(i: usize) -> u8 {
    ((i * 7 + i / 4096) % 251) as u8
}

#[test]
fn a_bitmap_shorter_than_the_heap_is_refused() {
    let bytes = load("exfat-4k.img.gz");
    let volume = ExFat::open(Memory::new(bytes.clone())).unwrap();
    let boot = *volume.boot_sector();
    let needed = u64::from(boot.cluster_count).div_ceil(8);
    // The Allocation Bitmap entry (type 0x81) in the root directory's first cluster.
    let root = boot.cluster_start(boot.root_cluster) as usize;
    let entry = (0..boot.cluster_size() as usize / 32)
        .map(|i| root + i * 32)
        .find(|&at| bytes[at] == 0x81)
        .expect("a bitmap entry");
    let mut short = bytes.clone();
    short[entry + 24..entry + 32].copy_from_slice(&encode(LittleEndian::from(needed / 2)));
    let err = ExFat::open(Memory::new(short)).unwrap_err();
    assert!(
        matches!(err, Error::Bitmap { length, needed: n, .. } if length == needed / 2 && n == needed),
        "{err:?}"
    );
}

/// A device of `size` bytes whose first bytes are `prefix` and the rest zeros, so a volume
/// far larger than memory can be described without allocating it.
#[derive(Debug)]
struct Sparse {
    prefix: Vec<u8>,
    size: u64,
}

impl ferric_blocklayer::BlockDevice for Sparse {
    type Error = ferric_blocklayer::OutOfRange;

    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        ferric_blocklayer::OutOfRange::check(offset, buf.len(), self.size)?;
        for (i, b) in buf.iter_mut().enumerate() {
            let at = offset as usize + i;
            *b = self.prefix.get(at).copied().unwrap_or(0);
        }
        Ok(())
    }
}

#[test]
fn a_root_directory_over_256_mib_is_refused_before_it_is_read() {
    use ferric_exfat::boot::{CHECKSUMMED_SECTORS, SIGNATURE, checksum};
    // 512-byte sectors and 32 MiB clusters: nine clusters of root directory are 288 MiB.
    let (fat_offset, heap_offset, clusters) = (24u32, 32u32, 9u32);
    let volume_sectors = u64::from(heap_offset) + u64::from(clusters) * 65_536;
    let mut image = vec![0u8; heap_offset as usize * 512];
    let b = &mut image[..512];
    b[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    b[3..11].copy_from_slice(SIGNATURE);
    b[72..80].copy_from_slice(&encode(LittleEndian::from(volume_sectors)));
    b[80..84].copy_from_slice(&encode(LittleEndian::from(fat_offset)));
    b[84..88].copy_from_slice(&encode(LittleEndian::from(8u32)));
    b[88..92].copy_from_slice(&encode(LittleEndian::from(heap_offset)));
    b[92..96].copy_from_slice(&encode(LittleEndian::from(clusters)));
    b[96..100].copy_from_slice(&encode(LittleEndian::from(2u32)));
    b[104..106].copy_from_slice(&encode(LittleEndian::from(0x0100u16)));
    b[108] = 9;
    b[109] = 16;
    b[110] = 1;
    b[510] = 0x55;
    b[511] = 0xAA;
    let sum = (0..CHECKSUMMED_SECTORS).fold(0, |sum, i| {
        let i = i as usize;
        checksum(sum, i as u64, &image[i * 512..(i + 1) * 512])
    });
    for word in image[11 * 512..12 * 512].chunks_exact_mut(4) {
        word.copy_from_slice(&encode(LittleEndian::from(sum)));
    }
    // The root chain: clusters 2 to 10, one after another, then the end.
    let fat = fat_offset as usize * 512;
    for cluster in 2..=10u32 {
        let next = if cluster == 10 {
            0xFFFF_FFFF
        } else {
            cluster + 1
        };
        let at = fat + cluster as usize * 4;
        image[at..at + 4].copy_from_slice(&encode(LittleEndian::from(next)));
    }
    let device = Sparse {
        prefix: image,
        size: volume_sectors * 512,
    };
    let err = ExFat::open(device).unwrap_err();
    assert!(
        matches!(err, Error::DirectoryTooLarge { length } if length == 9 << 25),
        "{err:?}"
    );
}

#[test]
fn invalid_upcase_lengths_are_rejected_before_reading_the_payload() {
    use ferric_blocklayer::{BlockDevice, OutOfRange};
    use ferric_exfat::UpcaseError;

    #[derive(Debug)]
    struct Guarded {
        disk: Memory<Vec<u8>>,
        payload: std::ops::Range<u64>,
    }
    impl BlockDevice for Guarded {
        type Error = OutOfRange;
        fn size(&self) -> u64 {
            self.disk.size()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
            assert!(
                offset + buf.len() as u64 <= self.payload.start || offset >= self.payload.end,
                "an invalid upcase stream must be rejected before its payload is read"
            );
            self.disk.read_at(offset, buf)
        }
    }
    for (file, _) in IMAGES {
        let bytes = load(file);
        let boot = ferric_exfat::BootSector::parse(bytes[..512].try_into().unwrap()).unwrap();
        let root = boot.cluster_start(boot.root_cluster) as usize;
        let entry = (root..root + boot.cluster_size() as usize)
            .step_by(32)
            .find(|&at| bytes[at] == 0x82)
            .expect("the upcase entry");
        let first = LittleEndian::<u32>::read_from_slice(&bytes[entry + 20..entry + 24])
            .unwrap()
            .to_native();
        let payload = boot.cluster_start(first)..boot.cluster_start(first) + boot.cluster_size();
        for len in [131_074u64, 131_071, 1 << 30, u64::MAX] {
            let mut damaged = bytes.clone();
            damaged[entry + 24..entry + 32].copy_from_slice(&encode(LittleEndian::from(len)));
            let err = ExFat::open(Guarded {
                disk: Memory::new(damaged),
                payload: payload.clone(),
            })
            .unwrap_err();
            assert!(
                matches!(err, Error::Upcase(UpcaseError::Length { len: found }) if found == len),
                "{file}, {len}: {err:?}"
            );
        }
    }
}
