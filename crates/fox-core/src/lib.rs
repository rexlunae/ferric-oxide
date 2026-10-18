//! Package identity, manifest, trust and verification types shared by every
//! `fox` tool.
//!
//! `fox` is the Ferric Oxide package manager (see
//! `packaging/packaging-design.md`). Its artifacts are unchanged upstream
//! Redox [`pkgar`](https://gitlab.redox-os.org/redox-os/pkgar) archives: an
//! Ed25519-signed header carrying the BLAKE3 hash of an entry table, and one
//! entry per file carrying that file's BLAKE3 hash. This crate holds the parts
//! every consumer of those archives needs, so the host tool, the target-side
//! `fox`, the installer and a future boot-time verifier agree on them:
//!
//! - [`Blake3Digest`] and [`KeyId`]: content hashes and signer keys;
//! - [`PackageName`] and [`Manifest`]: who a package is, read from the signed
//!   manifest file inside the archive ([`manifest_path`]);
//! - [`TrustedKeys`] and [`verify_head`]: which signers are accepted, and the
//!   check of a pkgar head (header plus entry table) against them;
//! - [`FileRecord`]: one verified file, as recorded in the installed-package
//!   database and in SBOMs;
//! - [`Attestation`], [`AttestationSignature`] and [`verify_attestation`]:
//!   a builder's signed statement binding a source tree, toolchain, target
//!   and feature set to an archive and its SBOM, and its verification;
//! - [`CpuLevel`] and [`feature_set_hash`]: parts of an artifact's
//!   identity;
//! - [`GitUrl`]: a repository URL in the normalized form used as a source's
//!   identity.
//!
//! The crate is `no_std` with `alloc`. The `std` feature adds the `archive`
//! module, which opens `.pkgar` files and hashes every entry's data, the
//! `attested` module, which reads attestation files and checks them against
//! an archive, the `keys` module, which loads trusted keys from disk, and the
//! `trust` module, the store of pinned repository authors and trusted
//! third-party builders. The `test-fixtures`
//! feature adds `fixtures`, which builds signed packages for tests.
//!
//! # Examples
//!
//! Checking a head that no trusted key signed:
//!
//! ```
//! use fox_core::{TrustedKeys, VerifyError, verify_head};
//!
//! let keys = TrustedKeys::new();
//! let head = [0u8; fox_core::HEADER_SIZE];
//! assert!(matches!(verify_head(&head, &keys), Err(VerifyError::NoTrustedKeys)));
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(feature = "std")]
pub mod archive;
pub mod attestation;
#[cfg(feature = "std")]
pub mod attested;
mod digest;
#[cfg(feature = "test-fixtures")]
pub mod fixtures;
mod git_url;
#[cfg(feature = "std")]
pub mod keys;
mod manifest;
mod name;
mod record;
#[cfg(feature = "std")]
pub mod trust;
mod verify;

pub use attestation::{
    ArtifactFacts, Attestation, AttestationError, AttestationFormat, AttestationSignature,
    AttestedField, AttestedPackage, BuildRecord, CpuLevel, CpuLevelError, Ed25519Signature,
    OutputRecord, Profile, SIGNING_CONTEXT, SignatureFormat, SignatureParseError, SourceRecord,
    VerifiedAttestation, feature_set_hash, verify_attestation,
};
pub use digest::{Blake3Digest, DigestParseError, KeyId};
pub use git_url::{GitUrl, GitUrlError};
pub use manifest::{
    MANIFEST_DIR, Manifest, ManifestError, ManifestFormat, SBOM_DIR, SourceInfo, manifest_path,
    sbom_path,
};
pub use name::{NameError, PackageName};
pub use pkgar_core::{ENTRY_SIZE, HEADER_SIZE};
pub use record::{FileKind, FileRecord};
pub use verify::{
    EntryPathError, TrustedKey, TrustedKeys, VerifiedEntry, VerifiedHead, VerifyError,
    check_entry_path, head_size, verify_head,
};
