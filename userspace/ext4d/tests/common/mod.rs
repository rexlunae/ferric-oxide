//! Helpers the journal tests share: the fixture images, and a device that stops taking
//! writes, as a crash would.

#![allow(dead_code, reason = "each test binary uses some of the helpers")]

use std::io::{self, Read};
use std::path::PathBuf;

use ext4d::device::{ReadAt, WriteAt};

/// The gzipped fixture `file`, unpacked: from `tests/fixtures/`, or from the directory
/// `EXT4D_FIXTURES` names (CI makes fresh images there with `make-fixtures.sh`).
pub fn load(file: &str) -> Vec<u8> {
    let dir = std::env::var_os("EXT4D_FIXTURES").map_or_else(
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
