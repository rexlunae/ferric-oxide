//! Helpers the journal and write tests share: the fixture images, a device that stops
//! taking writes, as a crash would, an in-memory device a test can still read after handing
//! it over, and `e2fsck`.

#![allow(dead_code, reason = "each test binary uses some of the helpers")]

use std::cell::RefCell;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use ext4d::device::{ReadAt, WriteAt};

/// Where the fixtures are: `tests/fixtures/`, or the directory `EXT4D_FIXTURES` names (CI
/// makes fresh images there with `make-fixtures.sh`).
fn fixtures() -> PathBuf {
    std::env::var_os("EXT4D_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        PathBuf::from,
    )
}

/// The gzipped fixture `file`, unpacked.
pub fn load(file: &str) -> Vec<u8> {
    let gz = std::fs::File::open(fixtures().join(file)).expect("the fixture image");
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut bytes)
        .expect("a gzip file");
    bytes
}

/// The fixture `file`, which is not compressed (`basic.ext4`).
pub fn load_raw(file: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(file)).expect("the fixture image")
}

/// Finds a fixture path's inode; optional mkfs features can reserve additional inodes.
///
/// # Panics
///
/// Panics if the fixture cannot be opened or the path cannot be resolved.
pub fn inode_number(image: &[u8], path: &str) -> u32 {
    let window = ext4d::device::Window::whole(image.to_vec()).expect("its length");
    ext4d::lookup::inode_of(&window, path).expect("the fixture path")
}

/// A volume that stops taking writes after a number of them, as a crash would.
pub struct Crashing {
    /// The volume's bytes.
    pub bytes: Vec<u8>,
    /// Writes left before the device fails.
    pub writes_left: usize,
}

impl ReadAt for Crashing {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.bytes.read_exact_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        self.bytes.size()
    }
}

impl WriteAt for Crashing {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.writes_left = self
            .writes_left
            .checked_sub(1)
            .ok_or(io::ErrorKind::BrokenPipe)?;
        self.bytes.write_all_at(buf, offset)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Recomputes the volume superblock's checksum after a test changed it (`metadata_csum`).
pub fn reseal(image: &mut [u8]) {
    let crc = ferric_jbd2::crc32c(&[&image[1024..1024 + 0x3FC]]);
    image[1024 + 0x3FC..2048].copy_from_slice(&crc.to_le_bytes());
}

/// The little-endian `u32` at `at`.
pub fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

/// The byte offset of inode `number` in a volume whose inode is in group 0, from the
/// superblock and the first group descriptor ("Finding an Inode" in "Ext4 Disk Layout").
pub fn inode_at(image: &[u8], number: u32) -> usize {
    let sb = &image[1024..2048];
    let block_size = 1024usize << le32(sb, 0x18);
    let first_data_block = le32(sb, 0x14) as usize;
    let inode_size = usize::from(le16(sb, 0x58));
    let descriptor = (first_data_block + 1) * block_size;
    let table = le32(image, descriptor + 0x8) as usize;
    table * block_size + (number as usize - 1) * inode_size
}

/// Recompute an inode checksum after deliberately changing an on-disk test fixture.
pub fn reseal_inode(image: &mut [u8], number: u32) {
    if le32(image, 1024 + 0x64) & 0x400 == 0 {
        return;
    }
    let seed = if le32(image, 1024 + 0x60) & 0x2000 != 0 {
        le32(image, 1024 + 0x270)
    } else {
        ferric_jbd2::crc32c(&[&image[1024 + 0x68..1024 + 0x78]])
    };
    let at = inode_at(image, number);
    let size = usize::from(le16(image, 1024 + 0x58));
    let inode = &mut image[at..at + size];
    let high = size >= 0x84 && le16(inode, 0x80) >= 4;
    inode[0x7C..0x7E].fill(0);
    if high {
        inode[0x82..0x84].fill(0);
    }
    let checksum =
        ferric_jbd2::crc32c_seeded(seed, &[&number.to_le_bytes(), &inode[0x64..0x68], inode]);
    inode[0x7C..0x7E].copy_from_slice(&(checksum as u16).to_le_bytes());
    if high {
        inode[0x82..0x84].copy_from_slice(&((checksum >> 16) as u16).to_le_bytes());
    }
}

/// The little-endian `u16` at `at`.
pub fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// Has `e2fsck` (named by `EXT4D_E2FSCK`) replay and then check `image`, and returns the
/// volume it leaves. Without `EXT4D_E2FSCK` the image comes back as it is.
pub fn e2fsck(name: &str, image: Vec<u8>) -> Vec<u8> {
    let Some(e2fsck) = std::env::var_os("EXT4D_E2FSCK") else {
        return image;
    };
    let path = std::env::temp_dir().join(format!("ext4d-{}-{name}.ext4", std::process::id()));
    std::fs::write(&path, &image).expect("a temporary file");
    // The first run replays a journal that needs it (exit status 0, or 1 when it says it
    // changed the volume); the second must find nothing at all to fix.
    let replay = Command::new(&e2fsck)
        .args(["-f", "-y"])
        .arg(&path)
        .output()
        .expect("e2fsck runs");
    assert!(
        matches!(replay.status.code(), Some(0 | 1)),
        "{name}: e2fsck -fy: {replay:?}"
    );
    let check = Command::new(&e2fsck)
        .args(["-f", "-n"])
        .arg(&path)
        .output()
        .expect("e2fsck runs");
    assert_eq!(
        check.status.code(),
        Some(0),
        "{name}: e2fsck -fn: {check:?}"
    );
    let after = std::fs::read(&path).expect("the checked image");
    std::fs::remove_file(&path).expect("removed");
    after
}

/// Has `e2fsck -fn` (named by `EXT4D_E2FSCK`) check `image` as it is, without letting it
/// replay or fix anything first: it must find nothing wrong. Without `EXT4D_E2FSCK`, nothing
/// is checked.
pub fn e2fsck_clean(name: &str, image: &[u8]) {
    let Some(e2fsck) = std::env::var_os("EXT4D_E2FSCK") else {
        return;
    };
    let path = std::env::temp_dir().join(format!("ext4d-{}-{name}.ext4", std::process::id()));
    std::fs::write(&path, image).expect("a temporary file");
    let check = Command::new(&e2fsck)
        .args(["-f", "-n"])
        .arg(&path)
        .output()
        .expect("e2fsck runs");
    std::fs::remove_file(&path).expect("removed");
    assert_eq!(
        check.status.code(),
        Some(0),
        "{name}: e2fsck -fn: {}",
        String::from_utf8_lossy(&check.stdout)
    );
}

/// A volume in memory that stays reachable after it is handed to a [`ext4d::fs::Fs`], so a
/// test can look at the bytes it was left with.
#[derive(Clone, Default)]
pub struct SharedImage(pub Rc<RefCell<Vec<u8>>>);

impl SharedImage {
    /// The image `bytes`.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Rc::new(RefCell::new(bytes)))
    }

    /// A copy of the bytes as they are now.
    pub fn bytes(&self) -> Vec<u8> {
        self.0.borrow().clone()
    }
}

impl ReadAt for SharedImage {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.0.borrow().read_exact_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        self.0.borrow().size()
    }
}

impl WriteAt for SharedImage {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.0.borrow_mut().write_all_at(buf, offset)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The fields of the volume superblock (at byte 1024) that e2fsck sets and the replay does
/// not: `s_wtime` (0x30), `s_kbytes_written` (0x178) and, over those, `s_checksum` (0x3FC).
fn without_e2fsck_bookkeeping(mut image: Vec<u8>) -> Vec<u8> {
    for (at, len) in [(0x30, 4), (0x178, 8), (0x3FC, 4)] {
        image[1024 + at..1024 + at + len].fill(0);
    }
    image
}

/// Compares replay output with e2fsck, excluding its documented bookkeeping fields.
///
/// # Panics
///
/// Panics if either image lacks a superblock, or their sizes or normalized blocks differ.
pub fn assert_same_blocks(name: &str, ours: &[u8], theirs: &[u8]) {
    assert_eq!(ours.len(), theirs.len(), "{name}: image size");
    let ours = without_e2fsck_bookkeeping(ours.to_vec());
    let theirs = without_e2fsck_bookkeeping(theirs.to_vec());
    let differ: Vec<usize> = ours
        .chunks(1024)
        .zip(theirs.chunks(1024))
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect();
    assert!(
        differ.is_empty(),
        "{name}: {} KiB blocks differ from e2fsck replay; first blocks: {:?}",
        differ.len(),
        &differ[..differ.len().min(16)]
    );
}
