//! Prebuilt registry artifacts: picking the variant for this system,
//! downloading it by hash, and accepting it only when enough of the
//! registry's builders attest to building exactly this archive from the
//! release's source.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use fox_core::archive::{Archive, ArchiveError};
use fox_core::attested::artifact_facts;
use fox_core::{Attestation, AttestationSignature, GitUrl, KeyId, verify_attestation};
use fox_git::{DownloadError, download_blob};
use fox_index::layout::{Artifact, PackageEntry, RegistryConfig, Release};

/// Largest attestation or signature blob downloaded.
const MAX_ATTESTATION: u64 = 1024 * 1024;

/// Why a prebuilt artifact was not accepted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PrebuiltError {
    #[error(transparent)]
    Download(#[from] Box<DownloadError>),
    #[error("cannot write {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("prebuilt archive does not verify against the registry's builders")]
    Archive(#[source] Box<ArchiveError>),
    #[error("{valid} of the registry's builders attest to this archive, {threshold} needed")]
    NotEnoughBuilders { valid: usize, threshold: u32 },
}

impl From<DownloadError> for PrebuiltError {
    fn from(error: DownloadError) -> Self {
        Self::Download(Box::new(error))
    }
}

/// The variant a request needs.
pub(crate) struct Wanted<'a> {
    pub(crate) target: &'a str,
    pub(crate) default_features: bool,
    pub(crate) features: &'a [String],
}

/// The artifacts of `artifacts` that match `wanted`. CPU-level variants
/// other than the x86-64-v3 baseline are not picked yet.
pub(crate) fn candidates<'a>(artifacts: &'a [Artifact], wanted: &Wanted<'_>) -> Vec<&'a Artifact> {
    let mut features: Vec<&str> = wanted.features.iter().map(String::as_str).collect();
    features.sort_unstable();
    features.dedup();
    artifacts
        .iter()
        .filter(|artifact| {
            let mut built: Vec<&str> = artifact.features.iter().map(String::as_str).collect();
            built.sort_unstable();
            built.dedup();
            artifact.target == wanted.target
                && artifact
                    .cpu_level
                    .is_none_or(|level| level == fox_core::CpuLevel::X86_64V3)
                && artifact.default_features == wanted.default_features
                && built == features
        })
        .collect()
}

/// Downloads `artifact` into `work_dir` and accepts it if at least
/// `threshold` distinct builders of `config` made valid attestations of it
/// that match the release's repository, commit, tree and directory.
pub(crate) fn fetch(
    work_dir: &Path,
    config: &RegistryConfig,
    entry: &PackageEntry,
    release: &Release,
    artifact: &Artifact,
    threshold: u32,
) -> Result<Archive, PrebuiltError> {
    let urls = &config.artifact_base_urls;
    let bytes = download_blob(urls, &artifact.pkgar_blake3, artifact.size)?;
    let path = work_dir.join(format!("{}-{}.pkgar", entry.name, release.version));
    let io_error = |source| PrebuiltError::Io {
        path: path.clone(),
        source,
    };
    fs::create_dir_all(work_dir).map_err(io_error)?;
    fs::write(&path, bytes).map_err(io_error)?;
    let builders = config.builder_keys();
    let archive = Archive::open(&path, &builders)
        .map_err(|source| PrebuiltError::Archive(Box::new(source)))?;
    let facts = artifact_facts(&archive);
    let mut valid: BTreeSet<KeyId> = BTreeSet::new();
    for attestation in &artifact.attestations {
        let checked = download_blob(urls, &attestation.attestation_blake3, MAX_ATTESTATION)
            .and_then(|bytes| {
                download_blob(urls, &attestation.signature_blake3, MAX_ATTESTATION)
                    .map(|signature| (bytes, signature))
            });
        let (bytes, signature) = match checked {
            Ok(blobs) => blobs,
            Err(error) => {
                warn(&attestation.builder, &error);
                continue;
            }
        };
        let Some(signature) = std::str::from_utf8(&signature)
            .ok()
            .and_then(|text| AttestationSignature::from_toml(text).ok())
        else {
            eprintln!(
                "fox: warning: attestation by {} has an invalid signature file",
                attestation.builder
            );
            continue;
        };
        let verified = match verify_attestation(&bytes, &signature, &builders)
            .and_then(|verified| verified.check_artifact(&facts).map(|()| verified))
        {
            Ok(verified) => verified,
            Err(error) => {
                warn(&attestation.builder, &error);
                continue;
            }
        };
        match source_mismatch(verified.attestation(), entry, release, artifact) {
            None => {
                valid.insert(verified.signer().key);
            }
            Some(field) => eprintln!(
                "fox: warning: attestation by {} names a different {field}",
                attestation.builder
            ),
        }
    }
    if (valid.len() as u64) < u64::from(threshold) {
        return Err(PrebuiltError::NotEnoughBuilders {
            valid: valid.len(),
            threshold,
        });
    }
    Ok(archive)
}

fn warn(builder: &str, error: &dyn std::error::Error) {
    eprintln!(
        "fox: warning: ignoring the attestation by {builder}: {}",
        crate::commands::chain(error)
    );
}

/// The first field in which the attestation disagrees with the registry.
fn source_mismatch(
    attestation: &Attestation,
    entry: &PackageEntry,
    release: &Release,
    artifact: &Artifact,
) -> Option<&'static str> {
    let source = &attestation.source;
    let checks = [
        (
            GitUrl::new(&source.git).ok().as_ref() == Some(&entry.git),
            "repository",
        ),
        (attestation.package.name == entry.name, "package name"),
        (source.commit == release.commit, "commit"),
        (source.tree_blake3 == release.tree_blake3, "tree"),
        (source.path == entry.path, "package directory"),
        (attestation.build.target == artifact.target, "target"),
        (
            attestation.build.feature_set_hash == artifact.feature_set_hash,
            "feature set",
        ),
        (
            attestation.output.sbom_blake3 == artifact.sbom_blake3,
            "SBOM",
        ),
        (
            attestation.package.version == release.version.to_string(),
            "version",
        ),
    ];
    checks
        .into_iter()
        .find(|(ok, _)| !ok)
        .map(|(_, field)| field)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_core::{Blake3Digest, PackageName};

    fn artifact(target: &str, default_features: bool, features: &[&str]) -> Artifact {
        let h = Blake3Digest::of(b"x");
        Artifact {
            target: target.to_owned(),
            cpu_level: None,
            default_features,
            features: features.iter().map(|f| (*f).to_owned()).collect(),
            feature_set_hash: h,
            pkgar_blake3: h,
            size: 1,
            sbom_blake3: h,
            attestations: Vec::new(),
        }
    }

    fn attested(name: &str) -> fox_core::Attestation {
        use fox_core::{
            AttestationFormat, AttestedPackage, BuildRecord, KeyId, OutputRecord, Profile,
            SourceRecord,
        };
        let h = Blake3Digest::of(b"x");
        let key = KeyId::from_bytes([1; 32]);
        fox_core::Attestation {
            format: AttestationFormat::V1,
            package: AttestedPackage {
                name: PackageName::new(name).unwrap(),
                version: "1.0.0".into(),
            },
            source: SourceRecord {
                git: "https://example.com/tools".into(),
                commit: "ab".repeat(20),
                tree_blake3: h,
                cargo_lock_blake3: h,
                path: None,
            },
            build: BuildRecord {
                target: "x86_64-unknown-redox".into(),
                cpu_level: None,
                features: Vec::new(),
                feature_set_hash: h,
                toolchain_id: "rust".into(),
                profile: Profile::Release,
                auditable: false,
                builder: "ci".into(),
                builder_key: key,
                built_at: "2026-10-17T02:00:00Z".parse().unwrap(),
            },
            output: OutputRecord {
                pkgar_blake3: h,
                pkgar_size: 1,
                pkgar_header_signer: key,
                entries_blake3: h,
                sbom_blake3: h,
            },
        }
    }

    #[test]
    fn an_artifact_of_another_package_from_the_same_source_is_refused() {
        let pair = ssh_key::private::Ed25519Keypair::from_seed(&[3; 32]);
        let owner = ssh_key::PublicKey::from(pair.public).to_openssh().unwrap();
        let text = format!(
            "name = \"cli\"\nkind = \"app\"\ngit = \"https://example.com/tools\"\n\n\
             [[owners]]\nid = \"me\"\nkey = \"{owner}\"\n"
        );
        let entry = PackageEntry::from_toml(&text, "packages/cl/cli.toml").unwrap();
        let release = Release {
            version: semver::Version::new(1, 0, 0),
            tag: "v1.0.0".into(),
            commit: "ab".repeat(20),
            tree_blake3: Blake3Digest::of(b"x"),
            yanked: false,
        };
        let same = attested("cli");
        let artifact = Artifact::from_attestation(&same, true, Vec::new());
        assert_eq!(source_mismatch(&same, &entry, &release, &artifact), None);
        let other = attested("daemon");
        assert_eq!(
            source_mismatch(&other, &entry, &release, &artifact),
            Some("package name")
        );
    }

    #[test]
    fn variants_match_target_and_features() {
        let artifacts = [
            artifact("x86_64-unknown-redox", true, &[]),
            artifact("x86_64-unknown-redox", true, &["tls", "http2"]),
            artifact("wasm32-wasip1", true, &[]),
        ];
        let wanted = |target, features: &'static [String]| Wanted {
            target,
            default_features: true,
            features,
        };
        assert_eq!(
            candidates(&artifacts, &wanted("x86_64-unknown-redox", &[])).len(),
            1
        );
        let features = ["http2".to_owned(), "tls".to_owned()];
        let found = candidates(
            &artifacts,
            &Wanted {
                target: "x86_64-unknown-redox",
                default_features: true,
                features: &features,
            },
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].features.len(), 2);
        assert!(candidates(&artifacts, &wanted("aarch64-unknown-redox", &[])).is_empty());
    }
}
