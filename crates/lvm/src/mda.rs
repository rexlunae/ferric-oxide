//! Metadata areas: the 512-byte `mda_header` and the circular buffer of metadata text after it
//! (libvslvm "Logical Volume Manager (LVM) format", section 3; pvck(8)).
//!
//! The header holds a checksum of its bytes 4..512, the signature ` LVM2 x[5A%r0N*>`, version 1,
//! the area's own offset and size, and a zero-terminated list of raw locations. The first raw
//! location not flagged "ignored" points at the current metadata text: an offset from the start
//! of the area, a size, and a checksum of the text. The text area is a circular buffer that
//! starts after the header, so text that runs past the end of the area continues right after
//! the header.

use alloc::vec;
use alloc::vec::Vec;

use thiserror::Error;

use crate::crc;
use crate::label::{SECTOR, le32, le64};

/// The metadata area header signature.
pub const MDA_MAGIC: [u8; 16] = *b" LVM2 x[5A%r0N*>";
/// The size of the metadata area header.
pub const MDA_HEADER_LEN: u64 = SECTOR as u64;
/// The raw-location flag for a location to ignore.
pub const RAW_LOCN_IGNORED: u32 = 1;
/// The largest metadata text this crate reads (16 MiB, far above what `pvcreate` allocates).
pub const MAX_TEXT_LEN: u64 = 16 << 20;
/// How many raw locations fit in the header (bytes 40..136).
const RAW_LOCATIONS: usize = 4;

/// A parsed metadata area header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdaHeader {
    /// The area's offset from the start of the device, in bytes.
    pub start: u64,
    /// The area's size, including this header, in bytes.
    pub size: u64,
    /// The raw locations, in order.
    pub locations: Vec<RawLocation>,
}

/// A raw location: where one copy of the metadata text is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RawLocation {
    /// The offset from the start of the metadata area, in bytes.
    pub offset: u64,
    /// The text size, in bytes.
    pub size: u64,
    /// The checksum of the text.
    pub checksum: u32,
    /// Whether the location is to be ignored.
    pub ignored: bool,
}

/// One or two byte ranges of the device holding a text, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextRanges {
    /// The first part: (device offset, length).
    pub first: (u64, u64),
    /// The part that wrapped round to just after the header, if any: (device offset, length).
    pub wrapped: Option<(u64, u64)>,
}

impl MdaHeader {
    /// Parses the header of the metadata area that the PV label puts at `expected_start`.
    ///
    /// # Errors
    ///
    /// An [`MdaError`] naming the first field that is not valid.
    pub fn parse(sector: &[u8; SECTOR], expected_start: u64) -> Result<Self, MdaError> {
        if le32(sector, 0) != crc::checksum(&sector[4..]) {
            return Err(MdaError::Checksum);
        }
        if sector[4..20] != MDA_MAGIC {
            return Err(MdaError::Magic);
        }
        let version = le32(sector, 20);
        if version != 1 {
            return Err(MdaError::Version(version));
        }
        let start = le64(sector, 24);
        if start != expected_start {
            return Err(MdaError::Misplaced {
                expected: expected_start,
                found: start,
            });
        }
        let size = le64(sector, 32);
        if size <= MDA_HEADER_LEN {
            return Err(MdaError::Size(size));
        }
        let mut locations = Vec::new();
        for i in 0..RAW_LOCATIONS {
            let at = 40 + i * 24;
            let location = RawLocation {
                offset: le64(sector, at),
                size: le64(sector, at + 8),
                checksum: le32(sector, at + 16),
                ignored: le32(sector, at + 20) & RAW_LOCN_IGNORED != 0,
            };
            if location.offset == 0 && location.size == 0 {
                break;
            }
            locations.push(location);
        }
        Ok(Self {
            start,
            size,
            locations,
        })
    }

    /// The current metadata text's location: the first that is not ignored and not empty, or
    /// `None` when the area holds no metadata (a PV in no volume group).
    pub fn current(&self) -> Option<&RawLocation> {
        self.locations.iter().find(|l| !l.ignored && l.size != 0)
    }

    /// Where on the device `location`'s text is, following the wrap at the end of the area.
    ///
    /// # Errors
    ///
    /// [`MdaError::Location`] when the location is not inside the text buffer, or the text is
    /// larger than the buffer or [`MAX_TEXT_LEN`].
    pub fn text_ranges(&self, location: &RawLocation) -> Result<TextRanges, MdaError> {
        let buffer = self.size - MDA_HEADER_LEN;
        if location.offset < MDA_HEADER_LEN
            || location.offset >= self.size
            || location.size > buffer
            || location.size > MAX_TEXT_LEN
        {
            return Err(MdaError::Location);
        }
        let first_len = location.size.min(self.size - location.offset);
        let first = (self.start + location.offset, first_len);
        let rest = location.size - first_len;
        let wrapped = (rest > 0).then_some((self.start + MDA_HEADER_LEN, rest));
        Ok(TextRanges { first, wrapped })
    }

    /// Reads and checks `location`'s text.
    ///
    /// # Errors
    ///
    /// [`TextReadError::Mda`] for a location outside the buffer or a checksum that does not
    /// match, and [`TextReadError::Io`] when a read fails.
    pub fn read_text<F, E>(
        &self,
        read_at: &mut F,
        location: &RawLocation,
    ) -> Result<Vec<u8>, TextReadError<E>>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), E>,
    {
        let ranges = self.text_ranges(location)?;
        let mut text = vec![0u8; location.size as usize];
        let (first_at, first_len) = ranges.first;
        let (head, tail) = text.split_at_mut(first_len as usize);
        read_at(first_at, head).map_err(|source| TextReadError::Io {
            offset: first_at,
            source,
        })?;
        if let Some((at, _)) = ranges.wrapped {
            read_at(at, tail).map_err(|source| TextReadError::Io { offset: at, source })?;
        }
        if crc::checksum(&text) != location.checksum {
            return Err(TextReadError::Mda(MdaError::TextChecksum));
        }
        Ok(text)
    }
}

/// A metadata area header or location that is not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum MdaError {
    /// The header checksum does not match.
    #[error("the LVM metadata area header checksum does not match")]
    Checksum,
    /// The signature is wrong.
    #[error("the LVM metadata area header signature is wrong")]
    Magic,
    /// The version is not 1.
    #[error("the LVM metadata area header version {0} is not 1")]
    Version(u32),
    /// The header records a different offset from where the label put it.
    #[error("the LVM metadata area at {expected:#x} says it is at {found:#x}")]
    Misplaced {
        /// Where the label put it.
        expected: u64,
        /// The header's own offset.
        found: u64,
    },
    /// The area is too small to hold any text.
    #[error("the LVM metadata area size {0} is too small")]
    Size(u64),
    /// A raw location is not inside the text buffer.
    #[error("an LVM metadata location is outside its area")]
    Location,
    /// The text checksum does not match.
    #[error("the LVM metadata text checksum does not match")]
    TextChecksum,
}

/// Metadata text that could not be read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TextReadError<E> {
    /// The location or text is not valid.
    #[error(transparent)]
    Mda(#[from] MdaError),
    /// A read failed.
    #[error("reading LVM metadata text at {offset:#x} failed")]
    Io {
        /// Where the read started.
        offset: u64,
        /// The read error.
        #[source]
        source: E,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(locations: &[RawLocation]) -> [u8; SECTOR] {
        let mut s = [0u8; SECTOR];
        s[4..20].copy_from_slice(&MDA_MAGIC);
        s[20..24].copy_from_slice(&1u32.to_le_bytes());
        s[24..32].copy_from_slice(&4096u64.to_le_bytes());
        s[32..40].copy_from_slice(&8192u64.to_le_bytes());
        for (i, l) in locations.iter().enumerate() {
            let at = 40 + i * 24;
            s[at..at + 8].copy_from_slice(&l.offset.to_le_bytes());
            s[at + 8..at + 16].copy_from_slice(&l.size.to_le_bytes());
            s[at + 16..at + 20].copy_from_slice(&l.checksum.to_le_bytes());
            s[at + 20..at + 24].copy_from_slice(&u32::from(l.ignored).to_le_bytes());
        }
        let sum = crc::checksum(&s[4..]);
        s[..4].copy_from_slice(&sum.to_le_bytes());
        s
    }

    fn location(offset: u64, size: u64, checksum: u32) -> RawLocation {
        RawLocation {
            offset,
            size,
            checksum,
            ignored: false,
        }
    }

    #[test]
    fn locations_and_wrapping() {
        let ignored = RawLocation {
            ignored: true,
            ..location(512, 10, 0)
        };
        let h = MdaHeader::parse(&header(&[ignored, location(8000, 300, 0)]), 4096).unwrap();
        assert_eq!((h.start, h.size, h.locations.len()), (4096, 8192, 2));
        let current = *h.current().expect("one is current");
        assert_eq!(current.offset, 8000);
        // 192 bytes fit before the end of the area; 108 continue after the header.
        assert_eq!(
            h.text_ranges(&current),
            Ok(TextRanges {
                first: (4096 + 8000, 192),
                wrapped: Some((4096 + 512, 108)),
            })
        );
        assert_eq!(
            h.text_ranges(&location(512, 100, 0)),
            Ok(TextRanges {
                first: (4096 + 512, 100),
                wrapped: None
            })
        );
        for bad in [
            location(100, 10, 0),
            location(8192, 10, 0),
            location(512, 8000, 0),
        ] {
            assert_eq!(h.text_ranges(&bad), Err(MdaError::Location));
        }
        let empty = MdaHeader::parse(&header(&[]), 4096).unwrap();
        assert_eq!(empty.current(), None);
    }

    #[test]
    fn wrapped_text_is_read_and_checked() {
        let text = b"0123456789abcdef";
        let sum = crc::checksum(text);
        let h = MdaHeader::parse(&header(&[location(8192 - 6, 16, sum)]), 4096).unwrap();
        let mut device = vec![0u8; 4096 + 8192];
        device[4096 + 8192 - 6..].copy_from_slice(&text[..6]);
        device[4096 + 512..4096 + 512 + 10].copy_from_slice(&text[6..]);
        let mut read = |at: u64, buf: &mut [u8]| -> Result<(), ()> {
            buf.copy_from_slice(&device[at as usize..at as usize + buf.len()]);
            Ok(())
        };
        assert_eq!(h.read_text(&mut read, h.current().unwrap()).unwrap(), text);
        let wrong = location(8192 - 6, 16, sum ^ 1);
        assert!(matches!(
            h.read_text(&mut read, &wrong),
            Err(TextReadError::Mda(MdaError::TextChecksum))
        ));
    }

    #[test]
    fn invalid_headers_are_named() {
        let good = header(&[]);
        let mut s = good;
        s[100] ^= 1;
        assert_eq!(MdaHeader::parse(&s, 4096), Err(MdaError::Checksum));
        assert_eq!(
            MdaHeader::parse(&good, 8192),
            Err(MdaError::Misplaced {
                expected: 8192,
                found: 4096
            })
        );
        let reseal = |mut s: [u8; SECTOR]| {
            let sum = crc::checksum(&s[4..]);
            s[..4].copy_from_slice(&sum.to_le_bytes());
            s
        };
        let mut s = good;
        s[5] = b'X';
        assert_eq!(MdaHeader::parse(&reseal(s), 4096), Err(MdaError::Magic));
        let mut s = good;
        s[20] = 2;
        assert_eq!(
            MdaHeader::parse(&reseal(s), 4096),
            Err(MdaError::Version(2))
        );
        let mut s = good;
        s[32..40].copy_from_slice(&512u64.to_le_bytes());
        assert_eq!(MdaHeader::parse(&reseal(s), 4096), Err(MdaError::Size(512)));
    }
}
