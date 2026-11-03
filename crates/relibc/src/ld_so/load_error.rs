//! Loader failures and their attempt-local diagnostics.
//!
//! Search and open context belongs to the returned error, never to mutable linker state.
//! A later parse, relocation or TLS failure therefore cannot report an earlier I/O failure.

use alloc::{string::String, vec::Vec};
use core::{ffi::CStr, fmt, ops::Range};

use crate::error::Errno;

/// Operation that failed while obtaining a dynamic object's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectIoOperation {
    /// Opening the selected path.
    Open,
    /// Reading or validating the opened file's size.
    Stat,
    /// Reading the start of the opened file, to decide how much of it to map.
    Read,
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

/// Why an object's bytes could not be turned into a loaded image.
///
/// Returned by `DSO::new` and carried by [`DlError::Load`] with the object's path.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadError {
    /// The ELF file and program headers could not be parsed.
    Parse(object::Error),
    /// The object has no `PT_LOAD` segment, so there is nothing to map.
    NoLoadSegment,
    /// Mapping anonymous memory for the object's segments failed.
    Map {
        /// Requested mapping length in bytes.
        size: usize,
        /// Original platform error.
        source: Errno,
    },
    /// A `PT_LOAD` segment's file range lies outside the object's bytes.
    SegmentOutsideFile {
        /// Byte range in the file named by the segment.
        range: Range<usize>,
    },
    /// A `PT_LOAD` segment's contents do not fit in the object's mapping.
    SegmentOutsideMapping {
        /// Byte range in the mapping the segment would be copied to.
        range: Range<usize>,
    },
    /// The `PT_DYNAMIC` segment's offset or size is invalid.
    DynamicSegment(object::Error),
    /// The object has no `PT_DYNAMIC` segment.
    NoDynamicSegment,
    /// An entry of the dynamic section is invalid or refers to invalid data.
    DynamicSection(object::Error),
}

impl LoadError {
    /// Returns the stable, null-terminated message used by `dlerror`.
    pub const fn repr(&self) -> &'static CStr {
        match self {
            Self::Parse(_) => {
                c"The DSO's ELF headers are invalid. Set `LD_DEBUG=all` for more information."
            }
            Self::NoLoadSegment => c"The DSO has no loadable segment.",
            Self::Map { .. } => {
                c"Failed to map memory for the DSO. Set `LD_DEBUG=all` for more information."
            }
            Self::SegmentOutsideFile { .. } => {
                c"A DSO segment extends past the end of its file. Set `LD_DEBUG=all` for more information."
            }
            Self::SegmentOutsideMapping { .. } => {
                c"A DSO segment does not fit in its mapping. Set `LD_DEBUG=all` for more information."
            }
            Self::DynamicSegment(_) => {
                c"The DSO's dynamic segment is invalid. Set `LD_DEBUG=all` for more information."
            }
            Self::NoDynamicSegment => c"The DSO has no dynamic segment.",
            Self::DynamicSection(_) => {
                c"The DSO's dynamic section is invalid. Set `LD_DEBUG=all` for more information."
            }
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(_) => f.write_str("invalid ELF headers"),
            Self::NoLoadSegment => f.write_str("no PT_LOAD segment"),
            Self::Map { size, .. } => write!(f, "mapping {size} bytes failed"),
            Self::SegmentOutsideFile { range } => {
                write!(f, "segment file range {range:#x?} is outside the file")
            }
            Self::SegmentOutsideMapping { range } => {
                write!(f, "segment range {range:#x?} is outside the mapping")
            }
            Self::DynamicSegment(_) => f.write_str("invalid PT_DYNAMIC segment"),
            Self::NoDynamicSegment => f.write_str("no PT_DYNAMIC segment"),
            Self::DynamicSection(_) => f.write_str("invalid dynamic section"),
        }
    }
}

impl core::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Parse(source) | Self::DynamicSegment(source) | Self::DynamicSection(source) => {
                Some(source)
            }
            Self::Map { source, .. } => Some(source),
            Self::NoLoadSegment
            | Self::NoDynamicSegment
            | Self::SegmentOutsideFile { .. }
            | Self::SegmentOutsideMapping { .. } => None,
        }
    }
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
    /// The selected object was opened but could not be loaded.
    Load {
        /// Selected object's path.
        path: String,
        /// Why loading failed.
        source: LoadError,
    },
    /// The DSO's TLS setup failed.
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

    /// This error followed by each of its sources, for diagnostics.
    pub(super) fn report(&self) -> Report<'_> {
        Report(self)
    }

    /// Returns the stable, null-terminated message used by `dlerror`.
    pub const fn repr(&self) -> &'static CStr {
        match self {
            Self::Load { source, .. } => source.repr(),
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

/// Display adapter for an error and its whole source chain, on one line.
///
/// An [`Errno`] in the chain is followed by its number, since the message table
/// may not name every value.
pub(super) struct Report<'a>(&'a (dyn core::error::Error + 'static));

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut error = Some(self.0);
        let mut separator = "";
        while let Some(current) = error {
            write!(f, "{separator}{current}")?;
            if let Some(errno) = current.downcast_ref::<Errno>() {
                write!(f, " (errno {})", errno.0)?;
            }
            separator = ": ";
            error = current.source();
        }
        Ok(())
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
        if matches!(
            self.error,
            DlError::Search { .. } | DlError::Open { .. } | DlError::Load { .. }
        ) {
            writeln!(f, "[ld.so]: {}", self.error.report())?;
        }
        f.write_str("[ld.so]: enable debug output with `LD_DEBUG=all` for more information")
    }
}

impl fmt::Display for DlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Search {
                access_error: Some((path, _)),
                ..
            } => write!(f, "access '{path}' failed"),
            Self::Search { name, paths, .. } => write!(f, "'{name}' is in none of {paths:?}"),
            Self::Open {
                path, operation, ..
            } => match operation {
                ObjectIoOperation::Open => write!(f, "opening '{path}' failed"),
                ObjectIoOperation::Stat => write!(f, "reading size of '{path}' failed"),
                ObjectIoOperation::Read => write!(f, "reading the headers of '{path}' failed"),
                ObjectIoOperation::Map { size } => {
                    write!(f, "mapping '{path}' ({size} bytes) failed")
                }
            },
            Self::Load { path, .. } => write!(f, "loading '{path}' failed"),
            _ => f.write_str(self.repr().to_str().expect("loader messages are ASCII")),
        }
    }
}

impl core::error::Error for DlError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Search {
                access_error: Some((_, source)),
                ..
            }
            | Self::Open { source, .. } => Some(source),
            Self::Load { source, .. } => Some(source),
            Self::NotFound
            | Self::Search { .. }
            | Self::Malformed
            | Self::InvalidHandle
            | Self::Oom => None,
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
        assert_eq!(format!("{access}"), "access '/lib/blocked.so' failed");
        assert!(format!("{}", access.report()).starts_with("access '/lib/blocked.so' failed: "));
        assert!(format!("{}", access.report()).ends_with("(errno 13)"));
        let opened = DlError::Open {
            path: "/lib/open.so".into(),
            operation: ObjectIoOperation::Open,
            source: Errno(5),
        };
        assert_eq!(format!("{opened}"), "opening '/lib/open.so' failed");
        assert!(format!("{}", opened.report()).starts_with("opening '/lib/open.so' failed: "));
        assert!(format!("{}", opened.report()).ends_with("(errno 5)"));
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
                ObjectIoOperation::Read,
                "reading the headers of '/lib/test.so' failed",
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

    fn parse_error() -> object::Error {
        use object::read::elf::FileHeader as _;
        object::elf::FileHeader64::<object::NativeEndian>::parse(&[0u8; 4][..])
            .err()
            .expect("four bytes are not an ELF header")
    }

    #[test]
    fn load_failures_name_the_object_and_cause() {
        let parse = DlError::Load {
            path: "/lib/bad.so".into(),
            source: LoadError::Parse(parse_error()),
        };
        assert_eq!(format!("{parse}"), "loading '/lib/bad.so' failed");
        let message = format!("{}", parse.report());
        assert!(
            message.starts_with("loading '/lib/bad.so' failed: invalid ELF headers: "),
            "{message}"
        );
        assert!(message.ends_with(&format!("{}", parse_error())));
        let fatal = format!("{}", parse.fatal("/bin/ion"));
        assert!(fatal.starts_with("[ld.so]: failed to link '/bin/ion': Load"));
        assert!(
            fatal.contains(&format!("\n[ld.so]: {message}\n")),
            "{fatal}"
        );
        assert_eq!(parse.repr(), LoadError::Parse(parse_error()).repr());
        assert_ne!(parse.repr(), DlError::Malformed.repr());

        let mapped = DlError::Load {
            path: "/lib/big.so".into(),
            source: LoadError::Map {
                size: 0x2000,
                source: Errno(12),
            },
        };
        let message = format!("{}", mapped.report());
        assert!(message.starts_with("loading '/lib/big.so' failed: mapping 8192 bytes failed: "));
        assert!(message.ends_with(" (errno 12)"), "{message}");
        assert_eq!(message.matches("errno").count(), 1);
    }

    #[test]
    fn dl_errors_expose_their_whole_source_chain() {
        use core::error::Error;
        let mapped = DlError::Load {
            path: "/lib/big.so".into(),
            source: LoadError::Map {
                size: 1,
                source: Errno(12),
            },
        };
        let load = mapped.source().expect("a load failure keeps its cause");
        assert!(matches!(
            load.downcast_ref::<LoadError>(),
            Some(LoadError::Map { size: 1, .. })
        ));
        let errno = load.source().expect("a mapping failure keeps its errno");
        assert_eq!(errno.downcast_ref::<Errno>(), Some(&Errno(12)));
        assert!(errno.source().is_none());

        let parse = DlError::Load {
            path: "/lib/bad.so".into(),
            source: LoadError::DynamicSection(parse_error()),
        };
        let object = parse
            .source()
            .and_then(Error::source)
            .expect("an invalid dynamic section keeps the object crate's error");
        assert!(object.downcast_ref::<object::Error>().is_some());

        let opened = DlError::Open {
            path: "/lib/open.so".into(),
            operation: ObjectIoOperation::Open,
            source: Errno(5),
        };
        let access = DlError::Search {
            name: "blocked.so".into(),
            paths: vec!["/lib".into()],
            access_error: Some(("/lib/blocked.so".into(), Errno(13))),
        };
        for (error, errno) in [(&opened, 5), (&access, 13)] {
            let source = error.source().and_then(|e| e.downcast_ref::<Errno>());
            assert_eq!(source, Some(&Errno(errno)));
        }
        let missing = DlError::Search {
            name: "missing.so".into(),
            paths: vec![],
            access_error: None,
        };
        for error in [
            missing,
            DlError::NotFound,
            DlError::Malformed,
            DlError::InvalidHandle,
            DlError::Oom,
        ] {
            assert!(error.source().is_none(), "{error:?}");
        }
    }

    #[test]
    fn load_errors_keep_their_sources() {
        use core::error::Error as _;
        let errors = [
            LoadError::Parse(parse_error()),
            LoadError::NoLoadSegment,
            LoadError::Map {
                size: 1,
                source: Errno(12),
            },
            LoadError::SegmentOutsideFile { range: 0x10..0x20 },
            LoadError::SegmentOutsideMapping { range: 0x30..0x40 },
            LoadError::DynamicSegment(parse_error()),
            LoadError::NoDynamicSegment,
            LoadError::DynamicSection(parse_error()),
        ];
        let has_source = [true, false, true, false, false, true, false, true];
        for (error, has_source) in errors.iter().zip(has_source) {
            assert_eq!(error.source().is_some(), has_source, "{error:?}");
        }
        let repr: Vec<_> = errors.iter().map(LoadError::repr).collect();
        for (i, message) in repr.iter().enumerate() {
            assert!(message.to_str().is_ok());
            assert!(
                !repr[..i].contains(message),
                "dlerror cannot tell {:?} apart",
                errors[i]
            );
        }
        let source = errors[2].source().expect("mapping keeps its errno");
        assert_eq!(source.downcast_ref::<Errno>(), Some(&Errno(12)));
        assert_eq!(
            format!("{}", errors[3]),
            "segment file range 0x10..0x20 is outside the file"
        );
        assert_eq!(
            format!("{}", errors[4]),
            "segment range 0x30..0x40 is outside the mapping"
        );
        assert_eq!(format!("{}", errors[1]), "no PT_LOAD segment");
        assert_eq!(format!("{}", errors[6]), "no PT_DYNAMIC segment");
    }
}
