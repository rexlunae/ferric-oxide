pub mod config;
pub mod cook;
pub mod recipe;
pub mod staged_pkg;
pub mod web;

/// Default for maximum number of levels to descend down dependencies tree.
pub const WALK_DEPTH: usize = 16;

pub fn is_redox() -> bool {
    cfg!(target_os = "redox")
}

pub fn cross_target() -> Option<String> {
    std::env::var("COOKBOOK_CROSS_TARGET")
        .ok()
        .filter(|s| !s.is_empty())
}

// Errors

use std::fmt::Display;
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

/// Error types used through cookbook.
///
/// When writing IO context, don't use "Failed at XXX". Look at impl Display for suitable word to use.
#[derive(Debug)]
pub enum Error {
    Io {
        source: io::Error,
        path: Option<PathBuf>,
        context: &'static str,
    },
    FileIo {
        source: io::Error,
        src: PathBuf,
        dst: PathBuf,
        context: &'static str,
    },
    Command(Command, ExitStatus),
    Package(PackageError),
    Pkgar(pkgar::Error),
    Options(String),
    Other(String),
    /// Ferric Oxide: a package or dependency name `fox` cannot use.
    FoxName {
        name: String,
        source: fox_core::NameError,
    },
    /// Ferric Oxide: a `fox` manifest could not be serialized.
    FoxManifest(fox_core::ManifestError),
    /// Ferric Oxide: a signing key could not be read or installed.
    FoxKey(fox_core::keys::KeyError),
    /// Ferric Oxide: a package archive failed fox's checks.
    FoxArchive(Box<fox_core::archive::ArchiveError>),
    /// Ferric Oxide: fox could not install a package into the sysroot.
    FoxStore(Box<fox_store::StoreError>),
}

/// Why a recipe or one of its packages could not be found or read.
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    /// A metadata or recipe file is not there.
    #[error("missing package file {}", .0.display())]
    FileMissing(PathBuf),
    /// A metadata or recipe file could not be read.
    #[error("cannot read package file {}", .path.display())]
    FileError {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A file is not valid TOML of the expected shape.
    #[error("cannot parse {}", .path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// Package metadata or a package name is invalid.
    #[error(transparent)]
    Metadata(#[from] fox_core::cookbook::CookbookError),
    /// No recipe has this name.
    #[error("package {0} not found")]
    PackageNotFound(fox_core::cookbook::RecipeName),
    /// Dependencies nest deeper than the walk allows; the names are the
    /// path down to where it stopped.
    #[error("recursion limit reached while processing dependencies; tree: {0:?}")]
    Recursion(std::collections::VecDeque<fox_core::cookbook::RecipeName>),
    /// One or more of the package's dependencies failed.
    #[error("package {0} is missing one or more dependencies")]
    DependencyInvalid(fox_core::cookbook::RecipeName),
}

impl PackageError {
    /// Adds `name` to the front of a [`PackageError::Recursion`] path, so
    /// the path can be built up while unwinding.
    pub fn append_recursion(&mut self, name: &fox_core::cookbook::RecipeName) {
        if let PackageError::Recursion(packages) = self {
            packages.push_front(name.clone());
        }
    }
}

impl Error {
    pub fn from_last_io_error(context: &'static str) -> Error {
        wrap_io_err!(context)(io::Error::last_os_error())
    }
    pub fn from_io_error(err: io::Error, context: &'static str) -> Error {
        wrap_io_err!(context)(err)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io {
                source,
                path,
                context,
            } => {
                if let Some(path) = path {
                    write!(f, "{context} failed at \"{}\": {}", path.display(), source)
                } else {
                    write!(f, "{context} failed: {}", source)
                }
            }
            Error::FileIo {
                source,
                src,
                dst,
                context,
            } => {
                write!(
                    f,
                    "{context} failed from \"{}\" to \"{}\": {}",
                    src.display(),
                    dst.display(),
                    source
                )
            }
            Error::Command(command, exit_status) => {
                write!(
                    f,
                    "Failed to run [{:?}]: exited with status {}",
                    command, exit_status
                )
            }
            Error::Package(package_error) => write!(f, "Package error: {}", package_error),
            Error::Pkgar(error) => write!(f, "Package archive error: {}", error),
            Error::Other(context) | Error::Options(context) => {
                write!(f, "{context}")
            }
            Error::FoxName { name, source } => {
                write!(f, "Package name {name:?} is not valid for fox: {source}")
            }
            Error::FoxManifest(error) => write!(f, "Cannot write fox manifest: {error}"),
            Error::FoxKey(error) => write!(f, "{error}"),
            Error::FoxArchive(error) => write!(f, "Package archive error: {error}"),
            Error::FoxStore(error) => write!(f, "Cannot install package: {error}"),
        }
    }
}

macro_rules! wrap_io_err {
    ($context:expr) => {
        |source| crate::Error::Io {
            source,
            path: None,
            context: $context,
        }
    };
    ($path:expr, $context:expr) => {
        |source| crate::Error::Io {
            source,
            path: Some($path.to_path_buf()),
            context: $context,
        }
    };
    ($src:expr, $dst:expr, $context:expr) => {
        |source| crate::Error::FileIo {
            source,
            src: $src.to_path_buf(),
            dst: $dst.to_path_buf(),
            context: $context,
        }
    };
}

macro_rules! wrap_other_err {
    ($($arg:tt)*) => {
        || crate::Error::Other(format!($($arg)*))
    };
}

macro_rules! bail_other_err {
    ($($arg:tt)*) => {
        return Err(crate::Error::Other(format!($($arg)*)))
    };
}

impl From<&'static str> for Error {
    fn from(value: &'static str) -> Self {
        Error::Other(value.to_string())
    }
}
impl From<String> for Error {
    fn from(value: String) -> Self {
        Error::Other(value)
    }
}

impl From<Error> for String {
    fn from(val: Error) -> Self {
        format!("{}", val)
    }
}

impl From<PackageError> for Error {
    fn from(value: PackageError) -> Self {
        Error::Package(value)
    }
}

impl From<fox_core::cookbook::CookbookError> for Error {
    fn from(value: fox_core::cookbook::CookbookError) -> Self {
        Error::Package(PackageError::Metadata(value))
    }
}

impl From<fox_core::keys::KeyError> for Error {
    fn from(value: fox_core::keys::KeyError) -> Self {
        Error::FoxKey(value)
    }
}

impl From<fox_core::archive::ArchiveError> for Error {
    fn from(value: fox_core::archive::ArchiveError) -> Self {
        Error::FoxArchive(Box::new(value))
    }
}

impl From<fox_store::StoreError> for Error {
    fn from(value: fox_store::StoreError) -> Self {
        Error::FoxStore(Box::new(value))
    }
}

impl From<pkgar::Error> for Error {
    fn from(value: pkgar::Error) -> Self {
        match value {
            pkgar::Error::Io {
                source,
                path,
                context,
            } => Error::Io {
                source,
                path,
                context,
            },
            _ => Error::Pkgar(value),
        }
    }
}

impl From<pkgar_keys::Error> for Error {
    fn from(value: pkgar_keys::Error) -> Self {
        Error::Pkgar(pkgar::Error::Keys(value))
    }
}

impl From<walkdir::Error> for Error {
    fn from(value: walkdir::Error) -> Self {
        if value.io_error().is_some() {
            let path = value.path().map(|s| s.to_path_buf());
            Error::Io {
                source: value.into_io_error().unwrap(),
                path,
                context: "Walkdir error",
            }
        } else {
            wrap_other_err!(
                "Walkdir file system loop found at {:?}",
                value.path().map(|s| s.to_string_lossy().to_string()),
            )()
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) use wrap_io_err;

pub(crate) use wrap_other_err;

pub(crate) use bail_other_err;

pub(crate) use cook::pty::log_to_pty;
