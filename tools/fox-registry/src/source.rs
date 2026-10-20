//! Checking releases against their git repositories: the tag is signed by
//! one of the package's owners, and names the recorded commit and tree.

use std::path::{Path, PathBuf};

use fox_git::{GitError, Repository};
use fox_index::layout::{PackageEntry, Release, ReleaseMismatch};
use semver::Version;

/// Why a release does not check out.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceError {
    /// Fetching or reading the repository, or the tag signature, failed.
    #[error(transparent)]
    Git(#[from] Box<GitError>),
    /// The tag name is not a version.
    #[error("tag {tag} does not name a version")]
    NoVersion { tag: String },
    /// The tag names another version than the release records.
    #[error("tag {tag} names version {found}, but the release is {recorded}")]
    Version {
        tag: String,
        found: Version,
        recorded: Version,
    },
    /// The recorded release disagrees with the repository.
    #[error("release {version}")]
    Mismatch {
        version: Version,
        #[source]
        source: ReleaseMismatch,
    },
}

impl From<GitError> for SourceError {
    fn from(error: GitError) -> Self {
        Self::Git(Box::new(error))
    }
}

/// Where [`fetch`] keeps the package's repository under `cache_root`.
pub(crate) fn cache_path(entry: &PackageEntry, cache_root: &Path) -> PathBuf {
    let digest = blake3::hash(entry.git.as_str().as_bytes()).to_hex();
    cache_root.join(&digest[..16])
}

/// Fetches the package's repository into a cache under `cache_root`.
pub(crate) fn fetch(entry: &PackageEntry, cache_root: &Path) -> Result<Repository, SourceError> {
    Ok(fox_git::fetch(
        entry.git.as_str(),
        &cache_path(entry, cache_root),
    )?)
}

/// Reads tag `tag`, checks it is signed by an owner, and describes it as a
/// release. Also returns the signing owner's id.
pub(crate) fn release_from_tag(
    entry: &PackageEntry,
    repo: &Repository,
    tag: &str,
) -> Result<(Release, String), SourceError> {
    let found = repo.tag(tag)?;
    let key = found.verify_signed_by(entry.owners.iter().map(|owner| &owner.key))?;
    let owner = entry
        .owner_with_key(key)
        .map_or_else(String::new, |owner| owner.id.clone());
    let version = found.version().ok_or_else(|| SourceError::NoVersion {
        tag: tag.to_owned(),
    })?;
    let tree_blake3 = repo.tree_blake3(found.commit())?;
    let release = Release {
        version,
        tag: tag.to_owned(),
        commit: found.commit().id(),
        tree_blake3,
        yanked: false,
    };
    Ok((release, owner))
}

/// Checks a recorded release against the repository.
pub(crate) fn check_release(
    entry: &PackageEntry,
    repo: &Repository,
    release: &Release,
) -> Result<(), SourceError> {
    let (actual, _) = release_from_tag(entry, repo, &release.tag)?;
    if actual.version != release.version {
        return Err(SourceError::Version {
            tag: release.tag.clone(),
            found: actual.version,
            recorded: release.version.clone(),
        });
    }
    release
        .check_source(&actual.commit, &actual.tree_blake3)
        .map_err(|source| SourceError::Mismatch {
            version: release.version.clone(),
            source,
        })
}
