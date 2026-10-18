//! The staging tree a package is packed from.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fox_core::{Blake3Digest, FileKind, FileRecord, check_entry_path};

use crate::BuildError;

/// A directory laid out like the install root, holding only what the
/// package installs.
#[derive(Debug)]
pub(crate) struct Stage {
    dir: PathBuf,
    paths: BTreeSet<String>,
}

impl Stage {
    /// Creates an empty stage at `dir`, removing anything there.
    pub(crate) fn create(dir: &Path) -> Result<Self, BuildError> {
        remove_dir_if_present(dir)?;
        fs::create_dir_all(dir).map_err(io_error(dir))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            paths: BTreeSet::new(),
        })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Copies the regular file `source` to `install_path` (absolute or
    /// relative to the root) with `mode`.
    pub(crate) fn copy(
        &mut self,
        install_path: &str,
        source: &Path,
        mode: u32,
    ) -> Result<(), BuildError> {
        // A symlink in the source tree could point anywhere on the build
        // host; only regular files are packed.
        let metadata = fs::symlink_metadata(source).map_err(io_error(source))?;
        if !metadata.file_type().is_file() {
            return Err(BuildError::NotAFile {
                path: source.to_path_buf(),
            });
        }
        let target = self.reserve(install_path)?;
        fs::copy(source, &target).map_err(io_error(&target))?;
        set_mode(&target, mode)
    }

    /// Writes `contents` to `install_path` with `mode`.
    pub(crate) fn write(
        &mut self,
        install_path: &str,
        contents: &[u8],
        mode: u32,
    ) -> Result<(), BuildError> {
        let target = self.reserve(install_path)?;
        fs::write(&target, contents).map_err(io_error(&target))?;
        set_mode(&target, mode)
    }

    /// Checks `install_path`, claims it and creates its parent directory.
    fn reserve(&mut self, install_path: &str) -> Result<PathBuf, BuildError> {
        let relative = install_path.trim_start_matches('/');
        let checked =
            check_entry_path(relative.as_bytes()).map_err(|source| BuildError::InstallPath {
                path: install_path.to_owned(),
                source,
            })?;
        // Two files at one path, or a file where another needs a directory,
        // would make the second silently replace or break the first.
        let clashes = self
            .paths
            .iter()
            .any(|p| p == checked || is_under(checked, p) || is_under(p, checked));
        if clashes || !self.paths.insert(checked.to_owned()) {
            return Err(BuildError::DuplicateInstallPath {
                path: checked.to_owned(),
            });
        }
        let target = self.dir.join(checked);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        Ok(target)
    }

    /// The records of every staged file, in path order.
    pub(crate) fn records(&self) -> Result<Vec<FileRecord>, BuildError> {
        self.paths
            .iter()
            .map(|path| {
                let full = self.dir.join(path);
                let contents = fs::read(&full).map_err(io_error(&full))?;
                let mode = fs::metadata(&full)
                    .map_err(io_error(&full))?
                    .permissions()
                    .mode();
                Ok(FileRecord {
                    path: path.clone(),
                    kind: FileKind::File,
                    mode: mode & 0o7777,
                    size: contents.len() as u64,
                    blake3: Blake3Digest::of(&contents),
                })
            })
            .collect()
    }
}

/// Whether `path` lies below `dir`.
fn is_under(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), BuildError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_error(path))
}

pub(crate) fn remove_dir_if_present(dir: &Path) -> Result<(), BuildError> {
    match fs::remove_dir_all(dir) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_error(dir)(error)),
        _ => Ok(()),
    }
}

pub(crate) fn io_error(path: &Path) -> impl FnOnce(io::Error) -> BuildError + '_ {
    move |source| BuildError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_files_with_modes_and_refuses_clashes() {
        let dir = tempfile::tempdir().unwrap();
        let mut stage = Stage::create(&dir.path().join("stage")).unwrap();
        stage.write("/usr/bin/hello", b"hi", 0o755).unwrap();
        stage.write("usr/share/doc/hello", b"doc", 0o644).unwrap();
        let records = stage.records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path, "usr/bin/hello");
        assert_eq!(records[0].mode, 0o755);
        assert_eq!(records[0].blake3, Blake3Digest::of(b"hi"));

        for clash in [
            "usr/bin/hello",
            "/usr/bin/hello",
            "usr/bin/hello/x",
            "usr/share/doc",
        ] {
            assert!(
                matches!(
                    stage.write(clash, b"", 0o644),
                    Err(BuildError::DuplicateInstallPath { .. })
                ),
                "{clash}"
            );
        }
        for bad in ["", "/", "usr/../etc/passwd", "usr//bin"] {
            assert!(
                matches!(
                    stage.write(bad, b"", 0o644),
                    Err(BuildError::InstallPath { .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn copies_only_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("link")).unwrap();
        let mut stage = Stage::create(&dir.path().join("stage")).unwrap();
        stage
            .copy("usr/a", &dir.path().join("real"), 0o644)
            .unwrap();
        assert!(matches!(
            stage.copy("usr/b", &dir.path().join("link"), 0o644),
            Err(BuildError::NotAFile { .. })
        ));
    }
}
