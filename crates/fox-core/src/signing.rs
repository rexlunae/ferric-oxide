//! Ed25519 detached signatures with a domain-separation context.
//!
//! Every signed fox format (attestations, registry metadata) signs the bytes
//! `context ‖ message`, where `context` names the format and ends in NUL, so
//! a signature made for one format can never verify as another. Keys use the
//! NaCl layout `pkgar-keys` uses: a 64-byte secret key (seed followed by the
//! public key) and a 32-byte public key ([`KeyId`]).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

use pkgar_core::dryoc::classic::crypto_sign::{crypto_sign_detached, crypto_sign_verify_detached};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::KeyId;

/// An Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ed25519Signature([u8; 64]);

impl Ed25519Signature {
    /// Wraps raw signature bytes.
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    /// The raw signature bytes.
    pub const fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

impl fmt::Display for Ed25519Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut hex = [0u8; 128];
        // Encoding into a buffer of exactly twice the input length cannot fail.
        hex::encode_to_slice(self.0, &mut hex).map_err(|_| fmt::Error)?;
        f.write_str(core::str::from_utf8(&hex).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for Ed25519Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ed25519Signature({self})")
    }
}

/// A string that is not 128 hex digits.
// `hex::FromHexError` implements `Error` only with `std`, so it is kept as a
// field and shown in the message rather than chained as a source.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("expected 128 hex digits: {0}")]
pub struct SignatureParseError(hex::FromHexError);

impl FromStr for Ed25519Signature {
    type Err = SignatureParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0u8; 64];
        hex::decode_to_slice(s, &mut bytes).map_err(SignatureParseError)?;
        Ok(Self(bytes))
    }
}

impl Serialize for Ed25519Signature {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Ed25519Signature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Signs `context ‖ message` with `secret_key` (NaCl layout).
///
/// # Errors
///
/// The signing primitive's error if it fails.
pub fn sign_detached(
    context: &[u8],
    message: &[u8],
    secret_key: &[u8; 64],
) -> Result<Ed25519Signature, pkgar_core::Error> {
    let mut signature = [0u8; 64];
    crypto_sign_detached(&mut signature, &with_context(context, message), secret_key)
        .map_err(pkgar_core::Error::from)?;
    Ok(Ed25519Signature(signature))
}

/// Checks that `signature` is `key`'s signature over `context ‖ message`.
///
/// # Errors
///
/// The verification primitive's error if the signature does not verify.
pub fn verify_detached(
    context: &[u8],
    message: &[u8],
    signature: &Ed25519Signature,
    key: &KeyId,
) -> Result<(), pkgar_core::Error> {
    crypto_sign_verify_detached(
        signature.as_bytes(),
        &with_context(context, message),
        key.as_bytes(),
    )
    .map_err(pkgar_core::Error::from)
}

/// The public key of a NaCl-layout secret key.
pub fn public_key_of(secret_key: &[u8; 64]) -> KeyId {
    let mut public = [0u8; 32];
    public.copy_from_slice(&secret_key[32..]);
    KeyId::from_bytes(public)
}

fn with_context(context: &[u8], message: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(context.len() + message.len());
    bytes.extend_from_slice(context);
    bytes.extend_from_slice(message);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use pkgar_core::dryoc::classic::crypto_sign::crypto_sign_seed_keypair;

    #[test]
    fn contexts_separate_signatures() {
        let secret = crypto_sign_seed_keypair(&[9; 32]).1;
        let key = public_key_of(&secret);
        let signature = sign_detached(b"a\0", b"msg", &secret).unwrap();
        verify_detached(b"a\0", b"msg", &signature, &key).unwrap();
        assert!(verify_detached(b"b\0", b"msg", &signature, &key).is_err());
        assert!(verify_detached(b"a\0", b"msh", &signature, &key).is_err());
        let text = signature.to_string();
        assert_eq!(text.len(), 128);
        assert_eq!(text.parse::<Ed25519Signature>().unwrap(), signature);
    }
}
