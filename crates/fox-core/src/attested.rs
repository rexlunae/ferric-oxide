//! Attestation files beside archives, and checking them against a verified
//! archive.
//!
//! A builder writes three files per artifact: `<stem>.pkgar`, the
//! attestation `<stem>.attest.toml` and its detached signature
//! `<stem>.attest.toml.sig` ([`attestation_path`], [`signature_path`]).
//! [`verify_archive_attestation`] reads the attestation, checks its
//! signature against trusted builder keys and checks that it describes the
//! archive, including the SBOM packed inside it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::archive::Archive;
use crate::{
    ArtifactFacts, AttestationError, AttestationSignature, TrustedKeys, VerifiedAttestation,
    verify_attestation,
};

/// Largest attestation or signature file read, so a hostile file cannot make
/// `fox` buffer an arbitrary amount of data.
const MAX_FILE_SIZE: u64 = 64 * 1024;

/// Why an attestation file could not be read or does not hold.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AttestedError {
    /// The file could not be read.
    #[error("cannot read {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is larger than 64 KiB.
    #[error("{} is larger than 64 KiB", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// The signature file is not UTF-8.
    #[error("{} is not UTF-8", path.display())]
    NotUtf8 {
        /// The file.
        path: PathBuf,
    },
    /// The attestation is invalid, untrusted or does not describe the
    /// archive.
    #[error("attestation {} does not hold", path.display())]
    Attestation {
        /// The attestation file.
        path: PathBuf,
        /// What failed.
        #[source]
        source: AttestationError,
    },
}

/// The attestation file for an archive: `foo.pkgar` gives
/// `foo.attest.toml`.
///
/// # Examples
///
/// ```
/// use fox_core::attested::{attestation_path, signature_path};
/// use std::path::Path;
///
/// let attestation = attestation_path(Path::new("out/hello-1.0.0.pkgar"));
/// assert_eq!(attestation, Path::new("out/hello-1.0.0.attest.toml"));
/// assert_eq!(signature_path(&attestation), Path::new("out/hello-1.0.0.attest.toml.sig"));
/// ```
pub fn attestation_path(archive: &Path) -> PathBuf {
    archive.with_extension("attest.toml")
}

/// The detached signature file for an attestation: its path plus `.sig`.
pub fn signature_path(attestation: &Path) -> PathBuf {
    let mut path = attestation.as_os_str().to_owned();
    path.push(".sig");
    PathBuf::from(path)
}

/// Reads an attestation file and its signature file.
///
/// # Errors
///
/// [`AttestedError::Io`] or [`AttestedError::TooLarge`] if either file
/// cannot be read, [`AttestedError::NotUtf8`] if the signature file is not
/// text, and [`AttestedError::Attestation`] if it is not a signature file.
pub fn read_attestation(
    attestation: &Path,
) -> Result<(Vec<u8>, AttestationSignature), AttestedError> {
    let bytes = read_limited(attestation)?;
    let sig_path = signature_path(attestation);
    let sig_bytes = read_limited(&sig_path)?;
    let text = std::str::from_utf8(&sig_bytes).map_err(|_| AttestedError::NotUtf8 {
        path: sig_path.clone(),
    })?;
    let signature =
        AttestationSignature::from_toml(text).map_err(|source| AttestedError::Attestation {
            path: sig_path,
            source,
        })?;
    Ok((bytes, signature))
}

/// Verifies the attestation at `attestation` (and its `.sig`) against the
/// trusted `builders` and checks that it describes `archive`.
///
/// # Errors
///
/// The errors of [`read_attestation`], and [`AttestedError::Attestation`]
/// with the [`AttestationError`] from [`verify_attestation`] or
/// [`VerifiedAttestation::check_artifact`].
pub fn verify_archive_attestation(
    archive: &Archive,
    attestation: &Path,
    builders: &TrustedKeys,
) -> Result<VerifiedAttestation, AttestedError> {
    let (bytes, signature) = read_attestation(attestation)?;
    let wrap = |source| AttestedError::Attestation {
        path: attestation.to_path_buf(),
        source,
    };
    let verified = verify_attestation(&bytes, &signature, builders).map_err(wrap)?;
    verified
        .check_artifact(&artifact_facts(archive))
        .map_err(wrap)?;
    Ok(verified)
}

/// What a verified archive says about itself.
pub fn artifact_facts(archive: &Archive) -> ArtifactFacts<'_> {
    ArtifactFacts {
        name: &archive.manifest().name,
        version: &archive.manifest().version,
        pkgar_blake3: archive.blake3(),
        pkgar_size: archive.size(),
        header_signer: archive.head().signer().key,
        entries_blake3: archive.head().entries_blake3(),
        files: archive.files(),
    }
}

fn read_limited(path: &Path) -> Result<Vec<u8>, AttestedError> {
    let io_error = |source| AttestedError::Io {
        path: path.to_path_buf(),
        source,
    };
    let len = fs::metadata(path).map_err(io_error)?.len();
    if len > MAX_FILE_SIZE {
        return Err(AttestedError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    fs::read(path).map_err(io_error)
}
