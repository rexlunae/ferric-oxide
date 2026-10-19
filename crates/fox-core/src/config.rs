//! Small TOML configuration files under an install root: the trust store,
//! the install policy and the configured registries.
//!
//! [`load_toml`] reads a file of at most 1 MiB, treating a missing file as
//! the type's default, and [`save_toml`] replaces a file atomically (write
//! to `<file>.new`, sync, rename).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Largest configuration file read.
pub const MAX_CONFIG_SIZE: u64 = 1024 * 1024;

/// Why a configuration file could not be read or written.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file could not be read or written.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is larger than [`MAX_CONFIG_SIZE`].
    #[error("{} is larger than 1 MiB", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// The file is not valid.
    #[error("invalid {}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The contents could not be serialized.
    #[error("cannot serialize {}", path.display())]
    Serialize {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::ser::Error,
    },
}

/// Reads and parses `path`; a missing file gives `T::default()`.
///
/// # Errors
///
/// [`ConfigError::Io`], [`ConfigError::TooLarge`] or
/// [`ConfigError::Parse`] if the file exists but cannot be read or is
/// invalid.
pub fn load_toml<T: DeserializeOwned + Default>(path: &Path) -> Result<T, ConfigError> {
    let io_error = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    let size = match fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(io_error(error)),
    };
    if size > MAX_CONFIG_SIZE {
        return Err(ConfigError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let text = fs::read_to_string(path).map_err(io_error)?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// Serializes `value` to `path`, creating its directory and replacing the
/// file atomically.
///
/// # Errors
///
/// [`ConfigError::Serialize`] or [`ConfigError::Io`].
pub fn save_toml<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let text = toml::to_string(value).map_err(|source| ConfigError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic(path, text.as_bytes())
}

/// Writes `bytes` to `path` atomically, creating its directory.
///
/// # Errors
///
/// [`ConfigError::Io`] if the directory or file cannot be written.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let io_error = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_error)?;
    }
    replace_file(path, bytes).map_err(io_error)
}

/// Replaces `path` with `bytes` atomically: writes and syncs `<path>.new`
/// in the same directory, then renames it over `path`. A failure at any
/// step leaves the old file whole and removes the temporary file. The
/// directory must exist.
///
/// # Errors
///
/// The I/O error of the step that failed.
pub fn replace_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".new");
    let temp = PathBuf::from(temp);
    let result = fs::File::create(&temp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        // Best effort: the error that matters is the one returned.
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, serde::Deserialize, Serialize)]
    struct Sample {
        value: u32,
    }

    #[test]
    fn missing_files_are_defaults_and_saves_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("etc/sample.toml");
        assert_eq!(load_toml::<Sample>(&path).unwrap(), Sample::default());
        save_toml(&path, &Sample { value: 3 }).unwrap();
        assert_eq!(load_toml::<Sample>(&path).unwrap(), Sample { value: 3 });
        fs::write(&path, "value = \"x\"").unwrap();
        assert!(matches!(
            load_toml::<Sample>(&path),
            Err(ConfigError::Parse { .. })
        ));
    }
}
