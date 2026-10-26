//! The LUKS decryption layer.
//!
//! [`Crypt::open`] reads the LUKS header at the start of the device below, recovers the volume
//! key from a passphrase and finds the data segment, all through `ferric-luks`. Reads are then
//! served from the segment: whole encryption sectors are read straight into the caller's
//! buffer and decrypted there, and a partial sector at either end goes through one sector of
//! scratch space. The IV of each sector is worked out by `ferric_luks::data` (it counts
//! 512-byte units for every sector size; that module says how this was established).

use ferric_luks::cipher::CipherError;
use ferric_luks::data::{DataSegment, DecryptError, Decryptor, SegmentError};
use ferric_luks::{Limits, ReadError, UnlockError};
use thiserror::Error;

use crate::{BlockDevice, OutOfRange};

/// The largest LUKS encryption sector, in bytes: `ferric-luks` accepts only 512 to 4096.
const MAX_SECTOR: usize = 4096;

/// A LUKS1 or LUKS2 volume, decrypted.
#[derive(Debug)]
pub struct Crypt<D> {
    inner: D,
    decryptor: Decryptor,
    segment: DataSegment,
    size: u64,
    keyslot: u32,
}

impl<D: BlockDevice> Crypt<D> {
    /// Opens the LUKS volume on `inner` with `passphrase`, trying each keyslot within `limits`.
    ///
    /// # Errors
    ///
    /// A [`CryptOpenError`] when the header cannot be read, no keyslot opens with the
    /// passphrase, the data segment is not one this layer reads, or the segment does not fit
    /// on the device.
    pub fn open(
        inner: D,
        passphrase: &[u8],
        limits: &Limits,
    ) -> Result<Self, CryptOpenError<D::Error>> {
        let mut read_at = |offset, buf: &mut [u8]| inner.read_at(offset, buf);
        let header = ferric_luks::read(&mut read_at).map_err(CryptOpenError::Header)?;
        let unlocked = header
            .unlock(&mut read_at, passphrase, limits)
            .map_err(CryptOpenError::Unlock)?;
        let segment = header.data_segment()?;
        let device = inner.size();
        let size = match segment.size {
            Some(size) => size,
            None => device.saturating_sub(segment.offset),
        };
        if segment
            .offset
            .checked_add(size)
            .is_none_or(|end| end > device)
        {
            return Err(CryptOpenError::SegmentPastEnd {
                offset: segment.offset,
                size,
                device,
            });
        }
        // A volume whose length is not whole sectors has a partial last sector no one wrote.
        let size = size - size % u64::from(segment.sector_size);
        let decryptor = segment
            .decryptor(&unlocked.key)
            .map_err(CryptOpenError::Key)?;
        Ok(Self {
            inner,
            decryptor,
            segment,
            size,
            keyslot: unlocked.keyslot,
        })
    }

    /// The keyslot that opened the volume.
    pub fn keyslot(&self) -> u32 {
        self.keyslot
    }

    /// The data segment being decrypted.
    pub fn segment(&self) -> &DataSegment {
        &self.segment
    }

    /// The device below.
    pub fn inner(&self) -> &D {
        &self.inner
    }

    /// Reads and decrypts whole sectors from `offset` (sector-aligned) of the segment.
    fn read_sectors(&self, offset: u64, buf: &mut [u8]) -> Result<(), CryptError<D::Error>> {
        self.inner
            .read_at(self.segment.offset + offset, buf)
            .map_err(CryptError::Inner)?;
        self.decryptor
            .decrypt(buf, offset)
            .map_err(CryptError::Decrypt)
    }
}

impl<D: BlockDevice> BlockDevice for Crypt<D> {
    type Error = CryptError<D::Error>;

    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, mut offset: u64, mut buf: &mut [u8]) -> Result<(), Self::Error> {
        OutOfRange::check(offset, buf.len(), self.size)?;
        let sector = self.decryptor.sector_size();
        let sector_len = sector as usize;
        while !buf.is_empty() {
            // `sector` divides the size, so a sector that starts inside the volume ends inside.
            let within = (offset % u64::from(sector)) as usize;
            let whole = if within == 0 {
                buf.len() - buf.len() % sector_len
            } else {
                0
            };
            let done = if whole > 0 {
                self.read_sectors(offset, &mut buf[..whole])?;
                whole
            } else {
                let mut scratch = [0u8; MAX_SECTOR];
                let scratch = &mut scratch[..sector_len];
                self.read_sectors(offset - within as u64, scratch)?;
                let n = (sector_len - within).min(buf.len());
                buf[..n].copy_from_slice(&scratch[within..within + n]);
                n
            };
            buf = &mut buf[done..];
            offset += done as u64;
        }
        Ok(())
    }
}

/// A LUKS volume that could not be opened.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CryptOpenError<E> {
    /// The LUKS header could not be read or is not valid.
    #[error("the LUKS header cannot be read")]
    Header(#[source] ReadError<E>),
    /// No keyslot opened with the passphrase, or reading one failed.
    #[error("the LUKS volume key cannot be recovered")]
    Unlock(#[source] UnlockError<E>),
    /// The data segment is not one this layer can read.
    #[error("the LUKS data segment cannot be read")]
    Segment(#[from] SegmentError),
    /// The volume key does not fit the data segment's cipher.
    #[error("the LUKS volume key does not fit the data cipher")]
    Key(#[source] CipherError),
    /// The data segment runs past the end of the device.
    #[error(
        "the LUKS data segment ({size} bytes at {offset:#x}) runs past the {device}-byte device"
    )]
    SegmentPastEnd {
        /// Where the segment starts.
        offset: u64,
        /// Its size.
        size: u64,
        /// The size of the device.
        device: u64,
    },
}

/// A read from a [`Crypt`] volume that failed.
#[derive(Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CryptError<E> {
    /// The read is not inside the volume.
    #[error(transparent)]
    OutOfRange(#[from] OutOfRange),
    /// The device below failed.
    #[error("reading the encrypted device failed")]
    Inner(#[source] E),
    /// The sectors could not be decrypted (never for reads this layer makes).
    #[error("decrypting the volume failed")]
    Decrypt(#[source] DecryptError),
}
