//! Downloading content-addressed blobs: prebuilt archives, attestations and
//! their signatures, which registries store at `<base>/blake3/<hash>`.
//!
//! [`download_blob`] tries each base URL in turn and accepts the first
//! response whose BLAKE3 matches, so a mirror carries no trust. Reads stop
//! after the caller's size limit.
//! `file://` URLs and plain paths read the local file system; `https://`
//! and `http://` use the same reqwest and rustls-on-ring stack as git
//! fetching, and are not built for Redox.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

use fox_core::Blake3Digest;

/// Why no base URL supplied a blob.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DownloadError {
    /// No base URLs were given.
    #[error("no URLs to download blob {blake3} from")]
    NoUrls {
        /// The blob.
        blake3: Blake3Digest,
    },
    /// Every URL failed; this is the last failure.
    #[error("cannot download blob {blake3}; last tried {url}")]
    Failed {
        /// The blob.
        blake3: Blake3Digest,
        /// The last URL tried.
        url: String,
        /// Why it failed.
        #[source]
        source: Box<BlobError>,
    },
}

/// Why one URL did not supply a blob.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlobError {
    /// A local file could not be read.
    #[error("cannot read {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// The HTTP request failed.
    #[cfg(not(target_os = "redox"))]
    #[error("HTTP request failed")]
    Http(#[source] reqwest::Error),
    /// The URL scheme is not supported on this system.
    #[error("unsupported URL scheme")]
    Scheme,
    /// The content is larger than the limit.
    #[error("blob is larger than {expected} bytes")]
    TooLarge {
        /// The expected length.
        expected: u64,
    },
    /// The content has the wrong hash.
    #[error("blob content does not match its hash")]
    Mismatch,
}

/// Downloads the blob with this hash, of at most `max_length` bytes, from
/// the first of `base_urls` that has it, as `<base>/blake3/<hash>`.
///
/// # Errors
///
/// [`DownloadError::NoUrls`] if `base_urls` is empty, and
/// [`DownloadError::Failed`] with the last URL's [`BlobError`] if none
/// supplies matching content.
pub fn download_blob<S: AsRef<str>>(
    base_urls: &[S],
    blake3: &Blake3Digest,
    max_length: u64,
) -> Result<Vec<u8>, DownloadError> {
    let mut last = None;
    for base in base_urls {
        let url = format!("{}/blake3/{blake3}", base.as_ref().trim_end_matches('/'));
        match fetch_url(&url, max_length).and_then(|bytes| check(bytes, blake3)) {
            Ok(bytes) => return Ok(bytes),
            Err(error) => last = Some((url, error)),
        }
    }
    match last {
        Some((url, error)) => Err(DownloadError::Failed {
            blake3: *blake3,
            url,
            source: Box::new(error),
        }),
        None => Err(DownloadError::NoUrls { blake3: *blake3 }),
    }
}

fn check(bytes: Vec<u8>, blake3: &Blake3Digest) -> Result<Vec<u8>, BlobError> {
    if Blake3Digest::of(&bytes) != *blake3 {
        return Err(BlobError::Mismatch);
    }
    Ok(bytes)
}

fn fetch_url(url: &str, length: u64) -> Result<Vec<u8>, BlobError> {
    if let Some(path) = url.strip_prefix("file://") {
        return read_file(PathBuf::from(path), length);
    }
    if url.starts_with('/') {
        return read_file(PathBuf::from(url), length);
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        return fetch_http(url, length);
    }
    Err(BlobError::Scheme)
}

fn read_file(path: PathBuf, length: u64) -> Result<Vec<u8>, BlobError> {
    let file = fs::File::open(&path).map_err(|source| BlobError::Io {
        path: path.clone(),
        source,
    })?;
    read_limited(file, length).map_err(|source| match source {
        Limited::Io(source) => BlobError::Io { path, source },
        Limited::TooLarge => BlobError::TooLarge { expected: length },
    })
}

enum Limited {
    Io(io::Error),
    TooLarge,
}

fn read_limited(reader: impl Read, length: u64) -> Result<Vec<u8>, Limited> {
    let mut bytes = Vec::new();
    reader
        .take(length.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(Limited::Io)?;
    if bytes.len() as u64 > length {
        return Err(Limited::TooLarge);
    }
    Ok(bytes)
}

#[cfg(not(target_os = "redox"))]
fn fetch_http(url: &str, length: u64) -> Result<Vec<u8>, BlobError> {
    crate::fetch::install_tls_provider();
    let response = reqwest::blocking::get(url)
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(BlobError::Http)?;
    read_limited(response, length).map_err(|error| match error {
        Limited::Io(source) => BlobError::Io {
            path: PathBuf::from(url),
            source,
        },
        Limited::TooLarge => BlobError::TooLarge { expected: length },
    })
}

#[cfg(target_os = "redox")]
fn fetch_http(_url: &str, _length: u64) -> Result<Vec<u8>, BlobError> {
    Err(BlobError::Scheme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_come_from_the_first_matching_mirror() {
        let bad = tempfile::tempdir().unwrap();
        let good = tempfile::tempdir().unwrap();
        let blob = b"artifact bytes";
        let hash = Blake3Digest::of(blob);
        for (dir, content) in [(&bad, &b"tampered bytes"[..]), (&good, &blob[..])] {
            fs::create_dir_all(dir.path().join("blake3")).unwrap();
            fs::write(dir.path().join(format!("blake3/{hash}")), content).unwrap();
        }
        let urls = [
            format!("file://{}", bad.path().display()),
            good.path().display().to_string(),
        ];
        assert_eq!(
            download_blob(&urls, &hash, blob.len() as u64).unwrap(),
            blob
        );
        let error = download_blob(&urls[..1], &hash, blob.len() as u64).unwrap_err();
        assert!(matches!(error, DownloadError::Failed { .. }));
        let error = download_blob(&urls[1..], &hash, 3).unwrap_err();
        let DownloadError::Failed { source, .. } = error else {
            panic!("{error:?}")
        };
        assert!(matches!(*source, BlobError::TooLarge { expected: 3 }));
        assert!(matches!(
            download_blob::<&str>(&[], &hash, 1),
            Err(DownloadError::NoUrls { .. })
        ));
        assert!(matches!(
            download_blob(&["ftp://x"], &hash, 1),
            Err(DownloadError::Failed { .. })
        ));
    }
}
