//! A device over a byte buffer.

use crate::{BlockDevice, OutOfRange};

/// A block device whose bytes are a buffer in memory: a test image, or a small volume read in
/// whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory<T>(T);

impl<T: AsRef<[u8]>> Memory<T> {
    /// A device holding `bytes`.
    pub fn new(bytes: T) -> Self {
        Self(bytes)
    }

    /// The buffer back.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: AsRef<[u8]>> BlockDevice for Memory<T> {
    type Error = OutOfRange;

    fn size(&self) -> u64 {
        // A slice is never longer than isize::MAX bytes, which fits in u64.
        self.0.as_ref().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), OutOfRange> {
        let bytes = self.0.as_ref();
        OutOfRange::check(offset, buf.len(), self.size())?;
        // In range, so the offset is below the slice length and fits in usize.
        let start = offset as usize;
        buf.copy_from_slice(&bytes[start..start + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_inside_the_buffer_only() {
        let device = Memory::new([1u8, 2, 3, 4]);
        assert_eq!(device.size(), 4);
        let mut buf = [0; 2];
        device.read_at(2, &mut buf).unwrap();
        assert_eq!(buf, [3, 4]);
        assert_eq!(
            device.read_at(3, &mut buf),
            Err(OutOfRange {
                offset: 3,
                len: 2,
                size: 4
            })
        );
        assert_eq!(device.into_inner(), [1, 2, 3, 4]);
    }
}
