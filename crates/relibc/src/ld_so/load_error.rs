//! Loader failures and their attempt-local diagnostics.
//!
//! Search and open context belongs to the returned error, never to mutable linker state.
//! A later parse, relocation or TLS failure therefore cannot report an earlier I/O failure.

use alloc::{string::String, vec::Vec};
use core::fmt;

use crate::error::Errno;

/// Operation that failed while obtaining a dynamic object's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectIoOperation {
    /// Opening the selected path.
    Open,
    /// Reading or validating the opened file's size.
    Stat,
    /// Mapping the opened file read-only.
    Map {
        /// Requested mapping length in bytes, before page rounding.
        size: usize,
    },
}

/// Operation and original errno, before the selected path is attached.
#[derive(Debug)]
pub(super) struct ObjectIoError {
    pub(super) operation: ObjectIoOperation,
    pub(super) source: Errno,
}

/// A failure to load or use a dynamic shared object.
#[derive(Debug)]
pub enum DlError {
    /// Failed to locate the requested DSO without a file-system search.
    NotFound,
    /// Every candidate in an object search failed.
    Search {
        /// Requested library or executable.
        name: String,
        /// Directories searched, in order.
        paths: Vec<String>,
        /// Last candidate that failed for a reason other than absence, if any.
        access_error: Option<(String, Errno)>,
    },
    /// Opening, inspecting or mapping the selected object failed.
    Open {
        /// Selected object's path.
        path: String,
        /// Exact I/O operation that failed.
        operation: ObjectIoOperation,
        /// Original platform error.
        source: Errno,
    },
    /// The DSO is malformed, or its TLS setup failed.
    Malformed,
    /// Invalid DSO handle.
    InvalidHandle,
    /// Out of memory.
    Oom,
}

impl DlError {
    /// The complete fatal diagnostic printed by loader startup.
    pub(super) fn fatal<'a>(&'a self, path: &'a str) -> FatalDiagnostic<'a> {
        FatalDiagnostic { path, error: self }
    }

    /// Returns the stable, null-terminated message used by `dlerror`.
    pub const fn repr(&self) -> &'static core::ffi::CStr {
        match self {
            Self::NotFound | Self::Search { .. } | Self::Open { .. } => {
                c"Failed to locate the requested DSO. Set `LD_DEBUG=all` for more information."
            }
            Self::Malformed => {
                c"The DSO is malformed somehow. Set `LD_DEBUG=all` for more information."
            }
            Self::InvalidHandle => c"Invalid DSO handle. Set `LD_DEBUG=all` for more information.",
            Self::Oom => c"Out of memory.",
        }
    }
}

/// Display adapter shared by loader startup and host regression tests.
pub(super) struct FatalDiagnostic<'a> {
    path: &'a str,
    error: &'a DlError,
}

impl fmt::Display for FatalDiagnostic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "[ld.so]: failed to link '{}': {:?}",
            self.path, self.error
        )?;
        if matches!(self.error, DlError::Search { .. } | DlError::Open { .. }) {
            writeln!(f, "[ld.so]: {}", self.error)?;
        }
        f.write_str("[ld.so]: enable debug output with `LD_DEBUG=all` for more information")
    }
}

impl fmt::Display for DlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Search {
                access_error: Some((path, source)),
                ..
            } => write!(f, "access '{path}' failed: {source} (errno {})", source.0),
            Self::Search { name, paths, .. } => write!(f, "'{name}' is in none of {paths:?}"),
            Self::Open {
                path,
                operation,
                source,
            } => {
                match operation {
                    ObjectIoOperation::Open => write!(f, "opening '{path}' failed"),
                    ObjectIoOperation::Stat => write!(f, "reading size of '{path}' failed"),
                    ObjectIoOperation::Map { size } => {
                        write!(f, "mapping '{path}' ({size} bytes) failed")
                    }
                }?;
                write!(f, ": {source} (errno {})", source.0)
            }
            _ => f.write_str(self.repr().to_str().expect("loader messages are ASCII")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    #[test]
    fn later_failures_do_not_inherit_search_context() {
        let missing = DlError::Search {
            name: "missing.so".into(),
            paths: vec!["/lib".into()],
            access_error: None,
        };
        assert_eq!(
            format!("{missing}"),
            "'missing.so' is in none of [\"/lib\"]"
        );
        // A subsequent parse or TLS failure carries only its own diagnostic,
        // even while the previous search error is still held by the caller.
        let malformed = DlError::Malformed;
        assert_eq!(format!("{malformed}"), malformed.repr().to_str().unwrap());
        assert!(!format!("{malformed}").contains("missing.so"));
        assert_eq!(
            format!("{}", malformed.fatal("/bin/ion")),
            "[ld.so]: failed to link '/bin/ion': Malformed\n[ld.so]: enable debug output with `LD_DEBUG=all` for more information"
        );
        assert_ne!(missing.repr(), malformed.repr());
    }

    #[test]
    fn io_failures_keep_the_path_and_original_errno() {
        let access = DlError::Search {
            name: "blocked.so".into(),
            paths: vec!["/lib".into()],
            access_error: Some(("/lib/blocked.so".into(), Errno(13))),
        };
        assert!(format!("{access}").contains("access '/lib/blocked.so' failed:"));
        assert!(format!("{access}").ends_with("(errno 13)"));
        let opened = DlError::Open {
            path: "/lib/open.so".into(),
            operation: ObjectIoOperation::Open,
            source: Errno(5),
        };
        assert!(format!("{opened}").contains("opening '/lib/open.so' failed:"));
        assert!(format!("{opened}").ends_with("(errno 5)"));
        let fatal = format!("{}", opened.fatal("/bin/ion"));
        assert!(fatal.starts_with("[ld.so]: failed to link '/bin/ion': Open"));
        assert!(fatal.contains("\n[ld.so]: opening '/lib/open.so' failed:"));
        assert!(fatal.contains("(errno 5)\n[ld.so]: enable debug output"));
        assert!(matches!(
            opened,
            DlError::Open {
                source: Errno(5),
                ..
            }
        ));
        assert_eq!(access.repr(), DlError::NotFound.repr());
        assert_eq!(
            DlError::InvalidHandle.repr(),
            c"Invalid DSO handle. Set `LD_DEBUG=all` for more information."
        );
        assert_eq!(DlError::Oom.repr(), c"Out of memory.");
    }

    #[test]
    fn stat_and_mapping_failures_name_the_actual_operation() {
        for (operation, expected) in [
            (
                ObjectIoOperation::Stat,
                "reading size of '/lib/test.so' failed",
            ),
            (
                ObjectIoOperation::Map { size: 12345 },
                "mapping '/lib/test.so' (12345 bytes) failed",
            ),
        ] {
            let error = DlError::Open {
                path: "/lib/test.so".into(),
                operation,
                source: Errno(12),
            };
            let fatal = format!("{}", error.fatal("/bin/test"));
            assert!(fatal.contains(expected), "{fatal}");
            assert!(fatal.contains("(errno 12)"));
        }
    }
}
