//! Git sources for `fox`: resolving commits, hashing their trees with
//! BLAKE3, and exporting them for hermetic builds.
//!
//! A fox package release is a git commit. Git names objects by SHA-1, which
//! is not trusted on its own, so every source is also identified by a
//! BLAKE3 hash of the commit's contents in a format defined here,
//! `fox-tree/1`. The same hash recognizes mirrors of a repository on other
//! hosts as the same source, and it is what attestations record as
//! `tree_blake3`.
//!
//! Repositories are read with [`gix`](https://crates.io/crates/gix), the
//! pure-Rust git implementation, without its network transports: this crate
//! works on local repositories only.
//!
//! # The `fox-tree/1` hash
//!
//! The hash covers every non-tree entry reachable from the commit's root
//! tree (git stores no empty directories). Entries are taken in ascending
//! byte order of their full path (components joined with `/`), not in git's
//! tree order. The hashed bytes are the 11 bytes `fox-tree/1\0`, then for
//! each entry:
//!
//! | Field | Bytes |
//! | --- | --- |
//! | kind | one byte: `f` file (`100644`, and the historical `100664`), `x` executable file (`100755`), `l` symlink (`120000`), `c` submodule commit (`160000`) |
//! | path length | 8 bytes, little-endian |
//! | path | the path bytes |
//! | content length | 8 bytes, little-endian |
//! | content | the blob contents (for a symlink, its target; for a submodule, the raw bytes of the commit id it pins) |
//!
//! Every field has an explicit length, so two different trees cannot encode
//! to the same bytes. Modes other than the four above do not occur in git
//! trees and are rejected.
//!
//! # Export
//!
//! [`Repository::export`] writes a commit's tree into an empty directory and
//! returns its `fox-tree/1` hash, computed from the same object reads. Files
//! get mode `0644` or `0755`, symlinks are recreated, and submodules become
//! empty directories, as in a clone without `--recurse-submodules`. Path
//! components that are empty, `.`, `..`, `.git` (in any case) or contain
//! `/`, `\` or NUL are refused, so a hostile tree cannot write outside the
//! directory or plant git metadata in it.
//!
//! # Examples
//!
//! ```no_run
//! use fox_git::Repository;
//! use std::path::Path;
//!
//! let repo = Repository::discover(Path::new("."))?;
//! let commit = repo.resolve("HEAD")?;
//! let export = repo.export(&commit, Path::new("/tmp/fox-src"))?;
//! println!("{} tree_blake3 {}", commit.id(), export.tree_blake3);
//! # Ok::<(), fox_git::GitError>(())
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use fox_core::Blake3Digest;
use gix::ObjectId;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::object::tree::EntryKind;

/// Bytes hashed before the first entry.
const TREE_CONTEXT: &[u8] = b"fox-tree/1\0";

/// Why a repository operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitError {
    /// No repository was found at or above the path.
    #[error("no git repository at or above {}", path.display())]
    Discover {
        /// Where the search started.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: Box<gix::Error>,
    },
    /// The repository could not be opened.
    #[error("cannot open git repository {}", path.display())]
    Open {
        /// The repository path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: Box<gix::Error>,
    },
    /// A revision did not resolve to a commit.
    #[error("cannot resolve {rev:?} to a commit")]
    Resolve {
        /// The revision.
        rev: String,
        /// The underlying error.
        #[source]
        source: Box<gix::Error>,
    },
    /// An object could not be read or decoded.
    #[error("cannot read git object {id}")]
    Object {
        /// The object id.
        id: ObjectId,
        /// The underlying error.
        #[source]
        source: Box<gix::Error>,
    },
    /// A tree entry has a path component that is unsafe to export.
    #[error("tree entry {path:?} has an unsafe path component")]
    UnsafePath {
        /// The entry path, lossily decoded.
        path: String,
    },
    /// The export directory exists and is not empty.
    #[error("export directory {} is not empty", path.display())]
    ExportNotEmpty {
        /// The directory.
        path: PathBuf,
    },
    /// Writing the exported tree failed.
    #[error("cannot write {}", path.display())]
    Export {
        /// The path being written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// A local git repository.
#[derive(Debug)]
pub struct Repository {
    inner: gix::Repository,
}

/// A resolved commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    id: ObjectId,
    tree: ObjectId,
    time: i64,
}

impl Commit {
    /// The full hex commit id.
    pub fn id(&self) -> String {
        self.id.to_string()
    }

    /// Committer time, in seconds since the Unix epoch. Builds use it as
    /// `SOURCE_DATE_EPOCH`.
    pub fn committer_time(&self) -> i64 {
        self.time
    }
}

/// The result of [`Repository::export`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeExport {
    /// The `fox-tree/1` hash of the exported tree.
    pub tree_blake3: Blake3Digest,
    /// Number of entries written (files, symlinks and submodule
    /// directories).
    pub entries: usize,
}

/// The kind of a non-tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Executable,
    Symlink,
    Submodule,
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Self::File => b'f',
            Self::Executable => b'x',
            Self::Symlink => b'l',
            Self::Submodule => b'c',
        }
    }
}

/// One entry of a flattened tree.
struct Entry {
    path: BString,
    kind: Kind,
    id: ObjectId,
}

impl Repository {
    /// Finds the repository containing `path`, searching upwards.
    ///
    /// # Errors
    ///
    /// [`GitError::Discover`] if there is none.
    pub fn discover(path: &Path) -> Result<Self, GitError> {
        gix::discover(path)
            .map(|inner| Self { inner })
            .map_err(|source| GitError::Discover {
                path: path.to_path_buf(),
                source: Box::new(source),
            })
    }

    /// Opens the repository at `path` (a work tree or a bare repository).
    ///
    /// # Errors
    ///
    /// [`GitError::Open`] if it is not a repository.
    pub fn open(path: &Path) -> Result<Self, GitError> {
        gix::open(path)
            .map(|inner| Self { inner })
            .map_err(|source| GitError::Open {
                path: path.to_path_buf(),
                source: Box::new(source),
            })
    }

    /// The work tree, unless the repository is bare.
    pub fn workdir(&self) -> Option<&Path> {
        self.inner.workdir()
    }

    /// The URL of the `origin` remote, if there is one.
    pub fn origin_url(&self) -> Option<String> {
        let remote = self.inner.find_remote("origin").ok()?;
        let url = remote.url(gix::remote::Direction::Fetch)?;
        Some(url.to_bstring().to_str_lossy().into_owned())
    }

    /// Resolves `rev` (a branch, tag, commit id or any revision git
    /// accepts) to a commit, peeling annotated tags.
    ///
    /// # Errors
    ///
    /// [`GitError::Resolve`] if it names nothing or something that is not
    /// a commit.
    pub fn resolve(&self, rev: &str) -> Result<Commit, GitError> {
        let resolve_error = |source| GitError::Resolve {
            rev: rev.to_owned(),
            source: Box::new(source),
        };
        let commit = self
            .inner
            .rev_parse_single(rev)
            .and_then(|id| id.object())
            .and_then(|object| object.peel_to_commit())
            .map_err(resolve_error)?;
        let tree = commit.tree_id().map_err(resolve_error)?.detach();
        let time = commit.time().map_err(resolve_error)?.seconds;
        Ok(Commit {
            id: commit.id,
            tree,
            time,
        })
    }

    /// The `fox-tree/1` hash of `commit`'s tree.
    ///
    /// # Errors
    ///
    /// [`GitError::Object`] if an object cannot be read, and
    /// [`GitError::UnsafePath`] for path components [`Repository::export`]
    /// would refuse, so a tree that hashes can also be exported.
    pub fn tree_blake3(&self, commit: &Commit) -> Result<Blake3Digest, GitError> {
        let entries = self.flatten(commit.tree)?;
        let mut hasher = TreeHasher::new();
        for entry in &entries {
            let content = self.content(entry)?;
            hasher.add(entry, &content);
        }
        Ok(hasher.finish())
    }

    /// Writes `commit`'s tree into `dest`, which must be missing or an
    /// empty directory, and returns its `fox-tree/1` hash.
    ///
    /// # Errors
    ///
    /// [`GitError::ExportNotEmpty`] if `dest` has contents,
    /// [`GitError::UnsafePath`] for an unsafe path component,
    /// [`GitError::Object`] if an object cannot be read and
    /// [`GitError::Export`] if writing fails.
    pub fn export(&self, commit: &Commit, dest: &Path) -> Result<TreeExport, GitError> {
        prepare_dest(dest)?;
        let entries = self.flatten(commit.tree)?;
        let mut hasher = TreeHasher::new();
        for entry in &entries {
            let content = self.content(entry)?;
            hasher.add(entry, &content);
            write_entry(dest, entry, &content)?;
        }
        Ok(TreeExport {
            tree_blake3: hasher.finish(),
            entries: entries.len(),
        })
    }

    /// Lists every non-tree entry under `tree`, sorted by full path.
    fn flatten(&self, tree: ObjectId) -> Result<Vec<Entry>, GitError> {
        let mut entries = Vec::new();
        let mut pending = vec![(BString::default(), tree)];
        while let Some((prefix, id)) = pending.pop() {
            let object = self.find(id)?;
            let tree = object.into_tree();
            let decoded = tree.decode().map_err(|source| GitError::Object {
                id,
                source: Box::new(source),
            })?;
            for item in &decoded.entries {
                let path = join(prefix.as_bstr(), item.filename);
                if !is_safe_component(item.filename) {
                    return Err(GitError::UnsafePath {
                        path: path.to_str_lossy().into_owned(),
                    });
                }
                let id = item.oid.to_owned();
                let kind = match item.mode.kind() {
                    EntryKind::Tree => {
                        pending.push((path, id));
                        continue;
                    }
                    EntryKind::Blob => Kind::File,
                    EntryKind::BlobExecutable => Kind::Executable,
                    EntryKind::Link => Kind::Symlink,
                    EntryKind::Commit => Kind::Submodule,
                };
                entries.push(Entry { path, kind, id });
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// The bytes an entry contributes to the hash and the export.
    fn content(&self, entry: &Entry) -> Result<Vec<u8>, GitError> {
        match entry.kind {
            Kind::Submodule => Ok(entry.id.as_bytes().to_vec()),
            _ => Ok(self.find(entry.id)?.detach().data),
        }
    }

    fn find(&self, id: ObjectId) -> Result<gix::Object<'_>, GitError> {
        self.inner
            .find_object(id)
            .map_err(|source| GitError::Object {
                id,
                source: Box::new(source),
            })
    }
}

/// Streams entries into the `fox-tree/1` hash.
struct TreeHasher(blake3::Hasher);

impl TreeHasher {
    fn new() -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(TREE_CONTEXT);
        Self(hasher)
    }

    fn add(&mut self, entry: &Entry, content: &[u8]) {
        self.0.update(&[entry.kind.tag()]);
        self.0.update(&(entry.path.len() as u64).to_le_bytes());
        self.0.update(&entry.path);
        self.0.update(&(content.len() as u64).to_le_bytes());
        self.0.update(content);
    }

    fn finish(self) -> Blake3Digest {
        self.0.finalize().into()
    }
}

fn join(prefix: &BStr, name: &BStr) -> BString {
    let mut path = BString::from(prefix);
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// Whether a tree entry name is safe to create as one path component.
fn is_safe_component(name: &BStr) -> bool {
    !(name.is_empty()
        || name == "."
        || name == ".."
        || name.eq_ignore_ascii_case(b".git")
        || name.iter().any(|&b| matches!(b, b'/' | b'\\' | 0)))
}

fn prepare_dest(dest: &Path) -> Result<(), GitError> {
    let export_error = |source| GitError::Export {
        path: dest.to_path_buf(),
        source,
    };
    match fs::read_dir(dest) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(GitError::ExportNotEmpty {
                    path: dest.to_path_buf(),
                });
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(dest).map_err(export_error)
        }
        Err(error) => Err(export_error(error)),
    }
}

fn write_entry(dest: &Path, entry: &Entry, content: &[u8]) -> Result<(), GitError> {
    let path = dest.join(std::ffi::OsStr::from_bytes(&entry.path));
    let export_error = |source| GitError::Export {
        path: path.clone(),
        source,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(export_error)?;
    }
    match entry.kind {
        Kind::File | Kind::Executable => {
            fs::write(&path, content).map_err(export_error)?;
            let mode = if entry.kind == Kind::Executable {
                0o755
            } else {
                0o644
            };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).map_err(export_error)
        }
        Kind::Symlink => symlink(std::ffi::OsStr::from_bytes(content), &path).map_err(export_error),
        Kind::Submodule => fs::create_dir(&path).map_err(export_error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_components() {
        for bad in ["", ".", "..", ".git", ".GIT", "a/b", "a\\b", "a\0b"] {
            assert!(!is_safe_component(bad.into()), "{bad:?}");
        }
        for good in ["a", ".github", "..a", "git", ".gitignore"] {
            assert!(is_safe_component(good.into()), "{good:?}");
        }
    }

    #[test]
    fn hash_encoding_is_length_prefixed() {
        let entry = |path: &str| Entry {
            path: path.into(),
            kind: Kind::File,
            id: ObjectId::null(gix::hash::Kind::Sha1),
        };
        let mut a = TreeHasher::new();
        a.add(&entry("ab"), b"c");
        let mut b = TreeHasher::new();
        b.add(&entry("a"), b"bc");
        assert_ne!(a.finish(), b.finish());

        // An empty tree hashes to the context alone.
        assert_eq!(TreeHasher::new().finish(), Blake3Digest::of(TREE_CONTEXT));
    }
}
