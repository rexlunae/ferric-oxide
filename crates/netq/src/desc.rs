//! The entries of the four rings.
//!
//! | Ring | Producer → consumer | Entry | Type |
//! | --- | --- | --- | --- |
//! | Fill | application → driver | [`FRAME_ENTRY_BYTES`]: frame offset | [`Frame`] |
//! | RX | driver → application | [`PACKET_ENTRY_BYTES`]: offset, length, flags | [`Packet`] |
//! | TX | application → driver | [`PACKET_ENTRY_BYTES`]: offset, length, flags | [`Packet`] |
//! | Completion | driver → application | [`FRAME_ENTRY_BYTES`]: frame offset | [`Frame`] |
//!
//! A frame entry is the frame's byte offset as a little-endian `u64`. A packet descriptor is
//! the offset (`u64`), the packet's length in bytes (`u32`) and flags (`u32`, all reserved and
//! zero in this version), little-endian.
//!
//! The consumer copies a whole entry out of shared memory first and decodes the copy, so the
//! producer cannot change it between the check and the use. Decoding checks the entry against
//! the consumer's own [`Umem`]: a driver drops and counts an entry that fails, so a hostile
//! application can never make the device reach outside its UMEM.

use thiserror::Error;

use crate::umem::{Frame, FrameError, Umem};

/// The size of a Fill or Completion ring entry.
pub const FRAME_ENTRY_BYTES: usize = 8;

/// The size of an RX or TX ring entry.
pub const PACKET_ENTRY_BYTES: usize = 16;

/// A Fill or Completion entry for `frame`.
pub const fn encode_frame(frame: Frame) -> [u8; FRAME_ENTRY_BYTES] {
    frame.offset().to_le_bytes()
}

/// The frame a Fill or Completion entry names.
///
/// # Errors
///
/// [`FrameError`] when the offset is not one of `umem`'s frames.
pub const fn decode_frame(
    entry: [u8; FRAME_ENTRY_BYTES],
    umem: &Umem,
) -> Result<Frame, FrameError> {
    umem.frame_at(u64::from_le_bytes(entry))
}

/// A packet in one frame: an RX or TX entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Packet {
    frame: Frame,
    len: u32,
}

impl Packet {
    /// A packet of `len` bytes at the start of `frame`.
    ///
    /// # Errors
    ///
    /// [`DescError::Frame`] when `frame` is not one of `umem`'s, and [`DescError::Length`] when
    /// `len` is 0 or larger than a frame.
    pub const fn new(umem: &Umem, frame: Frame, len: u32) -> Result<Self, DescError> {
        if let Err(error) = umem.frame_at(frame.offset()) {
            return Err(DescError::Frame(error));
        }
        if len == 0 || len > umem.frame_size().bytes() {
            return Err(DescError::Length { len });
        }
        Ok(Self { frame, len })
    }

    /// The frame that holds the packet.
    pub const fn frame(&self) -> Frame {
        self.frame
    }

    /// The packet's length in bytes.
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Whether the packet is empty; never, since [`Packet::new`] refuses that.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The ring entry.
    pub fn to_bytes(&self) -> [u8; PACKET_ENTRY_BYTES] {
        let mut entry = [0; PACKET_ENTRY_BYTES];
        entry[..8].copy_from_slice(&self.frame.offset().to_le_bytes());
        entry[8..12].copy_from_slice(&self.len.to_le_bytes());
        entry
    }

    /// The packet a ring entry describes.
    ///
    /// # Errors
    ///
    /// [`DescError::Frame`] when the offset is not one of `umem`'s frames,
    /// [`DescError::Length`] when the length is 0 or larger than a frame, and
    /// [`DescError::Flags`] when a reserved flag is set.
    pub fn from_bytes(entry: [u8; PACKET_ENTRY_BYTES], umem: &Umem) -> Result<Self, DescError> {
        let [
            o0,
            o1,
            o2,
            o3,
            o4,
            o5,
            o6,
            o7,
            l0,
            l1,
            l2,
            l3,
            f0,
            f1,
            f2,
            f3,
        ] = entry;
        let flags = u32::from_le_bytes([f0, f1, f2, f3]);
        if flags != 0 {
            return Err(DescError::Flags { flags });
        }
        let frame = umem.frame_at(u64::from_le_bytes([o0, o1, o2, o3, o4, o5, o6, o7]))?;
        Self::new(umem, frame, u32::from_le_bytes([l0, l1, l2, l3]))
    }
}

/// Why a packet descriptor was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DescError {
    /// The offset is not a frame of the UMEM.
    #[error("the packet's frame is not in the UMEM")]
    Frame(#[from] FrameError),
    /// The length is 0 or larger than a frame.
    #[error("a packet of {len} bytes does not fit a UMEM frame")]
    Length {
        /// The length.
        len: u32,
    },
    /// A reserved flag is set.
    #[error("reserved packet flags {flags:#x} are set")]
    Flags {
        /// The flags word.
        flags: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::umem::FrameSize;

    fn umem() -> Umem {
        Umem::new(16 * 2048, FrameSize::Bytes2048).expect("a valid UMEM")
    }

    #[test]
    fn frame_entries_round_trip_and_are_checked() {
        let umem = umem();
        for frame in umem.iter() {
            assert_eq!(decode_frame(encode_frame(frame), &umem), Ok(frame));
        }
        assert_eq!(
            decode_frame(100u64.to_le_bytes(), &umem),
            Err(FrameError::Unaligned { offset: 100 })
        );
        let past = 16 * 2048u64;
        assert_eq!(
            decode_frame(past.to_le_bytes(), &umem),
            Err(FrameError::OutOfRange { offset: past })
        );
    }

    #[test]
    fn packets_round_trip_and_are_checked() {
        let umem = umem();
        let frame = umem.frame(5).expect("frame 5");
        let packet = Packet::new(&umem, frame, 2048).expect("a full frame");
        assert_eq!(Packet::from_bytes(packet.to_bytes(), &umem), Ok(packet));
        assert_eq!(packet.len(), 2048);
        assert!(!packet.is_empty());
        assert_eq!(
            Packet::new(&umem, frame, 0),
            Err(DescError::Length { len: 0 })
        );
        assert_eq!(
            Packet::new(&umem, frame, 2049),
            Err(DescError::Length { len: 2049 })
        );

        let mut flagged = packet.to_bytes();
        flagged[15] = 0x80;
        assert_eq!(
            Packet::from_bytes(flagged, &umem),
            Err(DescError::Flags { flags: 0x8000_0000 })
        );
        let mut outside = packet.to_bytes();
        outside[..8].copy_from_slice(&(64 * 2048u64).to_le_bytes());
        assert!(matches!(
            Packet::from_bytes(outside, &umem),
            Err(DescError::Frame(FrameError::OutOfRange { .. }))
        ));
        // A frame of a larger UMEM is not one of this UMEM's.
        let larger = Umem::new(64 * 2048, FrameSize::Bytes2048).expect("a valid UMEM");
        let foreign = larger.frame(40).expect("frame 40");
        assert!(matches!(
            Packet::new(&umem, foreign, 64),
            Err(DescError::Frame(_))
        ));
    }

    /// Whatever bytes a hostile peer writes, decoding never panics, and an entry it accepts
    /// names a frame inside the UMEM and a length that fits.
    #[test]
    fn hostile_entries_are_contained() {
        let umem = umem();
        let mut seed: u64 = 0x0123_4567_89ab_cdef;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..100_000u64 {
            let mut entry = [0u8; PACKET_ENTRY_BYTES];
            entry[..8].copy_from_slice(&next().to_le_bytes());
            entry[8..].copy_from_slice(&next().to_le_bytes());
            // Bias some entries towards plausible values so the accepting path runs too.
            if round % 2 == 0 {
                entry[..8].copy_from_slice(&((next() % 20) * 2048).to_le_bytes());
                entry[8..12].copy_from_slice(&((next() % 2100) as u32).to_le_bytes());
                entry[12..].fill(0);
            }
            if let Ok(packet) = Packet::from_bytes(entry, &umem) {
                assert!(packet.frame().offset() + u64::from(packet.len()) <= umem.bytes());
                assert_eq!(packet.to_bytes(), entry);
            }
            let frame_entry: [u8; 8] = entry[..8].try_into().expect("8 bytes");
            if let Ok(frame) = decode_frame(frame_entry, &umem) {
                assert!(frame.offset() < umem.bytes());
            }
        }
    }
}
