//! `fox-registry build`: what a registry builder runs for a new release.
//! It checks the release against its owner-signed tag, builds it with the
//! hermetic `fox-build` pipeline, signs the archive and attestation with the
//! builder's key, stores the three blobs as `<blobs>/blake3/<hash>`, and
//! records the attestation in the release's artifact list. Uploading the
//! blobs to the registry's artifact store and running `sign` are left to
//! the CI job.

use std::fs;
use std::path::{Path, PathBuf};

use fox_build::{BuildError, BuildRequest, FeatureSelection, Signer};
use fox_core::{Blake3Digest, KeyId, PackageName};
use fox_index::edit::{EditError, Registry};
use fox_index::layout::{Artifact, ArtifactAttestation, ArtifactList};
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
}

/// Builds one release and records the artifact; returns its archive hash.
pub(crate) fn build(registry: &Registry, job: &BuildJob<'_>) -> Result<Blake3Digest, CiError> {
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
    request.target = job.target.clone();
    request.features = FeatureSelection::default();
    let built = fox_build::build(&request)?;
    let packaged = fox_build::package(&built, &signer, &job.work_dir.join("out"))?;

    store_blob(job.blobs, &packaged.archive)?;
    let attestation = ArtifactAttestation {
        builder: job.label.to_owned(),
        attestation_blake3: store_blob(job.blobs, &packaged.attestation)?,
        signature_blake3: store_blob(job.blobs, &packaged.signature)?,
    };
    let artifact = Artifact::from_attestation(&packaged.contents, true, Vec::new());
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
