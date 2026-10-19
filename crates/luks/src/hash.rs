//! The hash algorithms LUKS headers name, and the two ways LUKS uses them: PBKDF2-HMAC and plain
//! digests (for the header checksum and the AF splitter's diffusion function).
//!
//! LUKS1 section 2.2 and appendix B (table 3) list `sha1`, `sha256`, `sha512` and `ripemd160`;
//! LUKS2 section 4.6 makes SHA-1 and SHA-256 mandatory. This crate implements SHA-1, SHA-256 and
//! SHA-512 and keeps any other name, so a header that uses one still parses and only the
//! operation that needs it fails.

use alloc::string::String;

use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

/// The largest digest the supported algorithms produce (SHA-512), in bytes.
pub const MAX_DIGEST_LEN: usize = 64;

/// A hash algorithm named in a LUKS header.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HashAlg {
    /// SHA-1 (RFC 3174), the LUKS1 default.
    Sha1,
    /// SHA-256 (FIPS 180-4), the LUKS2 default.
    Sha256,
    /// SHA-512 (FIPS 180-4).
    Sha512,
    /// An algorithm this crate does not implement, by its header name.
    Other(String),
}

impl HashAlg {
    /// The algorithm a header names.
    pub fn from_name(name: &str) -> Self {
        match name {
            "sha1" => Self::Sha1,
            "sha256" => Self::Sha256,
            "sha512" => Self::Sha512,
            other => Self::Other(other.into()),
        }
    }

    /// The name headers use for the algorithm.
    pub fn name(&self) -> &str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
            Self::Other(name) => name,
        }
    }

    /// The digest length in bytes, or `None` for an algorithm this crate does not implement.
    pub fn output_len(&self) -> Option<usize> {
        match self {
            Self::Sha1 => Some(20),
            Self::Sha256 => Some(32),
            Self::Sha512 => Some(64),
            Self::Other(_) => None,
        }
    }

    /// Hashes the concatenation of `parts` into `out`, which must hold [`Self::output_len`]
    /// bytes. Returns `None` for an algorithm this crate does not implement.
    pub(crate) fn digest(&self, parts: &[&[u8]], out: &mut [u8; MAX_DIGEST_LEN]) -> Option<usize> {
        fn run<D: Digest>(parts: &[&[u8]], out: &mut [u8]) -> usize {
            let mut hasher = D::new();
            for part in parts {
                hasher.update(part);
            }
            let digest = hasher.finalize();
            out[..digest.len()].copy_from_slice(&digest);
            digest.len()
        }
        match self {
            Self::Sha1 => Some(run::<Sha1>(parts, out)),
            Self::Sha256 => Some(run::<Sha256>(parts, out)),
            Self::Sha512 => Some(run::<Sha512>(parts, out)),
            Self::Other(_) => None,
        }
    }

    /// PBKDF2 (RFC 8018) with HMAC over this hash, filling `out`. Returns `None` for an algorithm
    /// this crate does not implement.
    pub(crate) fn pbkdf2(
        &self,
        password: &[u8],
        salt: &[u8],
        iterations: u32,
        out: &mut [u8],
    ) -> Option<()> {
        match self {
            Self::Sha1 => pbkdf2::pbkdf2_hmac::<Sha1>(password, salt, iterations, out),
            Self::Sha256 => pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, out),
            Self::Sha512 => pbkdf2::pbkdf2_hmac::<Sha512>(password, salt, iterations, out),
            Self::Other(_) => return None,
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for alg in [HashAlg::Sha1, HashAlg::Sha256, HashAlg::Sha512] {
            assert_eq!(HashAlg::from_name(alg.name()), alg);
        }
        let other = HashAlg::from_name("ripemd160");
        assert_eq!(other, HashAlg::Other("ripemd160".into()));
        assert_eq!(other.name(), "ripemd160");
        assert_eq!(other.output_len(), None);
    }

    #[test]
    fn digests_match_known_answers() {
        // FIPS 180 "abc" test vectors.
        let mut out = [0; MAX_DIGEST_LEN];
        assert_eq!(HashAlg::Sha256.digest(&[b"a", b"bc"], &mut out), Some(32));
        assert_eq!(out[..4], [0xba, 0x78, 0x16, 0xbf]);
        assert_eq!(HashAlg::Sha1.digest(&[b"abc"], &mut out), Some(20));
        assert_eq!(out[..4], [0xa9, 0x99, 0x3e, 0x36]);
        assert_eq!(HashAlg::Sha512.digest(&[b"abc"], &mut out), Some(64));
        assert_eq!(out[..4], [0xdd, 0xaf, 0x35, 0xa1]);
        assert_eq!(HashAlg::from_name("md5").digest(&[b"abc"], &mut out), None);
    }

    #[test]
    fn pbkdf2_matches_rfc6070() {
        // RFC 6070 test vector 2: "password"/"salt", 2 iterations, HMAC-SHA1.
        let mut out = [0; 20];
        assert_eq!(
            HashAlg::Sha1.pbkdf2(b"password", b"salt", 2, &mut out),
            Some(())
        );
        assert_eq!(out[..4], [0xea, 0x6c, 0x01, 0x4d]);
        assert_eq!(
            HashAlg::from_name("md5").pbkdf2(b"p", b"s", 1, &mut out),
            None
        );
    }
}
