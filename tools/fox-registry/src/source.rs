//! Checking releases against their git repositories: the tag is signed by
//! one of the package's owners, and names the recorded commit and tree.

use std::path::{Path, PathBuf};

use fox_git::{GitError, Repository, TagSigner};
use fox_index::layout::{Owner, PackageEntry, Release};
use semver::Version;

/// Why a release does not check out.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceError {
    /// Fetching or reading the repository failed.
    #[error(transparent)]
    Git(#[from] Box<GitError>),
    /// The tag has no SSH signature.
    #[error("tag {tag} is not SSH-signed")]
    Unsigned { tag: String },
    /// The tag is signed by a key that is not an owner's.
    #[error("tag {tag} is signed by {fingerprint}, which is not an owner of {package}")]
    NotAnOwner {
        tag: String,
        fingerprint: String,
        package: String,
    },
    /// The tag name is not a version.
    #[error("tag {tag} does not name a version")]
    NoVersion { tag: String },
    /// The recorded release disagrees with the repository.
    #[error("release {version}: the registry says {field} {recorded}, the repository has {actual}")]
    Mismatch {
        version: Version,
        field: &'static str,
        recorded: String,
        actual: String,
    },
}

impl From<GitError> for SourceError {
    fn from(error: GitError) -> Self {
        Self::Git(Box::new(error))
    }
}

/// Fetches the package's repository into a cache under `cache_root`.
pub(crate) fn fetch(entry: &PackageEntry, cache_root: &Path) -> Result<Repository, SourceError> {
    let url = entry.git.as_str();
    let digest = blake3::hash(url.as_bytes()).to_hex();
    let cache: PathBuf = cache_root.join(&digest[..16]);
    Ok(fox_git::fetch(url, &cache)?)
}

/// Reads tag `tag`, checks it is signed by an owner, and describes it as a
/// release. Also returns the signing owner's id.
pub(crate) fn release_from_tag(
    entry: &PackageEntry,
    repo: &Repository,
    tag: &str,
) -> Result<(Release, String), SourceError> {
    let found = repo.tag(tag)?;
    let owner = signing_owner(entry, &found)?;
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
    Ok((release, owner.id.clone()))
}

/// Checks a recorded release against the repository.
pub(crate) fn check_release(
    entry: &PackageEntry,
    repo: &Repository,
    release: &Release,
) -> Result<(), SourceError> {
    let (actual, _) = release_from_tag(entry, repo, &release.tag)?;
    let mismatch = |field, recorded: String, actual: String| SourceError::Mismatch {
        version: release.version.clone(),
        field,
        recorded,
        actual,
    };
    if actual.version != release.version {
        return Err(mismatch(
            "version",
            release.version.to_string(),
            actual.version.to_string(),
        ));
    }
    if actual.commit != release.commit {
        return Err(mismatch("commit", release.commit.clone(), actual.commit));
    }
    if actual.tree_blake3 != release.tree_blake3 {
        return Err(mismatch(
            "tree_blake3",
            release.tree_blake3.to_string(),
            actual.tree_blake3.to_string(),
        ));
    }
    Ok(())
}

fn signing_owner<'a>(
    entry: &'a PackageEntry,
    tag: &fox_git::Tag,
) -> Result<&'a Owner, SourceError> {
    let key = match tag.signer() {
        TagSigner::Ssh(key) => key,
        _ => {
            return Err(SourceError::Unsigned {
                tag: tag.name().to_owned(),
            });
        }
    };
    let owner = entry
        .owner_with_key(&key)
        .ok_or_else(|| SourceError::NotAnOwner {
            tag: tag.name().to_owned(),
            fingerprint: key.fingerprint(),
            package: entry.name.to_string(),
        })?;
    tag.verify(&owner.key)?;
    Ok(owner)
}
