//! Read-only LUKS1 and LUKS2 headers: parsing, validation, and recovering the volume key from a
//! passphrase.
//!
//! This is the header half of Ferric Oxide's dm-crypt compatibility (roadmap Phase 3): the
//! bootloader and the block-layer daemon use it to find and check a LUKS header and to unlock
//! the volume key. Decrypting data segments is left to the block layer.
//!
//! # Specifications
//!
//! Written from the published format documents only, without reference to any implementation's
//! source:
//!
//! - *LUKS1 On-Disk Format Specification*, version 1.2.3, Clemens Fruhwirth, 2018;
//! - *LUKS2 On-Disk Format Specification*, version 1.1.4, Milan Brož, 2025;
//! - RFC 8018 (PBKDF2), RFC 9106 (Argon2), IEEE 1619 (XTS) and FIPS 180-4 (SHA), through the
//!   RustCrypto implementations.
//!
//! # Structure
//!
//! - [`read`] loads a header through a caller-supplied read function and returns a
//!   [`Header`]: [`luks1::Luks1`] or [`luks2::Luks2`].
//! - [`luks2`] checks both header copies (binary header, checksum, JSON metadata) and picks the
//!   valid, more recent one.
//! - `unlock` on either header derives keys with PBKDF2 or Argon2, decrypts key material
//!   ([`cipher`]), merges it ([`af`]) and checks the digest, within [`Limits`] that bound the
//!   work a hostile header can ask for.
//!
//! The crate does no I/O of its own: every read goes through a
//! `FnMut(offset, &mut buf) -> Result<(), E>` that fills the whole buffer, so it runs over a
//! block scheme, a UEFI block protocol or a byte slice alike.
//!
//! # Examples
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let image = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/luks2-pbkdf2.img"))?;
//! let mut read_at = |offset: u64, buf: &mut [u8]| -> Result<(), std::io::Error> {
//!     let start = usize::try_from(offset).map_err(std::io::Error::other)?;
//!     let bytes = image.get(start..start + buf.len()).ok_or(std::io::ErrorKind::UnexpectedEof)?;
//!     buf.copy_from_slice(bytes);
//!     Ok(())
//! };
//! let header = ferric_luks::read(&mut read_at)?;
//! assert_eq!(header.version(), 2);
//! let unlocked = header.unlock(&mut read_at, b"correct horse", &ferric_luks::Limits::default())?;
//! assert_eq!(unlocked.key.as_bytes().len(), 32);
//! # Ok(())
//! # }
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod af;
mod bytes;
pub mod cipher;
pub mod hash;
pub mod luks1;
pub mod luks2;
mod unlock;

use thiserror::Error;

pub use unlock::{DEFAULT_MAX_ARGON2_MEMORY_KIB, Limits, UnlockError, Unlocked, VolumeKey};

use crate::luks1::{Luks1, Luks1Error};
use crate::luks2::binary::{BINARY_HEADER_LEN, HeaderCopy};
use crate::luks2::{CopyError, Luks2};

/// A LUKS header.
#[derive(Debug)]
#[non_exhaustive]
pub enum Header {
    /// A LUKS1 header.
    V1(Luks1),
    /// A LUKS2 header.
    V2(Luks2),
}

impl Header {
    /// The format version: 1 or 2.
    pub fn version(&self) -> u16 {
        match self {
            Self::V1(_) => 1,
            Self::V2(_) => 2,
        }
    }

    /// The device UUID, as text.
    pub fn uuid(&self) -> &str {
        match self {
            Self::V1(h) => &h.uuid,
            Self::V2(h) => &h.binary.uuid,
        }
    }

    /// The offset of the encrypted data, in bytes (for LUKS2, of the first segment).
    pub fn payload_offset(&self) -> Option<u64> {
        match self {
            Self::V1(h) => Some(h.payload_offset_bytes()),
            Self::V2(h) => h.payload_offset(),
        }
    }

    /// Recovers the volume key with `passphrase`; see [`Luks1::unlock`] and [`Luks2::unlock`].
    ///
    /// # Errors
    ///
    /// The [`UnlockError`] of the header's `unlock`.
    pub fn unlock<F, E>(
        &self,
        read_at: &mut F,
        passphrase: &[u8],
        limits: &Limits,
    ) -> Result<Unlocked, UnlockError<E>>
    where
        F: FnMut(u64, &mut [u8]) -> Result<(), E>,
    {
        match self {
            Self::V1(h) => h.unlock(read_at, passphrase, limits),
            Self::V2(h) => h.unlock(read_at, passphrase, limits),
        }
    }
}

/// Reads the LUKS header at the start of a device.
///
/// `read_at(offset, buf)` must fill all of `buf` from byte `offset` of the device, or fail.
///
/// # Errors
///
/// - [`ReadError::NotLuks`] when the device does not start with a LUKS magic (and no LUKS2
///   secondary header is found);
/// - [`ReadError::Luks1`] for a LUKS1 header that is not valid;
/// - [`ReadError::NoValidCopy`] when neither LUKS2 copy is valid, with both reasons;
/// - [`ReadError::Io`] when a read fails, except while searching for a secondary header past
///   the end of the device.
pub fn read<F, E>(read_at: &mut F) -> Result<Header, ReadError<E>>
where
    F: FnMut(u64, &mut [u8]) -> Result<(), E>,
{
    let mut sector = [0u8; BINARY_HEADER_LEN];
    read_at(0, &mut sector).map_err(|source| ReadError::Io { offset: 0, source })?;
    if sector[..6] == luks1::MAGIC && bytes::be16(&sector, 6) == 1 {
        let header = sector[..luks1::HEADER_LEN]
            .try_into()
            .expect("592 of 4096 bytes");
        return Luks1::parse(header)
            .map(Header::V1)
            .map_err(ReadError::Luks1);
    }
    let io = |(offset, source)| ReadError::Io { offset, source };
    let primary = luks2::load_copy(read_at, 0, HeaderCopy::Primary).map_err(io)?;
    match luks2::choose(read_at, primary).map_err(io)? {
        Ok(header) => Ok(Header::V2(header)),
        // Nothing that looks like LUKS at either place: not a LUKS device at all.
        Err((
            CopyError::Binary(luks2::binary::BinaryHeaderError::Magic),
            CopyError::NotFound | CopyError::Unreadable { .. },
        )) => Err(ReadError::NotLuks),
        Err((primary, secondary)) => Err(ReadError::NoValidCopy { primary, secondary }),
    }
}

/// A LUKS header that could not be read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReadError<E> {
    /// A read failed.
    #[error("reading the LUKS header at {offset:#x} failed")]
    Io {
        /// Where the read started.
        offset: u64,
        /// The read error.
        #[source]
        source: E,
    },
    /// The device has no LUKS header.
    #[error("the device has no LUKS header")]
    NotLuks,
    /// The LUKS1 header is not valid.
    #[error("the LUKS1 header is not valid")]
    Luks1(#[source] Luks1Error),
    /// Neither LUKS2 header copy is valid.
    #[error("neither LUKS2 header copy is valid (primary: {primary}; secondary: {secondary})")]
    NoValidCopy {
        /// Why the primary copy was rejected.
        primary: CopyError,
        /// Why the secondary copy was rejected.
        #[source]
        secondary: CopyError,
    },
}
