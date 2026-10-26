//! Data segments: where a volume's encrypted data is, and decrypting it.
//!
//! [`crate::Header::data_segment`] gives the one [`DataSegment`] a volume has, and
//! [`DataSegment::decryptor`] pairs it with the volume key. A [`Decryptor`] decrypts whole
//! encryption sectors at any sector-aligned offset into the segment.
//!
//! # IVs
//!
//! Every sector is encrypted with the `plain64` IV, or `plain`, its 32-bit form, which wraps
//! to 0 at sector 2^32 (see [`crate::cipher`]). LUKS1 data has 512-byte sectors, the first
//! at IV 0 (LUKS1 specification, appendix B). A LUKS2 crypt segment names its `sector_size`
//! (512, 1024, 2048 or 4096 bytes) and an `iv_tweak`, "the starting offset for the
//! Initialization Vector" (LUKS2 specification 1.1.4, section 3.3), but the specification does
//! not say in which units the IV counts when sectors are larger than 512 bytes. It was settled
//! by experiment, with no implementation's source: cryptsetup 2.7 encrypted a volume with
//! 4096-byte sectors (`tests/fixtures/make-fixtures.sh` in `ferric-blocklayer`), and its data
//! decrypts only when the IV of the 4096-byte sector at byte `b` of the segment is
//! `iv_tweak + b / 512`. Counting in 4096-byte units (`iv_tweak + b / 4096`) gives noise past
//! the first sector. So the IV always counts 512-byte units, whatever the sector size: see
//! [`IV_UNIT`].

use thiserror::Error;

use crate::Header;
use crate::cipher::{Cipher, CipherError, DataCipher};
use crate::luks2::metadata::{SegmentKind, SegmentSize};
use crate::unlock::VolumeKey;

/// The size of the unit `plain64` IVs count, in bytes, for every sector size.
pub const IV_UNIT: u64 = 512;

/// A volume's encrypted data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataSegment {
    /// The offset of the data from the start of the device, in bytes.
    pub offset: u64,
    /// The length of the data in bytes, or `None` when it runs to the end of the device.
    pub size: Option<u64>,
    /// The cipher.
    pub cipher: Cipher,
    /// The encryption sector size, in bytes: 512, 1024, 2048 or 4096.
    pub sector_size: u32,
    /// The IV of the first sector.
    pub iv_tweak: u64,
}

impl DataSegment {
    /// Expands `key` for decrypting this segment.
    ///
    /// # Errors
    ///
    /// [`CipherError::KeySize`] when the key does not fit the cipher.
    pub fn decryptor(&self, key: &VolumeKey) -> Result<Decryptor, CipherError> {
        Ok(Decryptor {
            cipher: DataCipher::new(self.cipher, key.as_bytes())?,
            sector_size: self.sector_size,
            iv_tweak: self.iv_tweak,
        })
    }
}

/// Decrypts a data segment's sectors.
#[derive(Debug)]
pub struct Decryptor {
    cipher: DataCipher,
    sector_size: u32,
    iv_tweak: u64,
}

impl Decryptor {
    /// The encryption sector size, in bytes.
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    /// Decrypts `data` in place: whole sectors read from byte `offset` of the segment.
    ///
    /// # Errors
    ///
    /// [`DecryptError::Unaligned`] when `offset` or the length of `data` is not a multiple of
    /// the sector size.
    pub fn decrypt(&self, data: &mut [u8], offset: u64) -> Result<(), DecryptError> {
        let sector = usize::try_from(self.sector_size).expect("sector sizes fit in usize");
        if !offset.is_multiple_of(u64::from(self.sector_size)) || !data.len().is_multiple_of(sector)
        {
            return Err(DecryptError::Unaligned {
                offset,
                len: data.len(),
                sector_size: self.sector_size,
            });
        }
        let step = u64::from(self.sector_size) / IV_UNIT;
        // plain64 is a 64-bit counter, so it wraps rather than failing.
        let mut iv = self.iv_tweak.wrapping_add(offset / IV_UNIT);
        for chunk in data.chunks_exact_mut(sector) {
            self.cipher
                .decrypt_sector(chunk, iv)
                .expect("sectors are at least 512 bytes");
            iv = iv.wrapping_add(step);
        }
        Ok(())
    }
}

/// Data that cannot be decrypted as asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DecryptError {
    /// The read does not cover whole encryption sectors.
    #[error("{len} bytes at segment offset {offset:#x} are not whole {sector_size}-byte sectors")]
    Unaligned {
        /// The segment offset asked for.
        offset: u64,
        /// The length asked for.
        len: usize,
        /// The sector size.
        sector_size: u32,
    },
}

/// A volume whose data segment this crate cannot read.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SegmentError {
    /// The LUKS2 header has no segment, or several (as during reencryption).
    #[error("the LUKS2 header has {0} data segments, not one")]
    Count(usize),
    /// The segment has flags (such as a reencryption backup), so its data is not settled.
    #[error("the data segment is flagged")]
    Flagged,
    /// The segment is integrity-protected, which this crate does not implement.
    #[error("the data segment is integrity-protected")]
    Integrity,
    /// The segment is not a crypt segment.
    #[error("the data segment is not encrypted")]
    NotCrypt,
    /// The segment's cipher is not one this crate implements.
    #[error("the data segment cipher is not supported")]
    Cipher(#[source] CipherError),
}

impl Header {
    /// The volume's encrypted data.
    ///
    /// # Errors
    ///
    /// A [`SegmentError`] when the volume does not have exactly one plain, unflagged crypt
    /// segment with a supported cipher.
    pub fn data_segment(&self) -> Result<DataSegment, SegmentError> {
        match self {
            Self::V1(h) => Ok(DataSegment {
                offset: h.payload_offset_bytes(),
                size: None,
                cipher: Cipher::from_luks1(&h.cipher_name, &h.cipher_mode)
                    .map_err(SegmentError::Cipher)?,
                sector_size: 512,
                iv_tweak: 0,
            }),
            Self::V2(h) => {
                let segments = &h.metadata.segments;
                let (Some(segment), 1) = (segments.values().next(), segments.len()) else {
                    return Err(SegmentError::Count(segments.len()));
                };
                if !segment.flags.is_empty() {
                    return Err(SegmentError::Flagged);
                }
                let SegmentKind::Crypt {
                    iv_tweak,
                    encryption,
                    sector_size,
                    integrity,
                } = &segment.kind
                else {
                    return Err(SegmentError::NotCrypt);
                };
                if integrity.is_some() {
                    return Err(SegmentError::Integrity);
                }
                Ok(DataSegment {
                    offset: segment.offset,
                    size: match segment.size {
                        SegmentSize::Dynamic => None,
                        SegmentSize::Bytes(n) => Some(n),
                    },
                    cipher: Cipher::from_dm_crypt(encryption).map_err(SegmentError::Cipher)?,
                    sector_size: *sector_size,
                    iv_tweak: *iv_tweak,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use aes::Aes128;
    use aes::cipher::KeyInit;
    use alloc::vec::Vec;
    use xts_mode::Xts128;

    use super::*;

    fn decryptor(sector_size: u32, iv_tweak: u64) -> (Decryptor, Xts128<Aes128>) {
        decryptor_for(Cipher::AesXtsPlain64, sector_size, iv_tweak)
    }

    fn decryptor_for(
        cipher: Cipher,
        sector_size: u32,
        iv_tweak: u64,
    ) -> (Decryptor, Xts128<Aes128>) {
        let key: Vec<u8> = (100..132).collect();
        let xts = Xts128::new(
            Aes128::new_from_slice(&key[..16]).unwrap(),
            Aes128::new_from_slice(&key[16..]).unwrap(),
        );
        let cipher = DataCipher::new(cipher, &key).unwrap();
        (
            Decryptor {
                cipher,
                sector_size,
                iv_tweak,
            },
            xts,
        )
    }

    /// Large sectors step the IV by their size in 512-byte units, from `iv_tweak` plus the
    /// offset in 512-byte units.
    #[test]
    fn large_sectors_count_ivs_in_512_byte_units() {
        let (d, xts) = decryptor(4096, 5);
        assert_eq!(d.sector_size(), 4096);
        let plain: Vec<u8> = (0..8192).map(|i| (i % 253) as u8).collect();
        let mut data = plain.clone();
        let (a, b) = data.split_at_mut(4096);
        xts.encrypt_sector(a, u128::from(5u64 + 16).to_le_bytes());
        xts.encrypt_sector(b, u128::from(5u64 + 24).to_le_bytes());
        d.decrypt(&mut data, 8192).unwrap();
        assert_eq!(data, plain);
    }

    #[test]
    fn the_iv_wraps_like_a_64_bit_counter() {
        let (d, xts) = decryptor(512, u64::MAX);
        let plain = [7u8; 1024];
        let mut data = plain;
        let (a, b) = data.split_at_mut(512);
        xts.encrypt_sector(a, u128::from(u64::MAX).to_le_bytes());
        xts.encrypt_sector(b, 0u128.to_le_bytes());
        d.decrypt(&mut data, 0).unwrap();
        assert_eq!(data, plain);
    }

    /// At sector 2^32 (2 TiB in), `plain` wraps to IV 0 and `plain64` does not.
    #[test]
    fn plain_wraps_at_32_bits_and_plain64_does_not() {
        let boundary = 1u64 << 32;
        let offset = boundary * IV_UNIT;
        let plain = [9u8; 1024];
        for (cipher, first, second) in [
            (Cipher::AesXtsPlain, 0u64, 1u64),
            (Cipher::AesXtsPlain64, boundary, boundary + 1),
        ] {
            let (d, xts) = decryptor_for(cipher, 512, 0);
            let mut data = plain;
            let (a, b) = data.split_at_mut(512);
            xts.encrypt_sector(a, u128::from(first).to_le_bytes());
            xts.encrypt_sector(b, u128::from(second).to_le_bytes());
            d.decrypt(&mut data, offset).unwrap();
            assert_eq!(data, plain, "{cipher:?}");
        }
        // The sector just below the boundary is the same for both.
        for cipher in [Cipher::AesXtsPlain, Cipher::AesXtsPlain64] {
            let (d, xts) = decryptor_for(cipher, 512, 0);
            let mut data = [3u8; 512];
            xts.encrypt_sector(&mut data, u128::from(boundary - 1).to_le_bytes());
            d.decrypt(&mut data, offset - 512).unwrap();
            assert_eq!(data, [3u8; 512], "{cipher:?}");
        }
        assert_eq!(
            Cipher::from_dm_crypt("aes-xts-plain"),
            Ok(Cipher::AesXtsPlain)
        );
    }

    #[test]
    fn reads_must_be_whole_sectors() {
        let (d, _) = decryptor(1024, 0);
        let unaligned = |offset, len| {
            Err(DecryptError::Unaligned {
                offset,
                len,
                sector_size: 1024,
            })
        };
        assert_eq!(d.decrypt(&mut [0; 1024], 512), unaligned(512, 1024));
        assert_eq!(d.decrypt(&mut [0; 512], 0), unaligned(0, 512));
        assert_eq!(d.decrypt(&mut [], 2048), Ok(()));
    }
}
