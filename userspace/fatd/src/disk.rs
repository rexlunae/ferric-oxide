//! A block device as the read-only, seekable stream the `fatfs` reader takes.
//!
//! `fatfs` reads through `std::io::{Read, Seek}` and asks for `Write` as well. [`Disk`] gives
//! it a cursor over any [`BlockDevice`] (a whole disk, a [`ferric_blocklayer::Slice`] of one
//! for a partition, a decrypted LUKS volume) and refuses every write with
//! [`io::ErrorKind::ReadOnlyFilesystem`], so nothing `fatfs` does can change the volume.
//!
//! `fatfs` reads the allocation table one entry (2 to 4 bytes) and directories one 32-byte
//! entry at a time. On a Redox disk scheme each of those would be a whole-block read of its
//! own, so [`Disk`] keeps the last [`CHUNK`] bytes it read and serves small reads from them.
//! Reads of a chunk or more (file data, read a cluster at a time) go straight to the device.

use std::io::{self, Read, Seek, SeekFrom, Write};

use ferric_blocklayer::BlockDevice;

/// The size and alignment of the one chunk [`Disk`] keeps for small reads: a common
/// cluster size, and a whole number of the 512- and 4096-byte blocks of real disks.
pub const CHUNK: usize = 4096;

/// A block device that `fatfs` can read through, whose errors it can carry.
pub trait Device: BlockDevice<Error: std::error::Error + Send + Sync + 'static> {}

impl<D> Device for D
where
    D: BlockDevice,
    D::Error: std::error::Error + Send + Sync + 'static,
{
}

/// A read-only cursor over a block device (see the module docs).
#[derive(Debug)]
pub struct Disk<D> {
    device: D,
    position: u64,
    /// Where the kept chunk starts, and its bytes (shorter than [`CHUNK`] at the end of the
    /// device); `None` until the first small read.
    chunk: Option<(u64, Vec<u8>)>,
}

impl<D: Device> Disk<D> {
    /// A cursor at the start of `device`.
    pub fn new(device: D) -> Self {
        Self {
            device,
            position: 0,
            chunk: None,
        }
    }

    /// The device's size in bytes.
    pub fn size(&self) -> u64 {
        self.device.size()
    }

    /// The device underneath.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// Serves a read of less than a chunk from the kept chunk, loading the chunk that holds
    /// the position first if it is another one.
    fn read_small(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let start = self.position - self.position % CHUNK as u64;
        let loaded = matches!(&self.chunk, Some((at, _)) if *at == start);
        if !loaded {
            let len = usize::try_from((self.size() - start).min(CHUNK as u64))
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let mut bytes = self.chunk.take().map(|(_, b)| b).unwrap_or_default();
            bytes.resize(len, 0);
            self.device
                .read_at(start, &mut bytes)
                .map_err(io::Error::other)?;
            self.chunk = Some((start, bytes));
        }
        let bytes = self.chunk.as_ref().map_or(&[][..], |(_, b)| b.as_slice());
        // The position is inside the chunk: it starts at `start` and runs to the device's
        // end or a whole chunk, and the position is before the end.
        let within = (self.position - start) as usize;
        let n = (bytes.len() - within).min(buf.len());
        buf[..n].copy_from_slice(&bytes[within..within + n]);
        Ok(n)
    }
}

impl<D: Device> Read for Disk<D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.size().saturating_sub(self.position);
        let want = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
        if want == 0 {
            return Ok(0);
        }
        let n = if want < CHUNK {
            self.read_small(&mut buf[..want])?
        } else {
            self.device
                .read_at(self.position, &mut buf[..want])
                .map_err(io::Error::other)?;
            want
        };
        self.position += n as u64;
        Ok(n)
    }
}

impl<D: Device> Seek for Disk<D> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(delta) => self.size().checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        self.position = target.ok_or(io::ErrorKind::InvalidInput)?;
        Ok(self.position)
    }
}

impl<D> Write for Disk<D> {
    /// Refuses the write: `fatd` serves FAT volumes read-only.
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::ReadOnlyFilesystem.into())
    }

    /// Nothing is ever written, so there is nothing to flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use ferric_blocklayer::{Memory, OutOfRange};

    use super::*;

    /// A device that counts its reads.
    struct Counting {
        memory: Memory<Vec<u8>>,
        reads: Cell<usize>,
    }

    impl BlockDevice for Counting {
        type Error = OutOfRange;

        fn size(&self) -> u64 {
            self.memory.size()
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), OutOfRange> {
            self.reads.set(self.reads.get() + 1);
            self.memory.read_at(offset, buf)
        }
    }

    fn disk(len: usize) -> Disk<Counting> {
        let bytes = (0..len).map(|i| (i % 251) as u8).collect();
        Disk::new(Counting {
            memory: Memory::new(bytes),
            reads: Cell::new(0),
        })
    }

    fn want(offset: usize, len: usize) -> Vec<u8> {
        (offset..offset + len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn small_reads_share_one_device_read() {
        let mut d = disk(3 * CHUNK);
        let mut buf = [0; 32];
        d.seek(SeekFrom::Start(100)).unwrap();
        for i in 0..10 {
            d.read_exact(&mut buf).unwrap();
            assert_eq!(buf.to_vec(), want(100 + i * 32, 32));
        }
        assert_eq!(d.device().reads.get(), 1);
        // A read across the chunk's end takes the rest of it, then the next chunk.
        d.seek(SeekFrom::Start(CHUNK as u64 - 8)).unwrap();
        d.read_exact(&mut buf).unwrap();
        assert_eq!(buf.to_vec(), want(CHUNK - 8, 32));
        assert_eq!(d.device().reads.get(), 2);
    }

    #[test]
    fn large_reads_go_straight_to_the_device() {
        let mut d = disk(3 * CHUNK);
        let mut buf = vec![0; 2 * CHUNK];
        d.seek(SeekFrom::Start(10)).unwrap();
        assert_eq!(d.read(&mut buf).unwrap(), 2 * CHUNK);
        assert_eq!(buf, want(10, 2 * CHUNK));
        assert_eq!(d.device().reads.get(), 1);
        assert!(d.chunk.is_none());
    }

    #[test]
    fn reads_stop_at_the_end() {
        let mut d = disk(CHUNK + 10);
        let mut buf = [0; 64];
        d.seek(SeekFrom::End(-4)).unwrap();
        assert_eq!(d.read(&mut buf).unwrap(), 4);
        assert_eq!(buf[..4].to_vec(), want(CHUNK + 6, 4));
        assert_eq!(d.read(&mut buf).unwrap(), 0);
        d.seek(SeekFrom::Start(1 << 40)).unwrap();
        assert_eq!(d.read(&mut buf).unwrap(), 0);
        assert!(d.seek(SeekFrom::Current(-(1 << 41))).is_err());
    }

    #[test]
    fn writes_are_refused() {
        let mut d = disk(CHUNK);
        let err = d.write(b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ReadOnlyFilesystem);
        d.flush().unwrap();
    }

    /// A device whose reads all fail.
    struct Broken;

    impl BlockDevice for Broken {
        type Error = OutOfRange;

        fn size(&self) -> u64 {
            1 << 20
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), OutOfRange> {
            Err(OutOfRange {
                offset,
                len: buf.len(),
                size: 0,
            })
        }
    }

    #[test]
    fn device_errors_are_kept() {
        let mut d = Disk::new(Broken);
        let err = d.read(&mut [0; 8]).unwrap_err();
        let inner = err.get_ref().and_then(|e| e.downcast_ref::<OutOfRange>());
        // The small read asked the device for the whole chunk around it.
        assert_eq!(inner.map(|e| e.len), Some(CHUNK));
    }
}
