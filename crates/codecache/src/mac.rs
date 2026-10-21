//! The entry MAC.
//!
//! An entry's MAC is BLAKE3 in keyed mode, under a 32-byte key only the kernel holds, over:
//!
//! 1. the domain string `ferric-codecache/1` followed by a NUL byte;
//! 2. the [`CacheKey`] encoding;
//! 3. each section in order (text, read-only data, metadata, relocations) as its length
//!    (`u64`, little-endian) followed by its bytes.
//!
//! The lengths make the input unambiguous, so bytes cannot move from one section to the
//! next without changing the MAC. The header's magic, version and flags are fixed values
//! that [`Entry::parse`](crate::entry::Entry::parse) checks, and the version is in the
//! domain string. Comparison is constant-time ([`blake3::Hash`]'s equality).

use core::fmt;

use crate::key::CacheKey;

/// The domain string the MAC input starts with.
pub const DOMAIN: &[u8] = b"ferric-codecache/1\0";

/// The MAC key. Its [`Debug`](fmt::Debug) output never shows the bytes.
pub struct MacKey([u8; 32]);

impl MacKey {
    /// A key from its bytes. The kernel draws them from its random source at boot.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for MacKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MacKey(..)")
    }
}

/// A MAC value. Equality is constant-time.
#[derive(Clone, Copy)]
pub struct Mac([u8; 32]);

impl Mac {
    /// The size in bytes.
    pub const SIZE: usize = 32;

    /// A MAC from its stored bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The MAC of an entry's key and sections (text, read-only data, metadata,
    /// relocations).
    pub fn compute(secret: &MacKey, key: &CacheKey, sections: [&[u8]; 4]) -> Self {
        let mut hasher = blake3::Hasher::new_keyed(&secret.0);
        hasher.update(DOMAIN);
        hasher.update(&key.encode());
        for section in sections {
            // usize is at most 64 bits on every supported target.
            hasher.update(&(section.len() as u64).to_le_bytes());
            hasher.update(section);
        }
        Self(*hasher.finalize().as_bytes())
    }
}

impl PartialEq for Mac {
    fn eq(&self, other: &Self) -> bool {
        blake3::Hash::from_bytes(self.0) == blake3::Hash::from_bytes(other.0)
    }
}

impl Eq for Mac {}

impl fmt::Debug for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Mac(")?;
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))?;
        f.write_str(")")
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::format;

    #[test]
    fn the_mac_covers_lengths_and_the_key() {
        let secret = MacKey::from_bytes([7; 32]);
        let key = CacheKey::example();
        let base = Mac::compute(&secret, &key, [b"ab", b"c", b"", b""]);
        // Moving a byte across a section boundary changes the MAC.
        assert_ne!(base, Mac::compute(&secret, &key, [b"a", b"bc", b"", b""]));
        let mut other = key;
        other.verifier = 1;
        assert_ne!(base, Mac::compute(&secret, &other, [b"ab", b"c", b"", b""]));
        let wrong = MacKey::from_bytes([8; 32]);
        assert_ne!(base, Mac::compute(&wrong, &key, [b"ab", b"c", b"", b""]));
        assert_eq!(base, Mac::compute(&secret, &key, [b"ab", b"c", b"", b""]));
        assert_eq!(format!("{secret:?}"), "MacKey(..)");
    }

    #[test]
    fn the_mac_matches_a_plain_keyed_hash() {
        // Recomputes the documented input by hand, so the layout cannot drift silently.
        let secret = [7; 32];
        let key = CacheKey::example();
        let mut input = std::vec::Vec::new();
        input.extend_from_slice(DOMAIN);
        input.extend_from_slice(&key.encode());
        for section in [&b"text"[..], b"", b"meta", b""] {
            input.extend_from_slice(&(section.len() as u64).to_le_bytes());
            input.extend_from_slice(section);
        }
        let expected = blake3::keyed_hash(&secret, &input);
        let mac = Mac::compute(
            &MacKey::from_bytes(secret),
            &key,
            [b"text", b"", b"meta", b""],
        );
        assert_eq!(mac.as_bytes(), expected.as_bytes());
    }
}
