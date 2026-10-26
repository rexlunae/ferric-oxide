//! Loader failures and their attempt-local diagnostics.
//!
//! Search and open context belongs to the returned error, never to mutable linker state.
//! A later parse, relocation or TLS failure therefore cannot report an earlier I/O failure.

use alloc::{string::String, vec::Vec};
use core::fmt;

use crate::error::Errno;

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
    /// Opening or mapping the selected object failed.
    Open {
        /// Selected object's path.
        path: String,
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

impl fmt::Display for DlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Search {
                access_error: Some((path, source)),
                ..
            } => write!(f, "access '{path}' failed: {source} (errno {})", source.0),
            Self::Search { name, paths, .. } => write!(f, "'{name}' is in none of {paths:?}"),
            Self::Open { path, source } => {
                write!(f, "opening '{path}' failed: {source} (errno {})", source.0)
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
            source: Errno(5),
        };
        assert!(format!("{opened}").contains("opening '/lib/open.so' failed:"));
        assert!(format!("{opened}").ends_with("(errno 5)"));
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
}
