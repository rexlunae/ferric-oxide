//! File-system helpers that never follow symlinks out of the install root.

use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use fox_core::{Blake3Digest, FileKind};

use crate::StoreError;

/// What is at a path, read without following a final symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnDisk {
    /// Nothing.
    Missing,
    /// A directory.
    Directory,
    /// Something that is not a regular file, symlink or directory.
    Other,
    /// A file or symlink and the BLAKE3 of its contents (or link target).
    Hashed(FileKind, Blake3Digest),
}

/// Inspects `path` and hashes it if it is a file or symlink.
pub(crate) fn inspect(path: &Path) -> Result<OnDisk, StoreError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(OnDisk::Missing),
        Err(source) => return Err(StoreError::io(path, source)),
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(path).map_err(|e| StoreError::io(path, e))?;
        let digest = Blake3Digest::of(target.as_os_str().as_bytes());
        Ok(OnDisk::Hashed(FileKind::Symlink, digest))
    } else if file_type.is_file() {
        let mut file = File::open(path).map_err(|e| StoreError::io(path, e))?;
        let mut hasher = blake3::Hasher::new();
        io::copy(&mut file, &mut hasher).map_err(|e| StoreError::io(path, e))?;
        Ok(OnDisk::Hashed(FileKind::File, hasher.finalize().into()))
    } else if file_type.is_dir() {
        Ok(OnDisk::Directory)
    } else {
        Ok(OnDisk::Other)
    }
}

/// Returns the first ancestor of `root/relative` (below `root`) that is a
/// symlink, if any. Writing or removing through such a path could reach
/// outside the root.
pub(crate) fn symlinked_parent(root: &Path, relative: &str) -> Result<Option<PathBuf>, StoreError> {
    let mut current = root.to_path_buf();
    let mut components = relative.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(Some(current)),
            Ok(_) => {}
            // Missing directories are created later; nothing below exists.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::io(&current, source)),
        }
    }
    Ok(None)
}

/// Outcome of removing one installed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removal {
    /// The file matched its record and was removed.
    Removed,
    /// The file was already gone.
    Missing,
    /// The file differs from its record and was left in place.
    Kept,
}

/// Removes `root/relative` if it still has `kind` and `blake3`.
pub(crate) fn remove_if_unchanged(
    root: &Path,
    relative: &str,
    kind: FileKind,
    blake3: Blake3Digest,
) -> Result<Removal, StoreError> {
    if symlinked_parent(root, relative)?.is_some() {
        return Ok(Removal::Kept);
    }
    let path = root.join(relative);
    match inspect(&path)? {
        OnDisk::Missing => Ok(Removal::Missing),
        OnDisk::Hashed(found_kind, found) if found_kind == kind && found == blake3 => {
            fs::remove_file(&path).map_err(|e| StoreError::io(&path, e))?;
            Ok(Removal::Removed)
        }
        _ => Ok(Removal::Kept),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn finds_symlinked_parents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        symlink("/etc", root.join("usr/lib")).unwrap();
        assert_eq!(symlinked_parent(root, "usr/bin/x").unwrap(), None);
        assert_eq!(
            symlinked_parent(root, "usr/lib/x").unwrap(),
            Some(root.join("usr/lib"))
        );
        assert_eq!(symlinked_parent(root, "opt/new/x").unwrap(), None);
        // The final component may itself be a symlink; that is the entry.
        assert_eq!(symlinked_parent(root, "usr/lib").unwrap(), None);
    }

    #[test]
    fn removes_only_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a"), b"one").unwrap();
        fs::write(root.join("b"), b"changed").unwrap();
        let one = Blake3Digest::of(b"one");
        assert_eq!(
            remove_if_unchanged(root, "a", FileKind::File, one).unwrap(),
            Removal::Removed
        );
        assert_eq!(
            remove_if_unchanged(root, "a", FileKind::File, one).unwrap(),
            Removal::Missing
        );
        assert_eq!(
            remove_if_unchanged(root, "b", FileKind::File, one).unwrap(),
            Removal::Kept
        );
        assert!(root.join("b").exists());
    }
}
