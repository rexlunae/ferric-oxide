//! The UMEM: the application's packet memory, split into frames.
//!
//! The application registers a region of private memory with `kernel.regbuf` and grants it
//! `rw` to the driver; that grant is the only memory the device can reach for the socket. The
//! region is split into frames of one [`FrameSize`], at most [`MAX_FRAMES`] of them, and a
//! frame is named by its byte offset in the region. Every offset that comes from the other
//! side goes through [`Umem::frame_at`] before it is used.

use thiserror::Error;

/// The most frames a UMEM may have.
pub const MAX_FRAMES: u64 = 1 << 20;

/// The size of every frame of a UMEM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameSize {
    /// 2 KiB: holds a standard 1518-byte Ethernet frame.
    Bytes2048,
    /// 4 KiB: one page per frame.
    Bytes4096,
}

impl FrameSize {
    /// The size in bytes.
    pub const fn bytes(self) -> u32 {
        match self {
            Self::Bytes2048 => 2048,
            Self::Bytes4096 => 4096,
        }
    }

    /// The frame size of `bytes` bytes, if it is one.
    pub const fn from_bytes(bytes: u64) -> Option<Self> {
        match bytes {
            2048 => Some(Self::Bytes2048),
            4096 => Some(Self::Bytes4096),
            _ => None,
        }
    }
}

/// The geometry of a UMEM: its length and frame size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Umem {
    bytes: u64,
    frame_size: FrameSize,
}

impl Umem {
    /// A UMEM of `bytes` bytes in frames of `frame_size`.
    ///
    /// # Errors
    ///
    /// [`UmemError::Empty`] for 0 bytes, [`UmemError::Unaligned`] when `bytes` is not a whole
    /// number of frames, and [`UmemError::TooManyFrames`] above [`MAX_FRAMES`].
    pub const fn new(bytes: u64, frame_size: FrameSize) -> Result<Self, UmemError> {
        let size = frame_size.bytes() as u64;
        if bytes == 0 {
            return Err(UmemError::Empty);
        }
        if !bytes.is_multiple_of(size) {
            return Err(UmemError::Unaligned { bytes });
        }
        if bytes / size > MAX_FRAMES {
            return Err(UmemError::TooManyFrames {
                frames: bytes / size,
            });
        }
        Ok(Self { bytes, frame_size })
    }

    /// The region's length in bytes.
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The size of each frame.
    pub const fn frame_size(&self) -> FrameSize {
        self.frame_size
    }

    /// The number of frames.
    pub const fn frames(&self) -> u32 {
        // `new` bounded this by MAX_FRAMES, which fits in a u32.
        (self.bytes / self.frame_size.bytes() as u64) as u32
    }

    /// Frame number `index`, if there is one.
    pub const fn frame(&self, index: u32) -> Option<Frame> {
        if index >= self.frames() {
            return None;
        }
        Some(Frame(index as u64 * self.frame_size.bytes() as u64))
    }

    /// Every frame, in order: what an application puts on the Fill ring at first.
    pub fn iter(&self) -> impl Iterator<Item = Frame> + use<> {
        let size = u64::from(self.frame_size.bytes());
        (0..u64::from(self.frames())).map(move |index| Frame(index * size))
    }

    /// The frame at byte offset `offset`.
    ///
    /// # Errors
    ///
    /// [`FrameError::Unaligned`] when `offset` is not the start of a frame, and
    /// [`FrameError::OutOfRange`] when it is past the end of the region.
    pub const fn frame_at(&self, offset: u64) -> Result<Frame, FrameError> {
        if !offset.is_multiple_of(self.frame_size.bytes() as u64) {
            return Err(FrameError::Unaligned { offset });
        }
        if offset >= self.bytes {
            return Err(FrameError::OutOfRange { offset });
        }
        Ok(Frame(offset))
    }

    /// The index of `frame` in this UMEM.
    pub const fn index_of(&self, frame: Frame) -> u64 {
        frame.0 / self.frame_size.bytes() as u64
    }
}

/// One frame of a UMEM, by byte offset. Only a [`Umem`] makes one, so it is always aligned and
/// inside the UMEM it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frame(u64);

impl Frame {
    /// The byte offset in the region.
    pub const fn offset(self) -> u64 {
        self.0
    }
}

/// Why a UMEM geometry was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum UmemError {
    /// The region is empty.
    #[error("the UMEM is empty")]
    Empty,
    /// The region is not a whole number of frames.
    #[error("a UMEM of {bytes} bytes is not a whole number of frames")]
    Unaligned {
        /// The region's length.
        bytes: u64,
    },
    /// The region has more than [`MAX_FRAMES`] frames.
    #[error("a UMEM of {frames} frames is larger than the limit of {MAX_FRAMES}")]
    TooManyFrames {
        /// The number of frames it would have.
        frames: u64,
    },
}

/// Why an offset is not a frame of the UMEM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FrameError {
    /// The offset is not at the start of a frame.
    #[error("offset {offset:#x} is not the start of a UMEM frame")]
    Unaligned {
        /// The offset.
        offset: u64,
    },
    /// The offset is past the end of the UMEM.
    #[error("offset {offset:#x} is past the end of the UMEM")]
    OutOfRange {
        /// The offset.
        offset: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_is_checked() {
        assert_eq!(Umem::new(0, FrameSize::Bytes2048), Err(UmemError::Empty));
        assert_eq!(
            Umem::new(3000, FrameSize::Bytes2048),
            Err(UmemError::Unaligned { bytes: 3000 })
        );
        assert_eq!(
            Umem::new((MAX_FRAMES + 1) * 4096, FrameSize::Bytes4096),
            Err(UmemError::TooManyFrames {
                frames: MAX_FRAMES + 1
            })
        );
        let largest = Umem::new(MAX_FRAMES * 4096, FrameSize::Bytes4096).expect("the limit");
        assert_eq!(u64::from(largest.frames()), MAX_FRAMES);
        assert_eq!(FrameSize::from_bytes(4096), Some(FrameSize::Bytes4096));
        assert_eq!(FrameSize::from_bytes(1024), None);
    }

    #[test]
    fn offsets_are_checked() {
        let umem = Umem::new(8 * 2048, FrameSize::Bytes2048).expect("a valid UMEM");
        assert_eq!(umem.frame_at(2048).map(Frame::offset), Ok(2048));
        assert_eq!(
            umem.frame_at(2049),
            Err(FrameError::Unaligned { offset: 2049 })
        );
        assert_eq!(
            umem.frame_at(8 * 2048),
            Err(FrameError::OutOfRange { offset: 8 * 2048 })
        );
        assert_eq!(
            umem.frame_at(u64::MAX),
            Err(FrameError::Unaligned { offset: u64::MAX })
        );
        assert_eq!(umem.frame(7), umem.frame_at(7 * 2048).ok());
        assert_eq!(umem.frame(8), None);
        let all: [u64; 8] = core::array::from_fn(|i| 2048 * i as u64);
        assert!(umem.iter().map(Frame::offset).eq(all));
        assert!(
            umem.iter()
                .enumerate()
                .all(|(i, f)| umem.index_of(f) == i as u64)
        );
    }
}
