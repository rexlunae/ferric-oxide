//! Retains fixture images after failures and removes them after successful checks.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// A successfully checked fixture could not be removed.
#[derive(Debug, Error)]
#[error("cannot remove fixture image {}", path.display())]
pub(crate) struct RemoveError {
    path: PathBuf,
    #[source]
    source: io::Error,
}

/// Removes the image only after both its checks and daemon teardown succeeded.
///
/// # Errors
///
/// Returns the original failure without touching the image. After success, reports
/// a removal failure as [`RemoveError`], converted to the caller's error type.
pub(crate) fn finish<T, E>(path: &Path, result: Result<T, E>) -> Result<T, E>
where
    E: From<RemoveError>,
{
    let value = result?;
    fs::remove_file(path).map_err(|source| RemoveError {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Error)]
    enum Failure {
        #[error("fixture check failed")]
        Check,
        #[error("stopping the daemon failed")]
        Stop,
        #[error("reaping the daemon failed")]
        Reap,
        #[error(transparent)]
        Remove(#[from] RemoveError),
    }

    #[test]
    fn failed_checks_or_teardown_retain_the_image_and_original_error() {
        for cause in [Failure::Check, Failure::Stop, Failure::Reap] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fixture.ext4");
            fs::write(&path, b"diagnostic image bytes").unwrap();
            let expected = std::mem::discriminant(&cause);
            let error = finish::<(), _>(&path, Err(cause)).unwrap_err();
            assert_eq!(std::mem::discriminant(&error), expected);
            assert_eq!(fs::read(&path).unwrap(), b"diagnostic image bytes");
        }
    }

    #[test]
    fn successful_checks_and_teardown_remove_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.ext4");
        fs::write(&path, b"completed fixture").unwrap();
        assert_eq!(finish::<_, Failure>(&path, Ok(42)).unwrap(), 42);
        assert!(!path.exists());
    }

    #[test]
    fn removal_failures_keep_the_path_and_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.ext4");
        let Failure::Remove(error) = finish::<_, Failure>(&path, Ok(())).unwrap_err() else {
            panic!("expected a removal failure");
        };
        assert_eq!(error.path, path);
        assert_eq!(error.source.kind(), io::ErrorKind::NotFound);
    }
}
