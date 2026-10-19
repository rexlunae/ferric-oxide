//! Prebuilt registry artifacts: picking the variant for this system,
//! downloading it by hash, and accepting it only when enough of the
//! registry's builders attest to building exactly this archive from the
//! release's source.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use fox_core::archive::{Archive, ArchiveError};
use fox_core::attested::artifact_facts;
use fox_core::{Attestation, AttestationSignature, CpuLevel, GitUrl, KeyId, verify_attestation};
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
    /// The highest x86-64 level the machine runs; ignored for other
    /// targets.
    pub(crate) cpu_level: CpuLevel,
    pub(crate) default_features: bool,
    pub(crate) features: &'a [String],
}

/// The artifacts of `artifacts` that match `wanted`, best first: for an
/// x86-64 target, the highest CPU level not above the wanted one (an
/// artifact naming no level is the baseline); for another target, only
/// artifacts naming no level.
pub(crate) fn candidates<'a>(artifacts: &'a [Artifact], wanted: &Wanted<'_>) -> Vec<&'a Artifact> {
    let mut features: Vec<&str> = wanted.features.iter().map(String::as_str).collect();
    features.sort_unstable();
    features.dedup();
    let x86_64 = CpuLevel::applies_to(wanted.target);
    let level = |artifact: &Artifact| artifact.cpu_level.unwrap_or(CpuLevel::BASELINE);
    let mut found: Vec<&Artifact> = artifacts
        .iter()
        .filter(|artifact| {
            let mut built: Vec<&str> = artifact.features.iter().map(String::as_str).collect();
            built.sort_unstable();
            built.dedup();
            let level_ok = if x86_64 {
                level(artifact) <= wanted.cpu_level
            } else {
                artifact.cpu_level.is_none()
            };
            artifact.target == wanted.target
                && level_ok
                && artifact.default_features == wanted.default_features
                && built == features
        })
        .collect();
    // Stable, so equal levels keep the registry's order.
    found.sort_by_key(|artifact| std::cmp::Reverse(level(artifact)));
    found
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
            attestation.build.cpu_level == artifact.cpu_level,
            "CPU level",
        ),
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

    /// The registry's entry for `cli` and its release that [`attested`]
    /// describes.
    fn cli_release() -> (PackageEntry, Release) {
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
        (entry, release)
    }

    #[test]
    fn an_artifact_of_another_package_from_the_same_source_is_refused() {
        let (entry, release) = cli_release();
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
            cpu_level: CpuLevel::BASELINE,
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
                cpu_level: CpuLevel::BASELINE,
                default_features: true,
                features: &features,
            },
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].features.len(), 2);
        assert!(candidates(&artifacts, &wanted("aarch64-unknown-redox", &[])).is_empty());
    }

    fn at_level(target: &str, level: Option<CpuLevel>, tag: &[u8]) -> Artifact {
        let mut artifact = artifact(target, true, &[]);
        artifact.cpu_level = level;
        artifact.pkgar_blake3 = Blake3Digest::of(tag);
        artifact
    }

    #[test]
    fn the_highest_level_the_machine_runs_comes_first() {
        let v3 = Some(CpuLevel::X86_64V3);
        let v4 = Some(CpuLevel::X86_64V4);
        let x86 = "x86_64-unknown-redox";
        let artifacts = [
            at_level(x86, v3, b"v3"),
            at_level(x86, v4, b"v4"),
            at_level(x86, None, b"none"),
        ];
        let pick = |cpu_level| {
            candidates(
                &artifacts,
                &Wanted {
                    target: x86,
                    cpu_level,
                    default_features: true,
                    features: &[],
                },
            )
            .into_iter()
            .map(|artifact| artifact.pkgar_blake3)
            .collect::<Vec<_>>()
        };
        let hash = |tag: &[u8]| Blake3Digest::of(tag);
        // A v4 machine prefers v4 and falls back to the baseline builds.
        assert_eq!(
            pick(CpuLevel::X86_64V4),
            [hash(b"v4"), hash(b"v3"), hash(b"none")]
        );
        // A v3 machine never gets a v4 build.
        assert_eq!(pick(CpuLevel::X86_64V3), [hash(b"v3"), hash(b"none")]);
    }

    #[test]
    fn other_targets_take_only_artifacts_without_a_level() {
        let arm = "aarch64-unknown-redox";
        let artifacts = [
            at_level(arm, Some(CpuLevel::X86_64V3), b"odd"),
            at_level(arm, None, b"plain"),
        ];
        let found = candidates(
            &artifacts,
            &Wanted {
                target: arm,
                cpu_level: CpuLevel::X86_64V4,
                default_features: true,
                features: &[],
            },
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pkgar_blake3, Blake3Digest::of(b"plain"));
    }

    #[test]
    fn an_attestation_of_another_level_is_refused() {
        let (entry, release) = cli_release();
        let mut attestation = attested("cli");
        let artifact = Artifact::from_attestation(&attestation, true, Vec::new());
        attestation.build.cpu_level = Some(CpuLevel::X86_64V4);
        assert_eq!(
            source_mismatch(&attestation, &entry, &release, &artifact),
            Some("CPU level")
        );
    }
}
