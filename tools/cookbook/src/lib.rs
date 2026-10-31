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

/// Whether a recipe's build tools (`build.dev-dependencies`, the `host:`
/// recipes) are built for the target and run in a guest instead of being
/// compiled for, and run on, the host.
///
/// The build sets `FERRIC_BUILD_TOOLS=guest` from `features.toml`
/// `build.build_tools`. Only a native build can do this: a tool built for a
/// foreign architecture cannot run in a guest on this host, so the build
/// resolves the option to `host` there and this stays off.
///
/// It changes three things together, because they have to agree: the tool
/// builds for the target rather than the host ([`cook::package`]), a `host:`
/// recipe's dependencies are mapped for a target build — unprefixed ones stay
/// target packages and a `target:` pin keeps its package without the pin
/// ([`recipe::CookRecipe::new`]) — and the recipe builds against the target
/// sysroot with no host toolchain ([`cook::cook_build`]).
pub fn build_tools_in_guest() -> bool {
    build_tools_in_guest_for(std::env::var("FERRIC_BUILD_TOOLS").ok().as_deref())
}

/// Whether a build tool that runs `runs_on` is built for the target and run in
/// the guest: when [`build_tools_in_guest`], unless its recipe keeps it on the
/// host ([`recipe::ToolRunsOn::Host`]).
pub fn tool_runs_in_guest(runs_on: recipe::ToolRunsOn) -> bool {
    tool_runs_in_guest_for(build_tools_in_guest(), runs_on)
}

/// The decision in [`tool_runs_in_guest`], without the process environment.
fn tool_runs_in_guest_for(guest: bool, runs_on: recipe::ToolRunsOn) -> bool {
    guest && runs_on == recipe::ToolRunsOn::Configured
}

/// The decision in [`build_tools_in_guest`], on the value itself, so it can be
/// tested without the process environment.
///
/// The dependency mapping in [`recipe`] takes the result as a parameter for the
/// same reason: it has to reach both modes in a test.
fn build_tools_in_guest_for(value: Option<&str>) -> bool {
    value == Some("guest")
}

/// Where the guest build environment's image is, when tools run in the guest.
///
/// `mk/fstools.mk` builds it once per configuration and passes its path as
/// `FERRIC_GUEST_TOOLS_DIR`; recipe scripts dispatch tool commands into it
/// instead of running the Redox binaries directly.
pub fn guest_tools_dir() -> Option<String> {
    if !build_tools_in_guest() {
        return None;
    }
    std::env::var("FERRIC_GUEST_TOOLS_DIR")
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
    /// Ferric Oxide: recipes to build need programs the build machine lacks.
    MissingHostTools(cook::host_tools::MissingHostTools),
    /// Ferric Oxide: a build tool could not be run in the guest build environment.
    GuestRun(Box<redoxer::guest::GuestError>),
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
            Error::MissingHostTools(error) => write!(f, "{error}"),
            Error::GuestRun(error) => {
                write!(f, "Cannot run the build tool in the guest: {error}")?;
                let mut source = std::error::Error::source(error.as_ref());
                while let Some(cause) = source {
                    write!(f, ": {cause}")?;
                    source = cause.source();
                }
                Ok(())
            }
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

impl From<cook::host_tools::MissingHostTools> for Error {
    fn from(value: cook::host_tools::MissingHostTools) -> Self {
        Error::MissingHostTools(value)
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

#[cfg(test)]
mod tests {
    use super::{build_tools_in_guest_for, tool_runs_in_guest_for};
    use crate::recipe::ToolRunsOn;

    /// Only the `guest` value moves a recipe's build tools to the target.
    /// An unset or empty setting keeps the inherited host behaviour, which is
    /// what a build without `features.toml` must do.
    #[test]
    fn only_the_guest_value_builds_tools_for_the_target() {
        assert!(build_tools_in_guest_for(Some("guest")));
        assert!(!build_tools_in_guest_for(Some("host")));
        assert!(!build_tools_in_guest_for(Some("")));
        assert!(!build_tools_in_guest_for(None));
    }

    /// A tool that must run on the host stays there in guest mode; every other
    /// tool follows the setting.
    #[test]
    fn a_host_only_tool_never_runs_in_the_guest() {
        assert!(tool_runs_in_guest_for(true, ToolRunsOn::Configured));
        assert!(!tool_runs_in_guest_for(true, ToolRunsOn::Host));
        assert!(!tool_runs_in_guest_for(false, ToolRunsOn::Configured));
        assert!(!tool_runs_in_guest_for(false, ToolRunsOn::Host));
    }

    /// The field is optional and spelled as recipes write it.
    #[test]
    fn tool_runs_on_reads_from_a_recipe() {
        let recipe: crate::recipe::Recipe =
            toml::from_str("[build]\ntemplate = \"none\"\ntool-runs-on = \"host\"\n").unwrap();
        assert_eq!(recipe.build.tool_runs_on, ToolRunsOn::Host);
        let recipe: crate::recipe::Recipe =
            toml::from_str("[build]\ntemplate = \"none\"\n").unwrap();
        assert_eq!(recipe.build.tool_runs_on, ToolRunsOn::Configured);
    }
}
