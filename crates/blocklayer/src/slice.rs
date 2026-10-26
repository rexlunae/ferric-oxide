//! A byte window of a device.

use thiserror::Error;

use crate::{BlockDevice, OutOfRange};

/// The bytes `offset..offset + len` of a device, as a device of its own: a partition, or the
/// part of an image a volume is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slice<D> {
    inner: D,
    offset: u64,
    len: u64,
}

impl<D: BlockDevice> Slice<D> {
    /// The `len` bytes of `inner` from `offset`.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] when the window does not fit inside `inner`, with the window's length
    /// saturated to `usize`.
    pub fn new(inner: D, offset: u64, len: u64) -> Result<Self, OutOfRange> {
        let size = inner.size();
        match offset.checked_add(len) {
            Some(end) if end <= size => Ok(Self { inner, offset, len }),
            _ => Err(OutOfRange {
                offset,
                len: usize::try_from(len).unwrap_or(usize::MAX),
                size,
            }),
        }
    }

    /// The bytes of `inner` from `offset` to its end.
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] when `offset` is past the end of `inner`.
    pub fn from_offset(inner: D, offset: u64) -> Result<Self, OutOfRange> {
        let len = inner.size().checked_sub(offset).ok_or(OutOfRange {
            offset,
            len: 0,
            size: inner.size(),
        })?;
        Self::new(inner, offset, len)
    }

    /// Where the window starts in the device below.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The device below.
    pub fn inner(&self) -> &D {
        &self.inner
    }
}

impl<D: BlockDevice> BlockDevice for Slice<D> {
    type Error = SliceError<D::Error>;

    fn size(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        OutOfRange::check(offset, buf.len(), self.len)?;
        self.inner
            .read_at(self.offset + offset, buf)
            .map_err(SliceError::Inner)
    }
}

/// A read from a [`Slice`] that failed.
#[derive(Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SliceError<E> {
    /// The read is not inside the window.
    #[error(transparent)]
    OutOfRange(#[from] OutOfRange),
    /// The device below failed.
    #[error("reading the device below the window failed")]
    Inner(#[source] E),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Memory;

    #[test]
    fn windows_shift_and_bound_reads() {
        let bytes: [u8; 32] = core::array::from_fn(|i| i as u8);
        let slice = Slice::new(Memory::new(bytes), 8, 16).unwrap();
        assert_eq!((slice.size(), slice.offset()), (16, 8));
        assert_eq!(slice.inner().size(), 32);
        let mut buf = [0; 4];
        slice.read_at(12, &mut buf).unwrap();
        assert_eq!(buf, [20, 21, 22, 23]);
        assert!(matches!(
            slice.read_at(13, &mut buf),
            Err(SliceError::OutOfRange(_))
        ));
        let tail = Slice::from_offset(Memory::new(bytes), 30).unwrap();
        assert_eq!(tail.size(), 2);
    }

    #[test]
    fn windows_must_fit() {
        let device = Memory::new([0u8; 10]);
        assert!(Slice::new(&device, 4, 6).is_ok());
        assert_eq!(
            Slice::new(&device, 4, 7),
            Err(OutOfRange {
                offset: 4,
                len: 7,
                size: 10
            })
        );
        assert!(Slice::new(&device, u64::MAX, 2).is_err());
        assert!(Slice::from_offset(&device, 11).is_err());
    }
}
