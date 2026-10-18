//! BLAKE3 digests and Ed25519 key ids, both shown as lowercase hex.

use alloc::string::String;
use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A BLAKE3 hash: of a whole archive, a pkgar entry table, or one file.
///
/// It compares in constant time (through [`blake3::Hash`]), prints as 64
/// lowercase hex digits and serializes as that string.
///
/// # Examples
///
/// ```
/// use fox_core::Blake3Digest;
///
/// let digest = Blake3Digest::of(b"hello");
/// let parsed: Blake3Digest = digest.to_string().parse().unwrap();
/// assert_eq!(digest, parsed);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Blake3Digest(blake3::Hash);

impl Blake3Digest {
    /// Hashes `data` in one call.
    pub fn of(data: &[u8]) -> Self {
        Self(blake3::hash(data))
    }

    /// Wraps raw digest bytes, as stored in pkgar headers and entries.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(blake3::Hash::from_bytes(bytes))
    }

    /// The raw digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

impl From<blake3::Hash> for Blake3Digest {
    fn from(hash: blake3::Hash) -> Self {
        Self(hash)
    }
}

impl fmt::Display for Blake3Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.to_hex().as_str())
    }
}

impl fmt::Debug for Blake3Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blake3Digest({self})")
    }
}

/// A digest or key string that is not 64 hex digits.
// `blake3::HexError` implements `Error` only with `std`, so it is kept as a
// field and shown in the message rather than chained as a source.
#[derive(Debug, thiserror::Error)]
#[error("expected 64 hex digits: {0}")]
pub struct DigestParseError(blake3::HexError);

impl FromStr for Blake3Digest {
    type Err = DigestParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        blake3::Hash::from_hex(s)
            .map(Self)
            .map_err(DigestParseError)
    }
}

impl Serialize for Blake3Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.to_hex().as_str())
    }
}

impl<'de> Deserialize<'de> for Blake3Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// An Ed25519 public key that signed (or may sign) a pkgar header.
///
/// pkgar headers carry the signer's key in the clear, so the key itself is
/// the natural id. It prints as 64 lowercase hex digits, like `pkgar-keys`
/// key files.
///
/// # Examples
///
/// ```
/// use fox_core::KeyId;
///
/// let key = KeyId::from_bytes([0xab; 32]);
/// assert_eq!(key.to_string().len(), 64);
/// assert_eq!(key.to_string().parse::<KeyId>().unwrap(), key);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyId([u8; 32]);

impl KeyId {
    /// Wraps raw public key bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw public key bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same lowercase hex as `Blake3Digest`; a key is not a hash, but the
        // encoding is identical and this avoids a second hex implementation.
        f.write_str(blake3::Hash::from_bytes(self.0).to_hex().as_str())
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyId({self})")
    }
}

impl FromStr for KeyId {
    type Err = DigestParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        blake3::Hash::from_hex(s)
            .map(|hash| Self(*hash.as_bytes()))
            .map_err(DigestParseError)
    }
}

impl Serialize for KeyId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for KeyId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn digest_hex_round_trip() {
        let digest = Blake3Digest::of(b"fox");
        let text = digest.to_string();
        assert_eq!(text.len(), 64);
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(text.parse::<Blake3Digest>().unwrap(), digest);
    }

    #[test]
    fn digest_rejects_bad_hex() {
        assert!("abc".parse::<Blake3Digest>().is_err());
        assert!("zz".repeat(32).parse::<Blake3Digest>().is_err());
    }

    #[test]
    fn key_id_round_trip() {
        let key = KeyId::from_bytes([7; 32]);
        assert_eq!(key.to_string(), "07".repeat(32));
        assert_eq!(key.to_string().parse::<KeyId>().unwrap(), key);
    }
}
