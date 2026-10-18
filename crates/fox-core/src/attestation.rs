//! Build attestations: signed statements that an archive is what a source
//! tree builds to.
//!
//! A builder that runs the hermetic build (`fox-build`) writes one
//! attestation per artifact, as TOML (packaging design, section 5.2):
//!
//! ```toml
//! format = "fox-attestation/1"
//!
//! [package]
//! name = "hello"
//! version = "1.0.0"
//!
//! [source]
//! git = "https://github.com/example/hello"
//! commit = "3f1c0e5a9d2b4c6e8f0a1b3c5d7e9f1a2b4c6d8e"
//! tree_blake3 = "a91e…"
//! cargo_lock_blake3 = "77d0…"
//!
//! [build]
//! target = "x86_64-unknown-redox"
//! cpu_level = "x86_64-v3"
//! features = ["default"]
//! feature_set_hash = "c2b4…"
//! toolchain_id = "rust-1.93.0-nightly:9e1f…"
//! profile = "release"
//! auditable = false
//! builder = "ferric-ci-1"
//! builder_key = "5b0c…"
//! built_at = 2026-10-17T02:00:00Z
//!
//! [output]
//! pkgar_blake3 = "5d3a…"
//! pkgar_size = 1843200
//! pkgar_header_signer = "5b0c…"
//! entries_blake3 = "0be1…"
//! sbom_blake3 = "e40f…"
//! ```
//!
//! The attestation is signed with the builder's Ed25519 key in a detached
//! signature file ([`AttestationSignature`], `<attestation>.sig`):
//!
//! ```toml
//! format = "fox-signature/1"
//! key = "5b0c…"
//! signature = "…128 hex digits…"
//! ```
//!
//! The signature covers [`SIGNING_CONTEXT`] followed by the exact bytes of
//! the attestation file, so a signature over an attestation can never be
//! taken for a signature over anything else, and verification needs no
//! canonical form: [`verify_attestation`] checks the bytes first and only
//! then parses them.
//!
//! What is bound, and how it is checked against an archive
//! ([`VerifiedAttestation::check_artifact`]):
//!
//! - `source`: the git commit and the BLAKE3 tree hash of its contents (see
//!   `fox-git`), and the BLAKE3 of the `Cargo.lock` the build used;
//! - `build`: the target, CPU level, resolved feature set
//!   ([`feature_set_hash`]), toolchain, profile and builder;
//! - `output`: the archive's BLAKE3 and size, the key that signed its pkgar
//!   header, its signed entry-table hash, and the BLAKE3 of the CycloneDX
//!   SBOM packed inside it at [`sbom_path`].
//!
//! `entries_blake3` does not depend on the header signer, so independent
//! rebuilders that sign archives with their own keys can still show they
//! reproduced the same files.
//!
//! The field names follow SLSA provenance, so SLSA or in-toto statements
//! can be produced from an attestation later without changing what is
//! signed.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::str::FromStr;

use pkgar_core::dryoc::classic::crypto_sign::{crypto_sign_detached, crypto_sign_verify_detached};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Blake3Digest, FileRecord, KeyId, PackageName, TrustedKey, TrustedKeys, sbom_path};

/// Bytes signed before the attestation itself, for domain separation.
pub const SIGNING_CONTEXT: &[u8] = b"fox-attestation-signature/1\0";

/// Prefix of the bytes hashed by [`feature_set_hash`].
const FEATURE_SET_CONTEXT: &[u8] = b"fox-feature-set/1\0";

/// The attestation format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AttestationFormat {
    /// The first format, written `"fox-attestation/1"`.
    #[default]
    #[serde(rename = "fox-attestation/1")]
    V1,
}

/// The detached signature format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SignatureFormat {
    /// The first format, written `"fox-signature/1"`.
    #[default]
    #[serde(rename = "fox-signature/1")]
    V1,
}

/// An x86-64 microarchitecture level a native artifact is built for.
///
/// Ferric Oxide user space targets x86-64-v3 by default; v4 is an optional
/// build. Older levels are not offered.
///
/// # Examples
///
/// ```
/// use fox_core::CpuLevel;
///
/// let level: CpuLevel = "v4".parse()?;
/// assert_eq!(level, CpuLevel::X86_64V4);
/// assert_eq!(level.as_str(), "x86_64-v4");
/// assert_eq!(level.target_cpu(), "x86-64-v4");
/// # Ok::<(), fox_core::CpuLevelError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CpuLevel {
    /// x86-64-v3 (AVX2, BMI2, FMA): the baseline.
    #[serde(rename = "x86_64-v3")]
    X86_64V3,
    /// x86-64-v4 (AVX-512).
    #[serde(rename = "x86_64-v4")]
    X86_64V4,
}

/// A string that names no known [`CpuLevel`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown CPU level {0:?}; expected v3 or v4")]
pub struct CpuLevelError(String);

impl CpuLevel {
    /// The name used in attestations and file names, such as `x86_64-v3`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64V3 => "x86_64-v3",
            Self::X86_64V4 => "x86_64-v4",
        }
    }

    /// The value for rustc's `-C target-cpu`, the same one `features.toml`
    /// passes for this level.
    pub const fn target_cpu(self) -> &'static str {
        match self {
            Self::X86_64V3 => "x86-64-v3",
            Self::X86_64V4 => "x86-64-v4",
        }
    }
}

impl fmt::Display for CpuLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CpuLevel {
    type Err = CpuLevelError;

    /// Accepts `v3`, `x86_64-v3` and `x86-64-v3`, and the same for v4.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "v3" | "x86_64-v3" | "x86-64-v3" => Ok(Self::X86_64V3),
            "v4" | "x86_64-v4" | "x86-64-v4" => Ok(Self::X86_64V4),
            _ => Err(CpuLevelError(s.into())),
        }
    }
}

/// The Cargo profile an artifact was built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Profile {
    /// `--release`.
    #[default]
    Release,
}

impl Profile {
    /// The profile name, as Cargo spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
        }
    }
}

/// Which package an attestation is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestedPackage {
    /// The package name, as in its manifest.
    pub name: PackageName,
    /// The package version, as in its manifest.
    pub version: String,
}

/// The source an artifact was built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRecord {
    /// Git repository URL.
    pub git: String,
    /// Full hex id of the built commit.
    pub commit: String,
    /// BLAKE3 tree hash of the commit's contents (`fox-git`'s `fox-tree/1`).
    pub tree_blake3: Blake3Digest,
    /// BLAKE3 of the `Cargo.lock` the build used.
    pub cargo_lock_blake3: Blake3Digest,
    /// Directory of the package inside the repository, with `/`
    /// separators, when it is not the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// How an artifact was built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildRecord {
    /// Rust target triple.
    pub target: String,
    /// CPU level, for native x86-64 artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_level: Option<CpuLevel>,
    /// The package's enabled Cargo features after resolution, sorted.
    pub features: Vec<String>,
    /// [`feature_set_hash`] of `features` and the `Cargo.lock` hash.
    pub feature_set_hash: Blake3Digest,
    /// The toolchain: its release and a hash of its version details.
    pub toolchain_id: String,
    /// Cargo profile.
    pub profile: Profile,
    /// Whether binaries embed their dependency list (`cargo auditable`).
    pub auditable: bool,
    /// Label of the builder, such as `ferric-ci-1`.
    pub builder: String,
    /// The builder's Ed25519 key, which must sign this attestation.
    pub builder_key: KeyId,
    /// When the build ran.
    pub built_at: toml::value::Datetime,
}

/// The artifact a build produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRecord {
    /// BLAKE3 of the whole `.pkgar` file.
    pub pkgar_blake3: Blake3Digest,
    /// Size of the `.pkgar` file in bytes.
    pub pkgar_size: u64,
    /// The key that signed the pkgar header.
    pub pkgar_header_signer: KeyId,
    /// The entry-table hash signed in the pkgar header.
    pub entries_blake3: Blake3Digest,
    /// BLAKE3 of the CycloneDX SBOM packed in the archive.
    pub sbom_blake3: Blake3Digest,
}

/// A build attestation (see the [module docs](crate::attestation)).
///
/// # Examples
///
/// Signing an attestation and verifying it against the builder's key:
///
/// ```
/// use fox_core::{AttestationSignature, KeyId, TrustedKeys, verify_attestation};
/// # use fox_core::{Attestation, AttestationFormat, AttestedPackage, Blake3Digest,
/// #     BuildRecord, OutputRecord, PackageName, Profile, SourceRecord};
/// # use pkgar_core::dryoc::classic::crypto_sign::crypto_sign_seed_keypair;
/// # let (_, secret) = crypto_sign_seed_keypair(&[7; 32]);
/// # let key = KeyId::from_bytes(secret[32..].try_into().unwrap());
/// # let digest = Blake3Digest::of(b"x");
/// # let attestation = Attestation {
/// #     format: AttestationFormat::V1,
/// #     package: AttestedPackage { name: PackageName::new("hello")?, version: "1.0.0".into() },
/// #     source: SourceRecord { git: "https://example.com/hello".into(), commit: "00".repeat(20),
/// #         tree_blake3: digest, cargo_lock_blake3: digest, path: None },
/// #     build: BuildRecord { target: "x86_64-unknown-redox".into(), cpu_level: None,
/// #         features: vec![], feature_set_hash: digest, toolchain_id: "rust-1.93.0:00".into(),
/// #         profile: Profile::Release, auditable: false, builder: "ci".into(), builder_key: key,
/// #         built_at: "2026-10-17T02:00:00Z".parse()? },
/// #     output: OutputRecord { pkgar_blake3: digest, pkgar_size: 1, pkgar_header_signer: key,
/// #         entries_blake3: digest, sbom_blake3: digest },
/// # };
/// let bytes = attestation.to_toml()?;
/// let signature = AttestationSignature::sign(bytes.as_bytes(), &secret)?;
///
/// let mut builders = TrustedKeys::new();
/// builders.insert("ci", signature.key);
/// let verified = verify_attestation(bytes.as_bytes(), &signature, &builders)?;
/// assert_eq!(verified.attestation(), &attestation);
/// assert_eq!(verified.signer().label, "ci");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// Format version.
    pub format: AttestationFormat,
    /// The package.
    pub package: AttestedPackage,
    /// The source.
    pub source: SourceRecord,
    /// The build.
    pub build: BuildRecord,
    /// The artifact.
    pub output: OutputRecord,
}

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

/// A detached signature over an attestation file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationSignature {
    /// Format version.
    pub format: SignatureFormat,
    /// The signing key.
    pub key: KeyId,
    /// The signature over [`SIGNING_CONTEXT`] and the attestation bytes.
    pub signature: Ed25519Signature,
}

impl AttestationSignature {
    /// Signs the attestation file contents `attestation` with an Ed25519
    /// secret key in NaCl layout (seed followed by public key), as
    /// `pkgar-keys` secret key files hold it.
    ///
    /// # Errors
    ///
    /// [`AttestationError::Sign`] if the signing primitive fails.
    pub fn sign(attestation: &[u8], secret_key: &[u8; 64]) -> Result<Self, AttestationError> {
        let mut signature = [0u8; 64];
        crypto_sign_detached(&mut signature, &signed_message(attestation), secret_key)
            .map_err(|error| AttestationError::Sign(error.into()))?;
        let mut public = [0u8; 32];
        public.copy_from_slice(&secret_key[32..]);
        Ok(Self {
            format: SignatureFormat::V1,
            key: KeyId::from_bytes(public),
            signature: Ed25519Signature(signature),
        })
    }

    /// Parses a signature file.
    ///
    /// # Errors
    ///
    /// [`AttestationError::SignatureParse`] if the text is not a valid
    /// signature file.
    pub fn from_toml(text: &str) -> Result<Self, AttestationError> {
        toml::from_str(text).map_err(AttestationError::SignatureParse)
    }

    /// Serializes the signature file.
    ///
    /// # Errors
    ///
    /// [`AttestationError::Serialize`] if serialization fails.
    pub fn to_toml(&self) -> Result<String, AttestationError> {
        toml::to_string(self).map_err(AttestationError::Serialize)
    }
}

/// A field of an attestation's `output` or `package` table that did not
/// match the artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttestedField {
    /// `package.name`.
    Name,
    /// `package.version`.
    Version,
    /// `output.pkgar_blake3`.
    PkgarBlake3,
    /// `output.pkgar_size`.
    PkgarSize,
    /// `output.pkgar_header_signer`.
    HeaderSigner,
    /// `output.entries_blake3`.
    EntriesBlake3,
    /// `output.sbom_blake3`.
    SbomBlake3,
}

impl fmt::Display for AttestedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Name => "package.name",
            Self::Version => "package.version",
            Self::PkgarBlake3 => "output.pkgar_blake3",
            Self::PkgarSize => "output.pkgar_size",
            Self::HeaderSigner => "output.pkgar_header_signer",
            Self::EntriesBlake3 => "output.entries_blake3",
            Self::SbomBlake3 => "output.sbom_blake3",
        })
    }
}

/// Why an attestation could not be signed, read or verified, or does not
/// describe an artifact.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AttestationError {
    /// No builder key is trusted, so nothing can verify.
    #[error("no trusted builder keys are configured")]
    NoTrustedKeys,
    /// The attestation was signed by a key that is not trusted.
    #[error("attestation is signed by untrusted key {key}")]
    UntrustedBuilder {
        /// The signing key.
        key: KeyId,
    },
    /// The signature does not verify.
    #[error("bad attestation signature")]
    BadSignature(#[source] pkgar_core::Error),
    /// Signing failed.
    #[error("cannot sign attestation")]
    Sign(#[source] pkgar_core::Error),
    /// The attestation is not UTF-8.
    #[error("attestation is not UTF-8")]
    NotUtf8,
    /// The attestation is not valid TOML or does not match the schema.
    #[error("invalid attestation")]
    Parse(#[source] toml::de::Error),
    /// The signature file is not valid TOML or does not match the schema.
    #[error("invalid attestation signature file")]
    SignatureParse(#[source] toml::de::Error),
    /// The attestation or signature could not be serialized.
    #[error("cannot serialize attestation")]
    Serialize(#[source] toml::ser::Error),
    /// The attestation names a builder key other than the one that signed it.
    #[error("attestation names builder key {claimed} but is signed by {signer}")]
    BuilderKeyMismatch {
        /// `build.builder_key`.
        claimed: KeyId,
        /// The key that signed it.
        signer: KeyId,
    },
    /// The archive holds no SBOM at the expected path.
    #[error("archive has no SBOM at {path}")]
    MissingSbom {
        /// Where the SBOM was expected.
        path: String,
    },
    /// A field does not match the artifact.
    #[error("{field} in the attestation does not match the archive")]
    Mismatch {
        /// The field.
        field: AttestedField,
    },
}

impl Attestation {
    /// Parses an attestation from TOML.
    ///
    /// This does not check any signature; use [`verify_attestation`] for
    /// attestations from elsewhere.
    ///
    /// # Errors
    ///
    /// [`AttestationError::Parse`] if the text is not a valid attestation.
    pub fn from_toml(text: &str) -> Result<Self, AttestationError> {
        toml::from_str(text).map_err(AttestationError::Parse)
    }

    /// Serializes the attestation to TOML.
    ///
    /// # Errors
    ///
    /// [`AttestationError::Serialize`] if serialization fails.
    pub fn to_toml(&self) -> Result<String, AttestationError> {
        toml::to_string(self).map_err(AttestationError::Serialize)
    }
}

/// An attestation whose signature was checked against trusted builder keys.
#[derive(Debug, Clone)]
pub struct VerifiedAttestation {
    attestation: Attestation,
    signer: TrustedKey,
}

/// What an archive says about itself, to compare with an attestation.
///
/// With the `std` feature, `attested::artifact_facts` fills this in from a
/// verified archive.
#[derive(Debug, Clone, Copy)]
pub struct ArtifactFacts<'a> {
    /// The package name from the archive's manifest.
    pub name: &'a PackageName,
    /// The package version from the archive's manifest.
    pub version: &'a str,
    /// BLAKE3 of the archive file.
    pub pkgar_blake3: Blake3Digest,
    /// Size of the archive file.
    pub pkgar_size: u64,
    /// The key that signed the pkgar header.
    pub header_signer: KeyId,
    /// The signed entry-table hash.
    pub entries_blake3: Blake3Digest,
    /// The archive's verified files.
    pub files: &'a [FileRecord],
}

impl VerifiedAttestation {
    /// The attestation.
    pub fn attestation(&self) -> &Attestation {
        &self.attestation
    }

    /// The trusted builder key that signed it.
    pub fn signer(&self) -> &TrustedKey {
        &self.signer
    }

    /// Checks that the attestation describes the archive `facts` came from:
    /// the package name and version, the archive hash and size, the header
    /// signer and entry-table hash, and the SBOM packed at
    /// [`sbom_path`].
    ///
    /// # Errors
    ///
    /// [`AttestationError::Mismatch`] naming the first field that differs,
    /// and [`AttestationError::MissingSbom`] if the archive has no SBOM.
    pub fn check_artifact(&self, facts: &ArtifactFacts<'_>) -> Result<(), AttestationError> {
        let a = &self.attestation;
        let checks = [
            (a.package.name == *facts.name, AttestedField::Name),
            (a.package.version == facts.version, AttestedField::Version),
            (
                a.output.pkgar_blake3 == facts.pkgar_blake3,
                AttestedField::PkgarBlake3,
            ),
            (
                a.output.pkgar_size == facts.pkgar_size,
                AttestedField::PkgarSize,
            ),
            (
                a.output.pkgar_header_signer == facts.header_signer,
                AttestedField::HeaderSigner,
            ),
            (
                a.output.entries_blake3 == facts.entries_blake3,
                AttestedField::EntriesBlake3,
            ),
        ];
        if let Some((_, field)) = checks.iter().find(|(ok, _)| !ok) {
            return Err(AttestationError::Mismatch { field: *field });
        }
        let path = sbom_path(facts.name);
        let sbom = facts
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or(AttestationError::MissingSbom { path })?;
        if sbom.blake3 != a.output.sbom_blake3 {
            return Err(AttestationError::Mismatch {
                field: AttestedField::SbomBlake3,
            });
        }
        Ok(())
    }
}

/// Verifies `signature` over the attestation file contents `attestation`
/// with the trusted builder keys, then parses it.
///
/// The key is looked up in `builders` before the signature is checked, and
/// the attestation's own `build.builder_key` must be that key.
///
/// # Errors
///
/// - [`AttestationError::NoTrustedKeys`] if `builders` is empty;
/// - [`AttestationError::UntrustedBuilder`] if the signing key is not in it;
/// - [`AttestationError::BadSignature`] if the signature does not verify;
/// - [`AttestationError::NotUtf8`] or [`AttestationError::Parse`] if the
///   signed bytes are not a valid attestation;
/// - [`AttestationError::BuilderKeyMismatch`] if the attestation names
///   another builder key.
pub fn verify_attestation(
    attestation: &[u8],
    signature: &AttestationSignature,
    builders: &TrustedKeys,
) -> Result<VerifiedAttestation, AttestationError> {
    if builders.is_empty() {
        return Err(AttestationError::NoTrustedKeys);
    }
    let signer = builders
        .get(&signature.key)
        .ok_or(AttestationError::UntrustedBuilder { key: signature.key })?;
    crypto_sign_verify_detached(
        signature.signature.as_bytes(),
        &signed_message(attestation),
        signature.key.as_bytes(),
    )
    .map_err(|error| AttestationError::BadSignature(error.into()))?;
    let text = core::str::from_utf8(attestation).map_err(|_| AttestationError::NotUtf8)?;
    let attestation = Attestation::from_toml(text)?;
    if attestation.build.builder_key != signature.key {
        return Err(AttestationError::BuilderKeyMismatch {
            claimed: attestation.build.builder_key,
            signer: signature.key,
        });
    }
    Ok(VerifiedAttestation {
        attestation,
        signer,
    })
}

fn signed_message(attestation: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(SIGNING_CONTEXT.len() + attestation.len());
    message.extend_from_slice(SIGNING_CONTEXT);
    message.extend_from_slice(attestation);
    message
}

/// The feature-set part of an artifact's identity: BLAKE3 over the sorted,
/// de-duplicated enabled Cargo features and the `Cargo.lock` hash.
///
/// The hashed bytes are `fox-feature-set/1\0`, then each feature followed by
/// a NUL byte, then the 32 raw bytes of `cargo_lock`. Feature names cannot
/// contain NUL, so the encoding is unambiguous.
///
/// # Examples
///
/// ```
/// use fox_core::{Blake3Digest, feature_set_hash};
///
/// let lock = Blake3Digest::of(b"Cargo.lock contents");
/// assert_eq!(
///     feature_set_hash(&["std", "default"], &lock),
///     feature_set_hash(&["default", "std", "std"], &lock),
/// );
/// assert_ne!(feature_set_hash(&["std"], &lock), feature_set_hash::<&str>(&[], &lock));
/// ```
pub fn feature_set_hash<S: AsRef<str>>(features: &[S], cargo_lock: &Blake3Digest) -> Blake3Digest {
    let mut sorted: Vec<&str> = features.iter().map(AsRef::as_ref).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(FEATURE_SET_CONTEXT);
    for feature in sorted {
        hasher.update(feature.as_bytes());
        hasher.update(&[0]);
    }
    hasher.update(cargo_lock.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;
    use pkgar_core::dryoc::classic::crypto_sign::crypto_sign_seed_keypair;

    use crate::FileKind;

    fn secret(seed: u8) -> [u8; 64] {
        crypto_sign_seed_keypair(&[seed; 32]).1
    }

    fn key_of(secret: &[u8; 64]) -> KeyId {
        KeyId::from_bytes(secret[32..].try_into().unwrap())
    }

    fn sbom_file(contents: &[u8]) -> FileRecord {
        FileRecord {
            path: "usr/share/fox/sbom/hello.cdx.json".into(),
            kind: FileKind::File,
            mode: 0o644,
            size: contents.len() as u64,
            blake3: Blake3Digest::of(contents),
        }
    }

    fn attestation(builder: KeyId) -> Attestation {
        let lock = Blake3Digest::of(b"lock");
        Attestation {
            format: AttestationFormat::V1,
            package: AttestedPackage {
                name: PackageName::new("hello").unwrap(),
                version: "1.0.0".into(),
            },
            source: SourceRecord {
                git: "https://example.com/hello".into(),
                commit: "ab".repeat(20),
                tree_blake3: Blake3Digest::of(b"tree"),
                cargo_lock_blake3: lock,
                path: Some("crates/hello".into()),
            },
            build: BuildRecord {
                target: "x86_64-unknown-redox".into(),
                cpu_level: Some(CpuLevel::X86_64V3),
                features: vec!["default".into()],
                feature_set_hash: feature_set_hash(&["default"], &lock),
                toolchain_id: "rust-1.93.0:00".into(),
                profile: Profile::Release,
                auditable: false,
                builder: "ci".into(),
                builder_key: builder,
                built_at: "2026-10-17T02:00:00Z".parse().unwrap(),
            },
            output: OutputRecord {
                pkgar_blake3: Blake3Digest::of(b"pkgar"),
                pkgar_size: 1234,
                pkgar_header_signer: builder,
                entries_blake3: Blake3Digest::of(b"entries"),
                sbom_blake3: Blake3Digest::of(b"sbom"),
            },
        }
    }

    fn trusting(key: KeyId) -> TrustedKeys {
        let mut keys = TrustedKeys::new();
        keys.insert("ci", key);
        keys
    }

    #[test]
    fn round_trips_and_names_format() {
        let attestation = attestation(KeyId::from_bytes([1; 32]));
        let text = attestation.to_toml().unwrap();
        assert!(text.starts_with("format = \"fox-attestation/1\""));
        assert!(text.contains("cpu_level = \"x86_64-v3\""));
        assert!(text.contains("built_at = 2026-10-17T02:00:00Z"));
        assert_eq!(Attestation::from_toml(&text).unwrap(), attestation);
        let other = text.replace("fox-attestation/1", "fox-attestation/2");
        assert!(matches!(
            Attestation::from_toml(&other),
            Err(AttestationError::Parse(_))
        ));
    }

    #[test]
    fn signs_and_verifies() {
        let secret = secret(1);
        let key = key_of(&secret);
        let text = attestation(key).to_toml().unwrap();
        let signature = AttestationSignature::sign(text.as_bytes(), &secret).unwrap();
        assert_eq!(signature.key, key);
        let sig_text = signature.to_toml().unwrap();
        assert!(sig_text.contains("fox-signature/1"));
        let parsed = AttestationSignature::from_toml(&sig_text).unwrap();
        assert_eq!(parsed, signature);
        let verified = verify_attestation(text.as_bytes(), &parsed, &trusting(key)).unwrap();
        assert_eq!(verified.signer().label, "ci");
        assert_eq!(verified.attestation().package.version, "1.0.0");
    }

    #[test]
    fn rejects_untrusted_tampered_and_mislabelled() {
        let secret = secret(2);
        let key = key_of(&secret);
        let text = attestation(key).to_toml().unwrap();
        let signature = AttestationSignature::sign(text.as_bytes(), &secret).unwrap();

        assert!(matches!(
            verify_attestation(text.as_bytes(), &signature, &TrustedKeys::new()),
            Err(AttestationError::NoTrustedKeys)
        ));
        let other = KeyId::from_bytes([9; 32]);
        assert!(matches!(
            verify_attestation(text.as_bytes(), &signature, &trusting(other)),
            Err(AttestationError::UntrustedBuilder { key: k }) if k == key
        ));
        let tampered = text.replace("1.0.0", "1.0.1");
        assert!(matches!(
            verify_attestation(tampered.as_bytes(), &signature, &trusting(key)),
            Err(AttestationError::BadSignature(_))
        ));
        // A signature made without the context prefix does not verify.
        let mut raw = [0u8; 64];
        crypto_sign_detached(&mut raw, text.as_bytes(), &secret).unwrap();
        let bare = AttestationSignature {
            signature: Ed25519Signature::from_bytes(raw),
            ..signature.clone()
        };
        assert!(matches!(
            verify_attestation(text.as_bytes(), &bare, &trusting(key)),
            Err(AttestationError::BadSignature(_))
        ));
        // Signed by a trusted key, but claiming another builder.
        let claims_other = attestation(other).to_toml().unwrap();
        let signature = AttestationSignature::sign(claims_other.as_bytes(), &secret).unwrap();
        assert!(matches!(
            verify_attestation(claims_other.as_bytes(), &signature, &trusting(key)),
            Err(AttestationError::BuilderKeyMismatch { .. })
        ));
    }

    #[test]
    fn checks_artifact_facts() {
        let secret = secret(3);
        let key = key_of(&secret);
        let mut attestation = attestation(key);
        attestation.output.sbom_blake3 = Blake3Digest::of(b"{}");
        let text = attestation.to_toml().unwrap();
        let signature = AttestationSignature::sign(text.as_bytes(), &secret).unwrap();
        let verified = verify_attestation(text.as_bytes(), &signature, &trusting(key)).unwrap();

        let name = PackageName::new("hello").unwrap();
        let files = vec![sbom_file(b"{}")];
        let facts = ArtifactFacts {
            name: &name,
            version: "1.0.0",
            pkgar_blake3: attestation.output.pkgar_blake3,
            pkgar_size: 1234,
            header_signer: key,
            entries_blake3: attestation.output.entries_blake3,
            files: &files,
        };
        verified.check_artifact(&facts).unwrap();

        let wrong_size = ArtifactFacts {
            pkgar_size: 1,
            ..facts
        };
        assert!(matches!(
            verified.check_artifact(&wrong_size),
            Err(AttestationError::Mismatch {
                field: AttestedField::PkgarSize
            })
        ));
        let other_sbom = vec![sbom_file(b"[]")];
        let swapped = ArtifactFacts {
            files: &other_sbom,
            ..facts
        };
        assert!(matches!(
            verified.check_artifact(&swapped),
            Err(AttestationError::Mismatch {
                field: AttestedField::SbomBlake3
            })
        ));
        let none = ArtifactFacts {
            files: &[],
            ..facts
        };
        assert!(matches!(
            verified.check_artifact(&none),
            Err(AttestationError::MissingSbom { .. })
        ));
    }

    #[test]
    fn cpu_levels_parse() {
        assert_eq!("x86-64-v3".parse::<CpuLevel>().unwrap(), CpuLevel::X86_64V3);
        assert_eq!(CpuLevel::X86_64V4.to_string(), "x86_64-v4");
        assert!("v2".parse::<CpuLevel>().is_err());
    }

    #[test]
    fn feature_set_hash_binds_lockfile() {
        let a = Blake3Digest::of(b"a");
        let b = Blake3Digest::of(b"b");
        assert_ne!(feature_set_hash(&["x"], &a), feature_set_hash(&["x"], &b));
        // "ab" is not the same set as "a" + "b".
        assert_ne!(
            feature_set_hash(&["ab"], &a),
            feature_set_hash(&["a", "b"], &a)
        );
    }

    #[test]
    fn signature_hex_round_trip() {
        let signature = Ed25519Signature::from_bytes([0xa5; 64]);
        let text = signature.to_string();
        assert_eq!(text, "a5".repeat(64));
        assert_eq!(text.parse::<Ed25519Signature>().unwrap(), signature);
        assert!("a5".parse::<Ed25519Signature>().is_err());
    }
}
