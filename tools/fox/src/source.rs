//! Git sources: fetching, release-tag checks with trust on first use,
//! building with the local builder key, and accepting prebuilt archives
//! from trusted builders.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use fox_build::{BuildError, BuildRequest, FeatureSelection, Packaged, Signer, default_work_dir};
use fox_core::archive::{Archive, ArchiveError};
use fox_core::attested::{AttestedError, attestation_path, verify_archive_attestation};
use fox_core::keys::{LOCAL_BUILDER_KEY, LOCAL_BUILDER_LABEL, LOCAL_BUILDER_SECRET_KEY};
use fox_core::trust::{AuthorKey, AuthorTrust, TrustError, TrustStore};
use fox_core::{GitUrl, GitUrlError, TrustedKeys};
use fox_git::{GitError, Repository, Tag, TagSigner};
use fox_store::STATE_DIR;

use crate::SourceArgs;

/// Directory, under the store's state, of the bare repository caches.
const GIT_CACHE: &str = "git";

/// Why a git source could not be used.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceError {
    #[error("{spec:?} is neither an archive, a directory nor a git URL")]
    Spec {
        spec: String,
        #[source]
        source: GitUrlError,
    },
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Trust(#[from] TrustError),
    // The large errors are boxed.
    #[error(transparent)]
    Build(Box<BuildError>),
    #[error(transparent)]
    Archive(Box<ArchiveError>),
    #[error("prebuilt archive {} has no valid attestation from a trusted builder", path.display())]
    PrebuiltAttestation {
        path: PathBuf,
        #[source]
        source: Box<AttestedError>,
    },
    #[error("prebuilt archive {} was built from a different {field}", path.display())]
    PrebuiltMismatch { path: PathBuf, field: &'static str },
    #[error("{what} of {git} is not signed; pass --allow-unsigned to build it anyway")]
    Unsigned { git: GitUrl, what: String },
    #[error(
        "tag {tag} of {git} is signed by {offered}, but the repository is pinned to {pinned}; \
         if the author really changed keys, run `fox trust remove-author {git}` first"
    )]
    AuthorKeyChanged {
        git: GitUrl,
        tag: String,
        pinned: String,
        offered: String,
    },
    #[error(
        "{git} has no pinned author key and its tags are signed by {fingerprint}; \
         pass --trust-new-key or run `fox trust add-author` to trust it"
    )]
    UnknownAuthorKey { git: GitUrl, fingerprint: String },
    #[error("the key {fingerprint} for {git} was not accepted")]
    KeyRejected { git: GitUrl, fingerprint: String },
    #[error("cannot ask whether to trust the key")]
    Prompt(#[source] io::Error),
    #[error("tag {tag} names version {tag_version}, but the package's version is {version}")]
    VersionMismatch {
        tag: String,
        tag_version: String,
        version: String,
    },
}

impl From<BuildError> for SourceError {
    fn from(error: BuildError) -> Self {
        Self::Build(Box::new(error))
    }
}

impl From<ArchiveError> for SourceError {
    fn from(error: ArchiveError) -> Self {
        Self::Archive(Box::new(error))
    }
}

/// A `fox install` or `fox build` argument that names a git source.
#[derive(Debug, Clone)]
pub(crate) struct SourceSpec {
    /// The URL to fetch.
    fetch_url: String,
    /// Its normalized form: the source's identity.
    git: GitUrl,
    /// The tag or revision after `#`.
    rev: Option<String>,
}

impl Resolved {
    /// The source's normalized URL.
    pub(crate) fn git(&self) -> &GitUrl {
        &self.git
    }

    /// The tag or revision built.
    pub(crate) fn rev(&self) -> &str {
        &self.rev
    }

    /// The commit built.
    pub(crate) fn commit(&self) -> &fox_git::Commit {
        &self.commit
    }

    /// The fingerprint of the key that signed the tag, if it is SSH-signed.
    pub(crate) fn signer(&self) -> Option<String> {
        match self.tag.as_ref()?.signer() {
            TagSigner::Ssh(key) => Some(key.fingerprint()),
            _ => None,
        }
    }
}

impl SourceSpec {
    /// Parses `spec`: a git URL or an existing directory, optionally
    /// followed by `#<tag or commit>`.
    pub(crate) fn parse(spec: &str) -> Result<Self, SourceError> {
        let (location, rev) = match spec.rsplit_once('#') {
            Some((location, rev)) if !rev.is_empty() => (location, Some(rev.to_owned())),
            _ => (spec, None),
        };
        let path = Path::new(location);
        let fetch_url = match path.canonicalize() {
            Ok(dir) if dir.is_dir() => format!("file://{}", dir.display()),
            _ => location.to_owned(),
        };
        let git = GitUrl::new(&fetch_url).map_err(|source| SourceError::Spec {
            spec: spec.to_owned(),
            source,
        })?;
        Ok(Self {
            fetch_url,
            git,
            rev,
        })
    }
}

/// A fetched source at the commit chosen for it.
pub(crate) struct Resolved {
    git: GitUrl,
    repo: Repository,
    cache: PathBuf,
    rev: String,
    commit: fox_git::Commit,
    tag: Option<Tag>,
}

/// Fetches `spec` into the root's cache, picks its release tag (or the
/// requested tag or commit) and checks the tag's signature against the
/// trust store, pinning a new author key if the user accepts it.
pub(crate) fn resolve(
    root: &Path,
    spec: &SourceSpec,
    args: &SourceArgs,
) -> Result<Resolved, SourceError> {
    let digest = blake3::hash(spec.git.as_str().as_bytes()).to_hex();
    let cache = root.join(STATE_DIR).join(GIT_CACHE).join(&digest[..16]);
    let repo = fox_git::fetch(&spec.fetch_url, &cache)?;
    let (rev, tag) = match &spec.rev {
        Some(rev) => match repo.tag(rev) {
            Ok(tag) => (rev.clone(), Some(tag)),
            Err(GitError::Resolve { .. }) => (rev.clone(), None),
            Err(error) => return Err(error.into()),
        },
        None => match repo.newest_release_tag()? {
            Some(name) => {
                let tag = repo.tag(&name)?;
                (name, Some(tag))
            }
            None => ("HEAD".to_owned(), None),
        },
    };
    let commit = match &tag {
        Some(tag) => tag.commit().clone(),
        None => repo.resolve(&rev)?,
    };
    check_signature(root, &spec.git, &rev, tag.as_ref(), args)?;
    Ok(Resolved {
        git: spec.git.clone(),
        repo,
        cache,
        rev,
        commit,
        tag,
    })
}

fn check_signature(
    root: &Path,
    git: &GitUrl,
    rev: &str,
    tag: Option<&Tag>,
    args: &SourceArgs,
) -> Result<(), SourceError> {
    let unsigned = |what: String| {
        if args.allow_unsigned {
            eprintln!("fox: warning: building {what} of {git}, which is not signed");
            Ok(())
        } else {
            Err(SourceError::Unsigned {
                git: git.clone(),
                what,
            })
        }
    };
    let Some(tag) = tag else {
        return unsigned(format!("revision {rev}"));
    };
    let key = match tag.signer() {
        TagSigner::Ssh(key) => key,
        TagSigner::Unsupported(format) => {
            return unsigned(format!("tag {rev} (signed with {format:?}, not SSH)"));
        }
        _ => return unsigned(format!("tag {rev}")),
    };
    // The signature must hold for the key it names before that key is
    // compared with, or added to, the trust store.
    tag.verify(&key)?;
    let mut store = TrustStore::load(root)?;
    match store.check_author(git, &key) {
        AuthorTrust::Pinned => Ok(()),
        AuthorTrust::Changed { pinned } => Err(SourceError::AuthorKeyChanged {
            git: git.clone(),
            tag: rev.to_owned(),
            pinned: pinned.fingerprint(),
            offered: key.fingerprint(),
        }),
        _ => {
            accept_new_key(git, &key, args.trust_new_key)?;
            store.pin_author(git.clone(), key)?;
            store.save(root)?;
            Ok(())
        }
    }
}

/// Trust on first use: accepts `key` for `git` if `--trust-new-key` was
/// given or the user says yes at a terminal.
fn accept_new_key(git: &GitUrl, key: &AuthorKey, trust_new_key: bool) -> Result<(), SourceError> {
    let fingerprint = key.fingerprint();
    if trust_new_key {
        eprintln!("fox: pinning author key {fingerprint} for {git}");
        return Ok(());
    }
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Err(SourceError::UnknownAuthorKey {
            git: git.clone(),
            fingerprint,
        });
    }
    let mut stderr = io::stderr().lock();
    write!(
        stderr,
        "fox: {git} has no pinned author key.\n  Its release tags are signed by {fingerprint}.\n  \
         Check this fingerprint with the author, then pin it? [y/N] "
    )
    .and_then(|()| stderr.flush())
    .map_err(SourceError::Prompt)?;
    let mut answer = String::new();
    stdin
        .lock()
        .read_line(&mut answer)
        .map_err(SourceError::Prompt)?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES") {
        Ok(())
    } else {
        Err(SourceError::KeyRejected {
            git: git.clone(),
            fingerprint,
        })
    }
}

/// Builds a resolved source and packs it into `out` (default: `out` in the
/// work directory), signed by the root's local builder key (created on
/// first use).
pub(crate) fn build(
    root: &Path,
    resolved: &Resolved,
    args: &SourceArgs,
    out: Option<&Path>,
) -> Result<Packaged, SourceError> {
    let work_dir = args
        .work_dir
        .clone()
        .unwrap_or_else(|| default_work_dir(resolved.git.as_str().as_bytes()));
    let out = out.map_or_else(|| work_dir.join("out"), Path::to_path_buf);
    let mut request = BuildRequest::new(&resolved.cache, &work_dir);
    request.rev = resolved.commit.id();
    request.git_url = Some(resolved.git.to_string());
    request.package_dir = args.path.clone().unwrap_or_default();
    request.package = args.package.clone();
    request.target = args.target.clone();
    request.features = FeatureSelection {
        default_features: !args.no_default_features,
        features: args.features.clone(),
    };
    let signer = Signer::load_or_generate(
        &root.join(LOCAL_BUILDER_SECRET_KEY),
        &root.join(LOCAL_BUILDER_KEY),
        LOCAL_BUILDER_LABEL,
    )?;
    let built = fox_build::build(&request)?;
    check_version(resolved, &built.manifest.version)?;
    Ok(fox_build::package(&built, &signer, &out)?)
}

/// The tag's version, if it names one, must be the package's version.
fn check_version(resolved: &Resolved, version: &str) -> Result<(), SourceError> {
    let Some(tag_version) = resolved.tag.as_ref().and_then(Tag::version) else {
        return Ok(());
    };
    if tag_version.to_string() == version {
        Ok(())
    } else {
        Err(SourceError::VersionMismatch {
            tag: resolved.rev.clone(),
            tag_version: tag_version.to_string(),
            version: version.to_owned(),
        })
    }
}

/// Opens a prebuilt archive of `resolved` and checks that a trusted builder
/// attested to building it from the same repository, commit, tree and
/// package directory. `builders` holds the trust store's builders, and
/// `keys` the root's own signer keys, which are trusted as builders too.
pub(crate) fn prebuilt(
    root: &Path,
    resolved: &Resolved,
    args: &SourceArgs,
    path: &Path,
    keys: &TrustedKeys,
) -> Result<Archive, SourceError> {
    let mut trusted = keys.clone();
    for builder in TrustStore::load(root)?.builders() {
        trusted.insert(builder.label.clone(), builder.key);
    }
    let archive = Archive::open(path, &trusted)?;
    let verified = verify_archive_attestation(&archive, &attestation_path(path), &trusted)
        .map_err(|source| SourceError::PrebuiltAttestation {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    let source = &verified.attestation().source;
    let mismatch = |field| SourceError::PrebuiltMismatch {
        path: path.to_path_buf(),
        field,
    };
    if GitUrl::new(&source.git).ok().as_ref() != Some(&resolved.git) {
        return Err(mismatch("repository"));
    }
    if source.commit != resolved.commit.id() {
        return Err(mismatch("commit"));
    }
    if source.tree_blake3 != resolved.repo.tree_blake3(&resolved.commit)? {
        return Err(mismatch("tree"));
    }
    let wanted_path = args
        .path
        .as_ref()
        .map(|dir| dir.to_string_lossy().into_owned());
    if source.path != wanted_path {
        return Err(mismatch("package directory"));
    }
    check_version(resolved, &archive.manifest().version)?;
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_split_off_the_revision() {
        let spec = SourceSpec::parse("https://Example.com/a/hello.git#v1.0.0").unwrap();
        assert_eq!(spec.git.as_str(), "https://example.com/a/hello");
        assert_eq!(spec.fetch_url, "https://Example.com/a/hello.git");
        assert_eq!(spec.rev.as_deref(), Some("v1.0.0"));

        let dir = tempfile::tempdir().unwrap();
        let spec = SourceSpec::parse(dir.path().to_str().unwrap()).unwrap();
        assert!(spec.git.is_local());
        assert_eq!(spec.rev, None);

        assert!(matches!(
            SourceSpec::parse("no-such-thing"),
            Err(SourceError::Spec { .. })
        ));
    }
}
