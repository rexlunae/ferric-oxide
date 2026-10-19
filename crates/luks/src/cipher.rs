//! Decryption of keyslot key material.
//!
//! Key material is encrypted with the keyslot's cipher in 512-byte sectors, the first at sector
//! 0 (LUKS1 appendix B: "cipher modes using IVs and tweaks must start from the all-zero
//! IV/tweak"). This module implements AES in XTS mode (IEEE 1619) with the `plain64` IV, the
//! default of both formats: the tweak is the sector number as a little-endian 128-bit value,
//! which is what `plain64` gives for every sector a keyslot can have. `plain` (the 32-bit form)
//! is the same for sector numbers below 2^32, which covers any key material.

use aes::cipher::consts::U16;
use aes::cipher::{BlockCipher, BlockDecrypt, BlockEncrypt, BlockSizeUser, KeyInit};
use aes::{Aes128, Aes256};
use thiserror::Error;
use xts_mode::{Xts128, get_tweak_default};

/// The sector size key material is encrypted in.
pub const KEY_MATERIAL_SECTOR: usize = 512;

/// A cipher a keyslot names, resolved to one this crate implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cipher {
    /// AES-XTS with the `plain64` (or `plain`) IV.
    AesXtsPlain64,
}

impl Cipher {
    /// The cipher a LUKS2 dm-crypt specification such as `aes-xts-plain64` names.
    ///
    /// # Errors
    ///
    /// [`CipherError::Unsupported`] for any other cipher.
    pub fn from_dm_crypt(spec: &str) -> Result<Self, CipherError> {
        match spec.split_once('-') {
            Some((name, mode)) => Self::from_luks1(name, mode),
            None => Err(CipherError::Unsupported),
        }
    }

    /// The cipher a LUKS1 header's `cipher-name` and `cipher-mode` name.
    ///
    /// # Errors
    ///
    /// [`CipherError::Unsupported`] for any other cipher.
    pub fn from_luks1(name: &str, mode: &str) -> Result<Self, CipherError> {
        match (name, mode) {
            ("aes", "xts-plain64" | "xts-plain") => Ok(Self::AesXtsPlain64),
            _ => Err(CipherError::Unsupported),
        }
    }

    /// Decrypts `data`, a whole number of 512-byte sectors starting at sector 0, with `key`.
    ///
    /// # Errors
    ///
    /// [`CipherError::KeySize`] when `key` is not 32 or 64 bytes (AES-128 or AES-256 in XTS),
    /// and [`CipherError::Length`] when `data` is not a whole number of sectors.
    pub fn decrypt_key_material(self, key: &[u8], data: &mut [u8]) -> Result<(), CipherError> {
        if !data.len().is_multiple_of(KEY_MATERIAL_SECTOR) {
            return Err(CipherError::Length);
        }
        match self {
            Self::AesXtsPlain64 => match key.len() {
                32 => xts::<Aes128>(key, data),
                64 => xts::<Aes256>(key, data),
                _ => return Err(CipherError::KeySize(key.len())),
            },
        }
        Ok(())
    }
}

fn xts<C>(key: &[u8], data: &mut [u8])
where
    C: KeyInit + BlockCipher + BlockEncrypt + BlockDecrypt + BlockSizeUser<BlockSize = U16>,
{
    let (data_key, tweak_key) = key.split_at(key.len() / 2);
    let xts = Xts128::new(
        C::new_from_slice(data_key).expect("half of a checked key length"),
        C::new_from_slice(tweak_key).expect("half of a checked key length"),
    );
    xts.decrypt_area(data, KEY_MATERIAL_SECTOR, 0, get_tweak_default);
}

/// Key material that cannot be decrypted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CipherError {
    /// The cipher is not one this crate implements.
    #[error("the keyslot cipher is not supported")]
    Unsupported,
    /// The key size does not fit the cipher.
    #[error("a {0}-byte key does not fit the keyslot cipher")]
    KeySize(usize),
    /// The data is not a whole number of sectors.
    #[error("key material is not a whole number of 512-byte sectors")]
    Length,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IEEE 1619-2007 annex B, vector 4: AES-128-XTS, data unit sequence number 0, 512 bytes.
    #[test]
    fn decrypts_the_ieee_1619_vector() {
        let key: [u8; 32] = [
            0x27, 0x18, 0x28, 0x18, 0x28, 0x45, 0x90, 0x45, 0x23, 0x53, 0x60, 0x28, 0x74, 0x71,
            0x35, 0x26, 0x31, 0x41, 0x59, 0x26, 0x53, 0x58, 0x97, 0x93, 0x23, 0x84, 0x62, 0x64,
            0x33, 0x83, 0x27, 0x95,
        ];
        let plain: alloc::vec::Vec<u8> = (0..512).map(|i| i as u8).collect();
        let mut data = plain.clone();
        // Encrypt with the same construction, then check the published first ciphertext bytes.
        let xts = Xts128::new(
            Aes128::new_from_slice(&key[..16]).unwrap(),
            Aes128::new_from_slice(&key[16..]).unwrap(),
        );
        xts.encrypt_area(&mut data, 512, 0, get_tweak_default);
        assert_eq!(data[..8], [0x27, 0xa7, 0x47, 0x9b, 0xef, 0xa1, 0xd4, 0x76]);
        Cipher::AesXtsPlain64
            .decrypt_key_material(&key, &mut data)
            .expect("decrypts");
        assert_eq!(data, plain);
    }

    #[test]
    fn names_and_sizes_are_checked() {
        assert_eq!(
            Cipher::from_dm_crypt("aes-xts-plain64"),
            Ok(Cipher::AesXtsPlain64)
        );
        assert_eq!(
            Cipher::from_luks1("aes", "xts-plain"),
            Ok(Cipher::AesXtsPlain64)
        );
        assert_eq!(
            Cipher::from_dm_crypt("aes-cbc-essiv:sha256"),
            Err(CipherError::Unsupported)
        );
        assert_eq!(
            Cipher::from_dm_crypt("serpent"),
            Err(CipherError::Unsupported)
        );
        let mut data = [0u8; 512];
        assert_eq!(
            Cipher::AesXtsPlain64.decrypt_key_material(&[0; 48], &mut data),
            Err(CipherError::KeySize(48))
        );
        assert_eq!(
            Cipher::AesXtsPlain64.decrypt_key_material(&[0; 32], &mut data[..100]),
            Err(CipherError::Length)
        );
        assert!(
            Cipher::AesXtsPlain64
                .decrypt_key_material(&[0; 64], &mut data)
                .is_ok()
        );
    }
}
