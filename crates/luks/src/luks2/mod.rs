//! LUKS2 headers: the two copies of binary header and JSON area, and choosing between them
//! (LUKS2 On-Disk Format Specification 1.1.4, sections 2 and 4.5).
//!
//! [`read`](crate::read) loads the primary copy at offset 0 and the secondary copy after it. A
//! copy is valid when its binary header parses, its checksum matches, it sits where it says it
//! does, and its JSON metadata parses. When both are valid the one with the higher `seqid` wins
//! (the primary on a tie); when one is damaged the other is used and the damage is reported
//! through [`Luks2::other_copy_error`], including a secondary copy that cannot be read. When the primary binary header is unreadable, the
//! secondary is searched for at each offset table 1 allows.

pub mod binary;
pub mod metadata;

use alloc::vec;

use thiserror::Error;

use crate::luks2::binary::{
    BINARY_HEADER_LEN, BinaryHeader, BinaryHeaderError, HeaderCopy, SECONDARY_OFFSETS,
};
use crate::luks2::metadata::{Metadata, MetadataError};

/// A LUKS2 header: the copy in use, with the other copy's state.
#[derive(Debug)]
pub struct Luks2 {
    /// The binary header of the copy in use.
    pub binary: BinaryHeader,
    /// The JSON metadata of the copy in use.
    pub metadata: Metadata,
    other: Option<CopyError>,
}

impl Luks2 {
    /// Why the copy not in use was rejected, or `None` when both copies are valid.
    pub fn other_copy_error(&self) -> Option<&CopyError> {
        self.other.as_ref()
    }

    /// The offset of the first data segment (the lowest-numbered one), in bytes.
    pub fn payload_offset(&self) -> Option<u64> {
        self.metadata.segments.values().next().map(|s| s.offset)
    }
}

/// Why one header copy is not usable.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CopyError {
    /// Its binary header is not valid, or its checksum does not match.
    #[error("the LUKS2 binary header is not valid")]
    Binary(#[source] BinaryHeaderError),
    /// Its JSON metadata is not valid.
    #[error("the LUKS2 JSON metadata is not valid")]
    Metadata(#[source] MetadataError),
    /// It is the other copy (a secondary magic at offset 0, or the reverse).
    #[error("found the {found:?} LUKS2 header where the other copy belongs")]
    WrongCopy {
        /// The copy its magic names.
        found: HeaderCopy,
    },
    /// Its `hdr_offset` is not where it was read.
    #[error("the LUKS2 header copy at {expected:#x} says it is at {found:#x}")]
    Misplaced {
        /// Where it was read.
        expected: u64,
        /// Its `hdr_offset`.
        found: u64,
    },
    /// The secondary copy's size does not match its offset (the primary's `hdr_size`).
    #[error("the secondary LUKS2 header at {offset:#x} has hdr_size {hdr_size:#x}")]
    SizeMismatch {
        /// The secondary copy's offset.
        offset: u64,
        /// Its `hdr_size`.
        hdr_size: u64,
    },
    /// No secondary copy was found at any allowed offset.
    #[error("no secondary LUKS2 header was found")]
    NotFound,
    /// Reading the copy failed (for the secondary copy of a valid primary), or the search for
    /// the secondary copy stopped at a read that failed before finding one.
    #[error("reading the LUKS2 header copy at {offset:#x} failed")]
    Unreadable {
        /// Where the failed read started.
        offset: u64,
    },
}

pub(crate) type Copy = (BinaryHeader, Metadata);

/// Loads the copy at `offset`. The outer result is I/O, with the failing offset; the inner one
/// says whether the copy is valid.
pub(crate) fn load_copy<F, E>(
    read_at: &mut F,
    offset: u64,
    want: HeaderCopy,
) -> Result<Result<Copy, CopyError>, (u64, E)>
where
    F: FnMut(u64, &mut [u8]) -> Result<(), E>,
{
    let mut sector = [0u8; BINARY_HEADER_LEN];
    read_at(offset, &mut sector).map_err(|e| (offset, e))?;
    let binary = match BinaryHeader::parse(&sector) {
        Ok(binary) => binary,
        Err(error) => return Ok(Err(CopyError::Binary(error))),
    };
    Ok(match check_place(&binary, offset, want) {
        Ok(()) => {
            let mut area = vec![0u8; binary.hdr_size as usize];
            area[..BINARY_HEADER_LEN].copy_from_slice(&sector);
            let rest = offset + BINARY_HEADER_LEN as u64;
            read_at(rest, &mut area[BINARY_HEADER_LEN..]).map_err(|e| (rest, e))?;
            finish(binary, &area)
        }
        Err(error) => Err(error),
    })
}

fn check_place(binary: &BinaryHeader, offset: u64, want: HeaderCopy) -> Result<(), CopyError> {
    if binary.copy != want {
        return Err(CopyError::WrongCopy { found: binary.copy });
    }
    if binary.hdr_offset != offset {
        return Err(CopyError::Misplaced {
            expected: offset,
            found: binary.hdr_offset,
        });
    }
    // The secondary copy starts right after the primary, so its offset is its own size too.
    if want == HeaderCopy::Secondary && binary.hdr_size != offset {
        return Err(CopyError::SizeMismatch {
            offset,
            hdr_size: binary.hdr_size,
        });
    }
    Ok(())
}

fn finish(binary: BinaryHeader, area: &[u8]) -> Result<Copy, CopyError> {
    binary.verify_checksum(area).map_err(CopyError::Binary)?;
    let metadata = Metadata::parse(&area[BINARY_HEADER_LEN..], binary.hdr_size)
        .map_err(CopyError::Metadata)?;
    Ok((binary, metadata))
}

/// Loads both copies and picks one. `primary` is the result of loading offset 0.
pub(crate) fn choose<F, E>(
    read_at: &mut F,
    primary: Result<Copy, CopyError>,
) -> Result<Result<Luks2, (CopyError, CopyError)>, (u64, E)>
where
    F: FnMut(u64, &mut [u8]) -> Result<(), E>,
{
    let secondary = match &primary {
        // A valid primary is enough: a secondary that cannot be read is reported, not fatal.
        Ok((binary, _)) => load_copy(read_at, binary.hdr_size, HeaderCopy::Secondary)
            .unwrap_or_else(|(offset, _)| Err(CopyError::Unreadable { offset })),
        // A rejected primary's hdr_size cannot be trusted, so search every allowed offset.
        Err(_) => scan_secondary(read_at)?,
    };
    Ok(match (primary, secondary) {
        (Ok(p), Ok(s)) if s.0.seqid > p.0.seqid => Ok(used(s, None)),
        (Ok(p), Ok(_)) => Ok(used(p, None)),
        (Ok(p), Err(e)) | (Err(e), Ok(p)) => Ok(used(p, Some(e))),
        (Err(p), Err(s)) => Err((p, s)),
    })
}

fn used((binary, metadata): Copy, other: Option<CopyError>) -> Luks2 {
    Luks2 {
        binary,
        metadata,
        other,
    }
}

fn scan_secondary<F, E>(read_at: &mut F) -> Result<Result<Copy, CopyError>, (u64, E)>
where
    F: FnMut(u64, &mut [u8]) -> Result<(), E>,
{
    let mut last = CopyError::NotFound;
    for offset in SECONDARY_OFFSETS {
        let mut sector = [0u8; BINARY_HEADER_LEN];
        // Offsets past the end of a small device fail to read; that ends the search. When
        // nothing was found, the failed read is what is reported.
        if read_at(offset, &mut sector).is_err() {
            if matches!(last, CopyError::NotFound) {
                last = CopyError::Unreadable { offset };
            }
            break;
        }
        if sector[..6] != binary::MAGIC_SECONDARY {
            continue;
        }
        match load_copy(read_at, offset, HeaderCopy::Secondary)? {
            Ok(copy) => return Ok(Ok(copy)),
            Err(error) => last = error,
        }
    }
    Ok(Err(last))
}
