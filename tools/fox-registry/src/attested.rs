//! Whether a builder has already attested an artifact.
//!
//! An artifact list names each attestation's builder only by label, and a
//! label's key can be replaced in `registry.toml` (`configure --builder`).
//! Clients count only attestations whose signature verifies with the
//! label's current key, so `build` and `rebuild` do the same before they
//! skip an artifact: an attestation signed with a rotated-out key is
//! stale, and the builder attests again, replacing it. The attestation and
//! its signature are read from the builder's `--blobs` directory when they
//! are there with the right hash, and otherwise downloaded from the
//! registry's artifact store.

use std::fs;
use std::path::Path;

use fox_core::{AttestationSignature, Blake3Digest, TrustedKeys, verify_attestation};
use fox_git::download_blob;
use fox_index::layout::{Artifact, ArtifactAttestation, RegistryConfig};

/// The most bytes an attestation or its signature may have.
const MAX_ATTESTATION: u64 = 1024 * 1024;

/// Checks attestations by one builder against its current key.
pub(crate) struct Attester<'a> {
    config: &'a RegistryConfig,
    label: &'a str,
    blobs: &'a Path,
}

impl<'a> Attester<'a> {
    /// Checks `label`'s attestations, reading blobs from `blobs` first.
    pub(crate) fn new(config: &'a RegistryConfig, label: &'a str, blobs: &'a Path) -> Self {
        Self {
            config,
            label,
            blobs,
        }
    }

    /// Whether `artifact` carries an attestation by this builder that
    /// verifies with the builder's current key and names this archive. An
    /// attestation that cannot be read counts as missing, with a warning,
    /// so the builder redoes the work rather than skip it.
    pub(crate) fn has_attested(&self, artifact: &Artifact) -> bool {
        let Some(builder) = self
            .config
            .builders
            .iter()
            .find(|builder| builder.label == self.label)
        else {
            return false;
        };
        let mut keys = TrustedKeys::new();
        keys.insert(builder.label.clone(), builder.key);
        artifact
            .attestations
            .iter()
            .filter(|attestation| attestation.builder == self.label)
            .any(|attestation| self.verifies(artifact, attestation, &keys))
    }

    fn verifies(
        &self,
        artifact: &Artifact,
        attestation: &ArtifactAttestation,
        keys: &TrustedKeys,
    ) -> bool {
        let (Some(bytes), Some(signature)) = (
            self.blob(&attestation.attestation_blake3),
            self.blob(&attestation.signature_blake3),
        ) else {
            return false;
        };
        let Some(signature) = std::str::from_utf8(&signature)
            .ok()
            .and_then(|text| AttestationSignature::from_toml(text).ok())
        else {
            return false;
        };
        verify_attestation(&bytes, &signature, keys).is_ok_and(|verified| {
            verified.attestation().output.pkgar_blake3 == artifact.pkgar_blake3
        })
    }

    /// The blob with this hash, from `--blobs` or the artifact store.
    fn blob(&self, hash: &Blake3Digest) -> Option<Vec<u8>> {
        let local = self.blobs.join("blake3").join(hash.to_string());
        if let Ok(bytes) = fs::read(&local)
            && Blake3Digest::of(&bytes) == *hash
        {
            return Some(bytes);
        }
        match download_blob(&self.config.artifact_base_urls, hash, MAX_ATTESTATION) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                eprintln!(
                    "fox-registry: warning: cannot read blob {hash} of an attestation by {}, so it \
                     counts as missing: {}",
                    self.label,
                    crate::chain(&error)
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_index::layout::Builder;

    #[test]
    fn an_unlisted_builder_or_unreadable_attestation_has_not_attested() {
        let mut config = RegistryConfig::new("test");
        let blobs = tempfile::tempdir().unwrap();
        let artifact = Artifact {
            target: "x86_64-unknown-redox".to_owned(),
            cpu_level: None,
            default_features: true,
            features: Vec::new(),
            feature_set_hash: Blake3Digest::of(b"features"),
            pkgar_blake3: Blake3Digest::of(b"archive"),
            size: 7,
            sbom_blake3: Blake3Digest::of(b"sbom"),
            attestations: vec![ArtifactAttestation {
                builder: "ci".to_owned(),
                attestation_blake3: Blake3Digest::of(b"attestation"),
                signature_blake3: Blake3Digest::of(b"signature"),
            }],
        };
        assert!(!Attester::new(&config, "ci", blobs.path()).has_attested(&artifact));
        config.builders.push(Builder {
            label: "ci".to_owned(),
            key: fox_core::KeyId::from_bytes([7; 32]),
        });
        // Listed, but the blobs are nowhere to be found.
        assert!(!Attester::new(&config, "ci", blobs.path()).has_attested(&artifact));
        // A local blob with the wrong contents is not used either.
        let dir = blobs.path().join("blake3");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(Blake3Digest::of(b"signature").to_string()),
            b"other",
        )
        .unwrap();
        assert!(!Attester::new(&config, "ci", blobs.path()).has_attested(&artifact));
    }
}
