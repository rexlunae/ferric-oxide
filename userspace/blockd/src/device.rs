//! Devices and image files as block devices.
//!
//! The layers read any byte range, but a Redox disk scheme (`/scheme/disk.*`) only serves
//! reads that start on a block boundary and cover whole blocks: it turns the offset into a
//! block number by division and refuses other lengths with `EINVAL`. LVM metadata text, for
//! one, has whatever length it has. So [`Aligned`] widens every read to the device's blocks
//! (`st_blksize`), reading the aligned middle of a request straight into the caller's buffer
//! and a partial first or last block through a one-block bounce buffer. Image files take the
//! same path; for them it costs at most two extra partial blocks a read.

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use ferric_blocklayer::BlockDevice;

/// The block size assumed when a device reports none.
const DEFAULT_BLOCK: u64 = 512;

/// Something that reads whole runs at an offset, possibly only block-aligned ones.
pub trait ReadAt {
    /// Fills all of `buf` from byte `offset`.
    ///
    /// # Errors
    ///
    /// The I/O error, including `UnexpectedEof` when the source ends first.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

impl ReadAt for File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }
}

/// A source read only in whole, aligned blocks, served as a byte-addressed block device.
#[derive(Debug)]
pub struct Aligned<R> {
    inner: R,
    block: u64,
    size: u64,
}

impl<R: ReadAt> Aligned<R> {
    /// Serves the `size` bytes of `inner`, read in aligned blocks of `block` bytes (0 means
    /// 512).
    pub fn new(inner: R, block: u64, size: u64) -> Self {
        let block = if block == 0 { DEFAULT_BLOCK } else { block };
        Self { inner, block, size }
    }

    /// The block size reads are aligned to.
    pub fn block_size(&self) -> u64 {
        self.block
    }

    /// Reads the block that holds byte `offset` into `bounce` and copies its part of
    /// `offset..` into `buf`; returns how many bytes were copied.
    fn read_partial(&self, offset: u64, buf: &mut [u8], bounce: &mut [u8]) -> io::Result<usize> {
        let start = offset - offset % self.block;
        // The last block of a device whose size is not whole blocks is short.
        let len = usize::try_from((self.size - start).min(self.block))
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let bounce = &mut bounce[..len];
        self.inner.read_exact_at(bounce, start)?;
        let within = (offset - start) as usize;
        let n = (len - within).min(buf.len());
        buf[..n].copy_from_slice(&bounce[within..within + n]);
        Ok(n)
    }
}

impl<R: ReadAt> BlockDevice for Aligned<R> {
    type Error = io::Error;

    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
        let end = u64::try_from(buf.len())
            .ok()
            .and_then(|len| offset.checked_add(len));
        if end.is_none_or(|end| end > self.size) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let block = usize::try_from(self.block)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut bounce = Vec::new();
        while !buf.is_empty() {
            let whole = buf.len() - buf.len() % block;
            let done = if offset.is_multiple_of(self.block) && whole > 0 {
                self.inner.read_exact_at(&mut buf[..whole], offset)?;
                whole
            } else {
                if bounce.is_empty() {
                    bounce = vec![0; block];
                }
                self.read_partial(offset, buf, &mut bounce)?
            };
            buf = &mut buf[done..];
            offset += done as u64;
        }
        Ok(())
    }
}

/// An open disk, partition or image file, with the size and block size it had when opened.
pub type FileDevice = Aligned<File>;

impl Aligned<File> {
    /// Opens `path` for reading.
    ///
    /// # Errors
    ///
    /// The I/O error from opening the file or asking for its size.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        Ok(Self::new(file, meta.blksize(), meta.len()))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A disk-scheme stand-in: refuses reads that are not whole, aligned blocks, as
    /// driver-block does, and counts the reads it serves.
    struct Disk {
        bytes: Vec<u8>,
        block: usize,
        reads: Cell<usize>,
    }

    impl ReadAt for Disk {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
            let start = offset as usize;
            let last_block = start + buf.len() == self.bytes.len();
            if !start.is_multiple_of(self.block)
                || (!buf.len().is_multiple_of(self.block) && !last_block)
            {
                return Err(io::Error::from_raw_os_error(syscall::error::EINVAL));
            }
            let src = self
                .bytes
                .get(start..start + buf.len())
                .ok_or(io::ErrorKind::UnexpectedEof)?;
            buf.copy_from_slice(src);
            self.reads.set(self.reads.get() + 1);
            Ok(())
        }
    }

    fn disk(len: usize, block: usize) -> Aligned<Disk> {
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        Aligned::new(
            Disk {
                bytes,
                block,
                reads: Cell::new(0),
            },
            block as u64,
            len as u64,
        )
    }

    fn read(d: &Aligned<Disk>, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0; len];
        d.read_at(offset, &mut buf)?;
        Ok(buf)
    }

    fn want(offset: usize, len: usize) -> Vec<u8> {
        (offset..offset + len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn unaligned_reads_become_whole_block_reads() {
        let d = disk(64 * 1024, 4096);
        // An LVM metadata text: 2300 bytes from inside a block.
        assert_eq!(read(&d, 4096 + 512, 2300).unwrap(), want(4096 + 512, 2300));
        assert_eq!(read(&d, 100, 9000).unwrap(), want(100, 9000));
        assert_eq!(read(&d, 4095, 2).unwrap(), want(4095, 2));
        assert_eq!(read(&d, 0, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn the_aligned_middle_is_one_direct_read() {
        let d = disk(64 * 1024, 4096);
        assert_eq!(read(&d, 1000, 3 * 4096).unwrap(), want(1000, 3 * 4096));
        // Head block, two whole blocks straight into the buffer, tail block.
        assert_eq!(d.inner.reads.get(), 3);
        assert_eq!(read(&d, 8192, 8192).unwrap(), want(8192, 8192));
        assert_eq!(d.inner.reads.get(), 4);
    }

    #[test]
    fn a_short_last_block_and_the_end_are_handled() {
        let d = disk(10_000, 4096);
        assert_eq!(read(&d, 9000, 1000).unwrap(), want(9000, 1000));
        assert_eq!(
            read(&d, 9999, 2).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let unknown = Aligned::new(disk(0, 1).inner, 0, 0);
        assert_eq!(unknown.block_size(), 512);
    }

    #[test]
    fn files_read_at_offsets() {
        let path = std::env::temp_dir().join(format!("ferric-blockd-dev-{}", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        let device = FileDevice::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(device.size(), 10);
        let mut buf = [0; 3];
        device.read_at(7, &mut buf).unwrap();
        assert_eq!(&buf, b"789");
        let err = device.read_at(8, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
