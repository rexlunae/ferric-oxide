//! Where the file system's bytes come from.
//!
//! The ext4 reader asks for byte ranges ([`ext4_view::Ext4Read`]). A [`Window`] answers them
//! from a byte range of a [`ReadAt`] source, so the same daemon reads a whole disk, one
//! partition of it (`--offset`, `--length`) or an image file, and later a LUKS or LVM volume.
//! Replaying the journal (features.toml `fs.ext4_journal_replay`) also writes, through
//! [`WriteAt`].

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use ext4_view::Ext4Read;
use thiserror::Error;

/// A source of bytes that can be read at any offset.
///
/// Implement it for a block layer (a decrypted LUKS volume, an LVM logical volume) to put
/// ext4 on top of it.
pub trait ReadAt {
    /// Fills `buf` from the bytes starting at `offset`.
    ///
    /// # Errors
    ///
    /// Any I/O error, including [`io::ErrorKind::UnexpectedEof`] when the source ends first.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// The source's size in bytes.
    ///
    /// # Errors
    ///
    /// Any I/O error from asking the source.
    fn size(&self) -> io::Result<u64>;
}

impl ReadAt for File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
}

impl ReadAt for Vec<u8> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let src = start
            .checked_add(buf.len())
            .and_then(|end| self.get(start..end))
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        u64::try_from(self.len()).map_err(|_| io::ErrorKind::InvalidData.into())
    }
}

/// A [`ReadAt`] source that can also be written, for replaying the journal.
pub trait WriteAt: ReadAt {
    /// Writes all of `buf` at `offset`.
    ///
    /// # Errors
    ///
    /// Any I/O error.
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Waits until everything written so far is on stable storage.
    ///
    /// # Errors
    ///
    /// Any I/O error from the device.
    fn flush(&mut self) -> io::Result<()>;
}

impl WriteAt for File {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        FileExt::write_all_at(self, buf, offset)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sync_data()
    }
}

impl WriteAt for Vec<u8> {
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let dst = start
            .checked_add(buf.len())
            .and_then(|end| self.get_mut(start..end))
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        dst.copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Why a [`Window`] could not be made or could not serve a read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WindowError {
    /// Asking the source for its length failed.
    #[error("cannot get the length of the device")]
    Length(#[source] io::Error),
    /// The window does not fit inside the source.
    #[error(
        "bytes {offset}..{offset}+{length} are past the end of the device ({source_len} bytes)"
    )]
    OutOfBounds {
        /// The window's first byte.
        offset: u64,
        /// The window's length.
        length: u64,
        /// The source's length.
        source_len: u64,
    },
    /// A read reached past the end of the window.
    #[error("a read of {len} bytes at {start} is past the end of the file system ({length} bytes)")]
    PastEnd {
        /// The read's first byte, from the window's start.
        start: u64,
        /// The read's length.
        len: usize,
        /// The window's length.
        length: u64,
    },
    /// The source failed a read.
    #[error("reading {len} bytes at {start} failed")]
    Read {
        /// The read's first byte, from the window's start.
        start: u64,
        /// The read's length.
        len: usize,
        /// The source's error.
        #[source]
        source: io::Error,
    },
    /// The source failed a write.
    #[error("writing {len} bytes at {start} failed")]
    Write {
        /// The write's first byte, from the window's start.
        start: u64,
        /// The write's length.
        len: usize,
        /// The source's error.
        #[source]
        source: io::Error,
    },
    /// The source could not flush its writes.
    #[error("flushing the device failed")]
    Flush(#[source] io::Error),
}

/// Bytes `offset..offset + length` of a [`ReadAt`] source, as the ext4 reader sees them.
#[derive(Debug)]
pub struct Window<R> {
    source: R,
    offset: u64,
    length: u64,
}

impl<R: ReadAt> Window<R> {
    /// The whole of `source`.
    ///
    /// # Errors
    ///
    /// [`WindowError::Length`] when the source cannot say how long it is.
    pub fn whole(source: R) -> Result<Self, WindowError> {
        let length = source.size().map_err(WindowError::Length)?;
        Ok(Self {
            source,
            offset: 0,
            length,
        })
    }

    /// Bytes `offset..offset + length` of `source`, or from `offset` to its end when `length`
    /// is `None`: a partition of a disk.
    ///
    /// # Errors
    ///
    /// [`WindowError::Length`] when the source cannot say how long it is, and
    /// [`WindowError::OutOfBounds`] when the range does not fit in it.
    pub fn range(source: R, offset: u64, length: Option<u64>) -> Result<Self, WindowError> {
        let source_len = source.size().map_err(WindowError::Length)?;
        let length = length.unwrap_or_else(|| source_len.saturating_sub(offset));
        let fits = offset
            .checked_add(length)
            .is_some_and(|end| end <= source_len);
        if !fits {
            return Err(WindowError::OutOfBounds {
                offset,
                length,
                source_len,
            });
        }
        Ok(Self {
            source,
            offset,
            length,
        })
    }

    /// The window's length in bytes.
    pub const fn len(&self) -> u64 {
        self.length
    }

    /// Whether the window holds no bytes.
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Fills `buf` from byte `start` of the window.
    ///
    /// # Errors
    ///
    /// [`WindowError::PastEnd`] when the read does not fit in the window, and
    /// [`WindowError::Read`] when the source fails it.
    pub fn read_exact_at(&self, buf: &mut [u8], start: u64) -> Result<(), WindowError> {
        let at = self.place(start, buf.len())?;
        let len = buf.len();
        self.source
            .read_exact_at(buf, at)
            .map_err(|source| WindowError::Read { start, len, source })
    }

    /// Where in the source `len` bytes from byte `start` of the window are.
    fn place(&self, start: u64, len: usize) -> Result<u64, WindowError> {
        let past_end = || WindowError::PastEnd {
            start,
            len,
            length: self.length,
        };
        let wide = u64::try_from(len).map_err(|_| past_end())?;
        if start.checked_add(wide).is_none_or(|end| end > self.length) {
            return Err(past_end());
        }
        // `start + len` fits in the window, which fits in the source.
        Ok(self.offset + start)
    }
}

impl<R: WriteAt> Window<R> {
    /// Writes `buf` at byte `start` of the window.
    ///
    /// # Errors
    ///
    /// [`WindowError::PastEnd`] when the write does not fit in the window, and
    /// [`WindowError::Write`] when the source fails it.
    pub fn write_all_at(&mut self, buf: &[u8], start: u64) -> Result<(), WindowError> {
        let at = self.place(start, buf.len())?;
        let len = buf.len();
        self.source
            .write_all_at(buf, at)
            .map_err(|source| WindowError::Write { start, len, source })
    }

    /// Waits until every write is on stable storage.
    ///
    /// # Errors
    ///
    /// [`WindowError::Flush`] when the source fails to.
    pub fn flush(&mut self) -> Result<(), WindowError> {
        self.source.flush().map_err(WindowError::Flush)
    }

    /// Gives back the source.
    pub fn into_inner(self) -> R {
        self.source
    }
}

impl<R: ReadAt> Ext4Read for Window<R> {
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> Result<(), Box<dyn core::error::Error + Send + Sync + 'static>> {
        Ok(self.read_exact_at(dst, start_byte)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes() -> Vec<u8> {
        (0..=255).collect()
    }

    #[test]
    fn a_range_reads_from_its_offset() {
        let window = Window::range(bytes(), 16, Some(32)).expect("fits");
        let mut buf = [0; 4];
        window.read_exact_at(&mut buf, 2).expect("inside");
        assert_eq!(buf, [18, 19, 20, 21]);
        assert_eq!(window.len(), 32);
    }

    #[test]
    fn a_range_without_a_length_runs_to_the_end() {
        let window = Window::range(bytes(), 200, None).expect("fits");
        assert_eq!(window.len(), 56);
    }

    #[test]
    fn a_range_past_the_source_is_refused() {
        let err = Window::range(bytes(), 250, Some(10)).expect_err("too long");
        assert!(matches!(
            err,
            WindowError::OutOfBounds {
                offset: 250,
                length: 10,
                source_len: 256
            }
        ));
        assert!(Window::range(bytes(), u64::MAX, Some(2)).is_err());
    }

    #[test]
    fn reads_stay_inside_the_window() {
        let window = Window::range(bytes(), 16, Some(32)).expect("fits");
        let mut buf = [0; 4];
        let err = window
            .read_exact_at(&mut buf, 30)
            .expect_err("past the end");
        assert!(matches!(
            err,
            WindowError::PastEnd {
                start: 30,
                len: 4,
                length: 32
            }
        ));
        assert!(window.read_exact_at(&mut buf, u64::MAX).is_err());
        // The last bytes of the window are still readable.
        window.read_exact_at(&mut buf, 28).expect("inside");
        assert_eq!(buf, [44, 45, 46, 47]);
    }

    #[test]
    fn writes_stay_inside_the_window() {
        let mut window = Window::range(bytes(), 16, Some(32)).expect("fits");
        window.write_all_at(&[9, 9], 30).expect("inside");
        assert!(matches!(
            window.write_all_at(&[9, 9], 31),
            Err(WindowError::PastEnd { start: 31, .. })
        ));
        window.flush().expect("a vector flushes");
        let bytes = window.into_inner();
        assert_eq!(&bytes[45..49], &[45, 9, 9, 48]);
    }

    #[test]
    fn the_ext4_reader_sees_the_window_errors() {
        let mut window = Window::whole(bytes()).expect("fits");
        let mut buf = [0; 8];
        let err = Ext4Read::read(&mut window, 252, &mut buf).expect_err("past the end");
        assert!(err.downcast_ref::<WindowError>().is_some());
    }
}
