//! The calls a disk scheme takes on a disk or partition descriptor to move blocks straight
//! between the device and an application's registered buffer.
//!
//! The application attaches a grant to one of the descriptor's *slots* once, then names the
//! slot in each request; requests carry no buffer of their own, and the scheme copies nothing.
//!
//! | Call | Encoding |
//! | --- | --- |
//! | attach | `SYS_CALL` with `CallFlags::FD` on the disk descriptor: the payload is the grant's descriptor number (it moves to the scheme), the one metadata word the [`Slot`] |
//! | [`Request::Read`] | verb 1, slot, offset in the grant, first block, length in bytes |
//! | [`Request::Write`] | verb 2, slot, offset in the grant, first block, length in bytes |
//! | [`Request::Detach`] | verb 3, slot |
//!
//! Blocks count from the start of the descriptor's disk or partition, in the disk's block
//! size (`fstat`'s `st_blksize`). `Read` has the device write the blocks into the grant, so it
//! needs a read-write grant; `Write` has the device read them from it.
//!
//! Attaching maps the grant into the driver's DMA domain (`MapGrant`, `ferric-iommu`'s `abi`),
//! so the device reaches the buffer at one contiguous IOVA range and nothing else of the
//! application. Attaching to a slot that holds a grant replaces it. Detaching, and closing the
//! disk descriptor, unmap the slot's grant from the domain and close the scheme's copy of the
//! grant. When the application revokes the grant (or closes it, or its region), the kernel
//! takes the device mapping down before `Revoke` returns; requests on the slot then fail with
//! `ECANCELED` and the device does not touch the buffer again.
//!
//! # Errors
//!
//! Attaching: `EOPNOTSUPP` when the driver is built without grant DMA, `ENODEV` when there is no
//! IOMMU or the driver has no DMA domain (there is no physical-address fallback), `EINVAL` for
//! a slot of [`MAX_SLOTS`] or more, a descriptor that is not a grant or a grant that is not
//! page-aligned, `EBADF` for a descriptor that is not open, and `ECANCELED` for a revoked grant.
//!
//! Requests: `ENOENT` for an empty slot, `EINVAL` for a length that is zero or not a multiple of
//! the block size, an offset that is not a multiple of [`PAGE_SIZE`] or a range outside the
//! grant, `EACCES` for a `Read` into a read-only grant, `ECANCELED` once the grant is revoked,
//! `EOVERFLOW` past the end of the disk or partition, and `EIO` when the device fails.

use core::fmt;

/// The page size grant offsets are aligned to.
pub const PAGE_SIZE: u64 = 4096;

/// How many slots a disk descriptor has.
pub const MAX_SLOTS: u16 = 64;

/// One of a disk descriptor's grant slots, below [`MAX_SLOTS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(u16);

impl Slot {
    /// The slot numbered `raw`, or `None` when it is [`MAX_SLOTS`] or more.
    pub const fn new(raw: u64) -> Option<Self> {
        if raw < MAX_SLOTS as u64 {
            Some(Self(raw as u16))
        } else {
            None
        }
    }

    /// The slot's number.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "slot {}", self.0)
    }
}

/// A block transfer between a slot's grant and the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transfer {
    /// The slot whose grant holds the buffer.
    pub slot: Slot,
    /// Where in the grant the buffer starts, in bytes.
    pub offset: u64,
    /// The first block, from the start of the disk or partition.
    pub lba: u64,
    /// The length in bytes.
    pub len: u64,
}

/// A call's verb, the first metadata word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
#[non_exhaustive]
pub enum Verb {
    /// Blocks from the disk into the grant.
    Read = 1,
    /// Blocks from the grant onto the disk.
    Write = 2,
    /// Empty a slot.
    Detach = 3,
}

impl Verb {
    const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            1 => Some(Self::Read),
            2 => Some(Self::Write),
            3 => Some(Self::Detach),
            _ => None,
        }
    }
}

/// A grant call on a disk descriptor (attaching travels as fd passing instead).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// Read blocks into the slot's grant.
    Read(Transfer),
    /// Write blocks from the slot's grant.
    Write(Transfer),
    /// Unmap and drop the slot's grant.
    Detach {
        /// The slot.
        slot: Slot,
    },
}

impl Request {
    /// The most metadata words a request takes.
    pub const WORDS: usize = 5;

    /// The call's metadata words, and how many of them are used.
    pub const fn encode(self) -> ([u64; Self::WORDS], usize) {
        match self {
            Self::Read(t) => (Self::transfer(Verb::Read, t), 5),
            Self::Write(t) => (Self::transfer(Verb::Write, t), 5),
            Self::Detach { slot } => ([Verb::Detach as u64, slot.0 as u64, 0, 0, 0], 2),
        }
    }

    const fn transfer(verb: Verb, t: Transfer) -> [u64; Self::WORDS] {
        [verb as u64, t.slot.0 as u64, t.offset, t.lba, t.len]
    }

    /// The request in call metadata words.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, [`DecodeError::Length`] when the
    /// number of words does not match the verb, and [`DecodeError::Slot`] for a slot of
    /// [`MAX_SLOTS`] or more.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        let (&verb, args) = words.split_first().ok_or(DecodeError::Verb)?;
        let slot = |raw| Slot::new(raw).ok_or(DecodeError::Slot);
        match (Verb::from_raw(verb).ok_or(DecodeError::Verb)?, args) {
            (verb @ (Verb::Read | Verb::Write), &[s, offset, lba, len]) => {
                let t = Transfer {
                    slot: slot(s)?,
                    offset,
                    lba,
                    len,
                };
                Ok(if verb == Verb::Read {
                    Self::Read(t)
                } else {
                    Self::Write(t)
                })
            }
            (Verb::Detach, &[s]) => Ok(Self::Detach { slot: slot(s)? }),
            _ => Err(DecodeError::Length),
        }
    }
}

/// Why call metadata is not a valid [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The verb is missing or unknown.
    Verb,
    /// The number of words does not match the verb.
    Length,
    /// The slot is [`MAX_SLOTS`] or more.
    Slot,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Verb => "missing or unknown disk grant verb",
            Self::Length => "wrong number of metadata words for the disk grant verb",
            Self::Slot => "grant slot out of range",
        })
    }
}

impl core::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(raw: u64) -> Slot {
        Slot::new(raw).expect("in range")
    }

    #[test]
    fn requests_round_trip() {
        let t = Transfer {
            slot: slot(3),
            offset: 0x2000,
            lba: 77,
            len: 0x10000,
        };
        for request in [
            Request::Read(t),
            Request::Write(t),
            Request::Detach { slot: slot(63) },
        ] {
            let (words, n) = request.encode();
            assert_eq!(Request::decode(&words[..n]), Ok(request));
        }
        assert_eq!(Request::Read(t).encode().0, [1, 3, 0x2000, 77, 0x10000]);
        assert_eq!(
            Request::Detach { slot: slot(5) }.encode(),
            ([3, 5, 0, 0, 0], 2)
        );
    }

    #[test]
    fn bad_metadata_is_refused() {
        assert_eq!(Request::decode(&[]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[9, 0]), Err(DecodeError::Verb));
        assert_eq!(Request::decode(&[1, 0, 0, 0]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[3]), Err(DecodeError::Length));
        assert_eq!(Request::decode(&[3, 64]), Err(DecodeError::Slot));
        assert_eq!(Request::decode(&[2, 64, 0, 0, 512]), Err(DecodeError::Slot));
    }

    #[test]
    fn slots_stop_at_the_limit() {
        assert_eq!(Slot::new(63).map(Slot::get), Some(63));
        assert_eq!(Slot::new(u64::from(MAX_SLOTS)), None);
        assert_eq!(Slot::new(u64::MAX), None);
    }
}
