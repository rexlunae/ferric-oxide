//! `fox-registry build` and `rebuild`: what registry builders run.
//!
//! `build` checks a release against its owner-signed tag, builds it with
//! the hermetic `fox-build` pipeline, signs the archive and attestation with
//! the builder's key, stores the three blobs as `<blobs>/blake3/<hash>`, and
//! records the attestation in the release's artifact list.
//!
//! `rebuild` is the independent rebuilder: it builds the release the same
//! way, downloads the archive another builder published, and, only if the
//! rebuild staged exactly the archive's files, signs its own attestation of
//! that archive and adds it to the same artifact. Clients can then require
//! several builders (`min-builder-signatures`).
//!
//! Uploading the blobs to the registry's artifact store and running `sign`
//! are left to the CI job.

use std::fs;
use std::path::{Path, PathBuf};

use fox_build::{Build, BuildError, BuildRequest, FeatureSelection, Signer};
use fox_core::archive::{Archive, ArchiveError};
use fox_core::{Blake3Digest, CpuLevel, KeyId, PackageName};
use fox_git::{DownloadError, download_blob};
use fox_index::edit::{EditError, Registry};
use fox_index::layout::{Artifact, ArtifactAttestation, ArtifactList, RegistryConfig};
use semver::Version;

use crate::source::{self, SourceError};

/// Why a build could not be recorded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CiError {
    #[error(transparent)]
    Edit(#[from] EditError),
    #[error("cannot check {name} {version}")]
    Source {
        name: PackageName,
        version: Version,
        #[source]
        source: Box<SourceError>,
    },
    #[error("build failed")]
    Build(#[source] Box<BuildError>),
    #[error("no release {version} of {name}")]
    NoRelease { name: PackageName, version: Version },
    #[error("builder {label} with key {key} is not listed in registry.toml")]
    NotABuilder { label: String, key: KeyId },
    #[error("release {version} of {name} built package {found} {found_version}")]
    Mismatch {
        name: PackageName,
        version: Version,
        found: PackageName,
        found_version: String,
    },
    #[error("no artifact of {name} {version} for this target and level to rebuild")]
    NothingToRebuild { name: PackageName, version: Version },
    #[error("cannot download the published archive {pkgar}")]
    Download {
        pkgar: Blake3Digest,
        #[source]
        source: Box<DownloadError>,
    },
    #[error("the published archive {pkgar} does not verify against the registry's builders")]
    Published {
        pkgar: Blake3Digest,
        #[source]
        source: Box<ArchiveError>,
    },
    #[error("cannot store blob {}", path.display())]
    Blob {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl From<BuildError> for CiError {
    fn from(error: BuildError) -> Self {
        Self::Build(Box::new(error))
    }
}

/// What to build.
pub(crate) struct BuildJob<'a> {
    pub(crate) name: &'a PackageName,
    pub(crate) version: &'a Version,
    pub(crate) key: &'a Path,
    pub(crate) label: &'a str,
    pub(crate) blobs: &'a Path,
    pub(crate) work_dir: &'a Path,
    pub(crate) cache: &'a Path,
    pub(crate) target: Option<String>,
    pub(crate) cpu_level: Option<CpuLevel>,
}

/// Builds one release and records the artifact; returns its archive hash.
pub(crate) fn build(registry: &Registry, job: &BuildJob<'_>) -> Result<Blake3Digest, CiError> {
    let (built, signer) = build_release(registry, job)?;
    let packaged = fox_build::package(&built, &signer, &job.work_dir.join("out"))?;
    store_blob(job.blobs, &packaged.archive)?;
    let artifact = Artifact::from_attestation(&packaged.contents, true, Vec::new());
    record(registry, job, artifact, &packaged)
}

/// Rebuilds one release and attests to every published archive of this
/// target and level, not yet attested by `job.label`, that the rebuild
/// reproduces; returns their hashes.
///
/// # Errors
///
/// [`CiError::NothingToRebuild`] if no such archive is listed, and
/// [`CiError::Build`] with [`BuildError::NotReproduced`] if the rebuild
/// matches none of them.
pub(crate) fn rebuild(
    registry: &Registry,
    job: &BuildJob<'_>,
) -> Result<Vec<Blake3Digest>, CiError> {
    let pending: Vec<Artifact> = registry
        .artifacts(job.name, job.version)?
        .map(|list| {
            list.artifacts
                .into_iter()
                .filter(|artifact| {
                    crate::unattested(
                        artifact,
                        job.label,
                        job.target.as_deref().unwrap_or_default(),
                        job.cpu_level,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if pending.is_empty() {
        return Err(CiError::NothingToRebuild {
            name: job.name.clone(),
            version: job.version.clone(),
        });
    }
    let (built, signer) = build_release(registry, job)?;
    let config = registry.config()?;
    let mut reproduced = Vec::new();
    let mut last = None;
    for artifact in pending {
        let (archive, path) = published(job, &config, &artifact)?;
        match fox_build::reattest(&built, &signer, &archive, &path, &job.work_dir.join("out")) {
            Ok(packaged) => {
                reproduced.push(record(registry, job, artifact, &packaged)?);
            }
            Err(error @ BuildError::NotReproduced { .. }) => last = Some(error),
            Err(error) => return Err(error.into()),
        }
    }
    match last {
        Some(error) if reproduced.is_empty() => Err(error.into()),
        _ => Ok(reproduced),
    }
}

/// Downloads a published archive into the work directory and opens it
/// against the registry's builders.
fn published(
    job: &BuildJob<'_>,
    config: &RegistryConfig,
    artifact: &Artifact,
) -> Result<(Archive, PathBuf), CiError> {
    let pkgar = artifact.pkgar_blake3;
    let bytes =
        download_blob(&config.artifact_base_urls, &pkgar, artifact.size).map_err(|source| {
            CiError::Download {
                pkgar,
                source: Box::new(source),
            }
        })?;
    let dir = job.work_dir.join("published");
    let path = dir.join(format!("{pkgar}.pkgar"));
    let error = |source| CiError::Blob {
        path: path.clone(),
        source,
    };
    fs::create_dir_all(&dir).map_err(error)?;
    fs::write(&path, bytes).map_err(error)?;
    let archive =
        Archive::open(&path, &config.builder_keys()).map_err(|source| CiError::Published {
            pkgar,
            source: Box::new(source),
        })?;
    Ok((archive, path))
}

/// Checks the release against its tag and the builder against
/// `registry.toml`, and builds it.
fn build_release(registry: &Registry, job: &BuildJob<'_>) -> Result<(Build, Signer), CiError> {
    let entry = registry
        .package(job.name)?
        .ok_or_else(|| EditError::UnknownPackage {
            name: job.name.clone(),
        })?;
    let release = entry
        .release(job.version)
        .ok_or_else(|| CiError::NoRelease {
            name: job.name.clone(),
            version: job.version.clone(),
        })?
        .clone();
    let source_error = |source| CiError::Source {
        name: job.name.clone(),
        version: job.version.clone(),
        source: Box::new(source),
    };
    let repo = source::fetch(&entry, job.cache).map_err(source_error)?;
    source::check_release(&entry, &repo, &release).map_err(source_error)?;

    let signer = Signer::load(job.key, job.label)?;
    let config = registry.config()?;
    if !config
        .builders
        .iter()
        .any(|builder| builder.label == job.label && builder.key == signer.key())
    {
        return Err(CiError::NotABuilder {
            label: job.label.to_owned(),
            key: signer.key(),
        });
    }
    let mut request = BuildRequest::new(&source::cache_path(&entry, job.cache), job.work_dir);
    request.rev = release.commit.clone();
    request.git_url = Some(entry.git.to_string());
    request.package_dir = entry.path.as_ref().map(PathBuf::from).unwrap_or_default();
    // The registry names the package; a workspace root may hold another.
    request.package = Some(job.name.to_string());
    request.target = job.target.clone();
    request.cpu_level = job.cpu_level;
    request.features = FeatureSelection::default();
    let built = fox_build::build(&request)?;
    // Clients check the attestation against the release, so an artifact
    // of another package or version would never be used.
    if &built.manifest.name != job.name || built.manifest.version != job.version.to_string() {
        return Err(CiError::Mismatch {
            name: job.name.clone(),
            version: job.version.clone(),
            found: built.manifest.name.clone(),
            found_version: built.manifest.version.clone(),
        });
    }
    Ok((built, signer))
}

/// Stores the attestation blobs and records them on `artifact` in the
/// release's artifact list; returns the archive hash.
fn record(
    registry: &Registry,
    job: &BuildJob<'_>,
    artifact: Artifact,
    packaged: &fox_build::Packaged,
) -> Result<Blake3Digest, CiError> {
    let attestation = ArtifactAttestation {
        builder: job.label.to_owned(),
        attestation_blake3: store_blob(job.blobs, &packaged.attestation)?,
        signature_blake3: store_blob(job.blobs, &packaged.signature)?,
    };
    let pkgar = artifact.pkgar_blake3;
    let mut list = registry
        .artifacts(job.name, job.version)?
        .unwrap_or_else(|| ArtifactList {
            name: job.name.clone(),
            version: job.version.clone(),
            artifacts: Vec::new(),
        });
    list.add_attestation(artifact, attestation);
    registry.write_artifacts(&list)?;
    Ok(pkgar)
}

/// Copies `path` to `<blobs>/blake3/<hash>` and returns the hash.
fn store_blob(blobs: &Path, path: &Path) -> Result<Blake3Digest, CiError> {
    let error = |source| CiError::Blob {
        path: path.to_path_buf(),
        source,
    };
    let bytes = fs::read(path).map_err(error)?;
    let hash = Blake3Digest::of(&bytes);
    let dir = blobs.join("blake3");
    fs::create_dir_all(&dir).map_err(error)?;
    fs::write(dir.join(hash.to_string()), bytes).map_err(error)?;
    Ok(hash)
}
