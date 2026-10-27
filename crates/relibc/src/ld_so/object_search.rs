//! Dynamic-object search order and attempt-local error construction.
//!
//! The linker supplies platform access checks; the search policy and its error context
//! are shared with host tests. Candidate misses never escape a successful fallback.

use alloc::{borrow::Cow, format, string::ToString, vec::Vec};

use super::load_error::DlError;
use crate::{error::Errno, header::errno::ENOENT};

/// Separator between dynamic-loader library search directories.
pub const PATH_SEP: char = ':';

/// Searches the direct path, parent runpath, environment path and `/lib`, in order.
///
/// # Errors
/// Returns [`DlError::Search`] with the searched directories and last non-absence access
/// error when no candidate succeeds. The returned error owns only this attempt's context.
pub(super) fn find<'a>(
    name: &'a str,
    parent_runpath: Option<&str>,
    library_path: Option<&str>,
    mut accessible: impl FnMut(&str) -> Result<(), Errno>,
) -> Result<Cow<'a, str>, DlError> {
    let mut access_error = None;
    if name.contains('/') {
        match accessible(name) {
            Ok(()) => return Ok(name.into()),
            Err(source) if source.0 != ENOENT => access_error = Some((name.to_string(), source)),
            Err(_) => {}
        }
    }
    let mut paths = Vec::new();
    if let Some(runpath) = parent_runpath {
        paths.extend(runpath.split(PATH_SEP));
    }
    if let Some(path) = library_path {
        paths.extend(path.split(PATH_SEP));
    }
    paths.push("/lib");
    for part in &paths {
        let path = format!("{part}/{name}");
        match accessible(&path) {
            Ok(()) => return Ok(path.into()),
            Err(source) if source.0 != ENOENT => access_error = Some((path, source)),
            Err(_) => {}
        }
    }
    Err(DlError::Search {
        name: name.to_string(),
        paths: paths.into_iter().map(ToString::to_string).collect(),
        access_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_failed_search_reaches_the_startup_formatter_with_its_context() {
        let mut tried = Vec::new();
        let error = find("missing.so", Some("/parent"), Some("/env:/other"), |path| {
            tried.push(path.to_string());
            Err(Errno(ENOENT))
        })
        .unwrap_err();
        assert_eq!(
            tried,
            vec![
                "/parent/missing.so",
                "/env/missing.so",
                "/other/missing.so",
                "/lib/missing.so"
            ]
        );
        let output = format!("{}", error.fatal("/bin/ion"));
        assert!(output.starts_with("[ld.so]: failed to link '/bin/ion': Search"));
        assert!(output.contains(
            "\n[ld.so]: 'missing.so' is in none of [\"/parent\", \"/env\", \"/other\", \"/lib\"]\n"
        ));
    }

    #[test]
    fn a_successful_fallback_and_a_later_parse_failure_drop_earlier_context() {
        let earlier = find("earlier.so", None, None, |_| Err(Errno(13))).unwrap_err();
        let mut tried = Vec::new();
        let path = find("new.so", Some("/denied"), Some("/found"), |path| {
            tried.push(path.to_string());
            if path == "/found/new.so" {
                Ok(())
            } else {
                Err(Errno(13))
            }
        })
        .unwrap();
        assert_eq!(path, "/found/new.so");
        assert_eq!(tried, vec!["/denied/new.so", "/found/new.so"]);
        let output = format!("{}", DlError::Malformed.fatal(&path));
        assert_eq!(output.lines().count(), 2);
        assert!(!output.contains("earlier.so"));
        assert!(!output.contains("errno"));
        assert!(!output.contains("denied"));
        assert!(format!("{}", earlier.fatal("/bin/old")).contains("errno 13"));
    }

    #[test]
    fn direct_paths_and_non_absence_errors_are_preserved() {
        let direct = find("/chosen/ok.so", None, None, |path| {
            assert_eq!(path, "/chosen/ok.so");
            Ok(())
        })
        .unwrap();
        assert!(matches!(direct, Cow::Borrowed("/chosen/ok.so")));
        let error = find("bad.so", Some("/denied"), None, |path| {
            Err(Errno(if path.starts_with("/denied") {
                13
            } else {
                ENOENT
            }))
        })
        .unwrap_err();
        let output = format!("{}", error.fatal("/bin/ion"));
        assert!(output.contains("\n[ld.so]: access '/denied/bad.so' failed:"));
        assert!(output.contains("(errno 13)\n"));
    }
}
