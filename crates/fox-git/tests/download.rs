//! Blob downloads over HTTP, from a server on 127.0.0.1.

use fox_core::Blake3Digest;
use fox_git::download::BlobError;
use fox_git::{DownloadError, HttpError, download_blob};

mod common;
use common::http_server;

#[test]
fn blobs_download_over_http_and_follow_redirects() {
    let blob = b"prebuilt archive";
    let hash = Blake3Digest::of(blob);
    let files = tempfile::tempdir().unwrap();
    std::fs::create_dir(files.path().join("blake3")).unwrap();
    std::fs::write(files.path().join(format!("blake3/{hash}")), blob).unwrap();
    let mirror = http_server::serve_files(files.path());
    let empty = http_server::serve_files(tempfile::tempdir().unwrap().path());

    // The first mirror lacks the blob; the second has it.
    let bytes = download_blob(&[&empty, &mirror], &hash, blob.len() as u64).unwrap();
    assert_eq!(bytes, blob);

    let error = download_blob(&[&empty], &hash, blob.len() as u64).unwrap_err();
    let DownloadError::Failed { source, .. } = error else {
        panic!("{error:?}")
    };
    assert!(
        matches!(&*source, BlobError::Http(HttpError::Status { status, .. }) if status.as_u16() == 404),
        "{source:?}"
    );

    let location = mirror.clone();
    let redirecting = http_server::serve(move |request| {
        let mut response = http_server::Response::new("302 Found", "");
        response
            .headers
            .push(("Location".into(), format!("{location}{}", request.path)));
        response
    });
    let bytes = download_blob(&[&redirecting], &hash, blob.len() as u64).unwrap();
    assert_eq!(bytes, blob);

    let error = download_blob(&[&mirror], &hash, 3).unwrap_err();
    let DownloadError::Failed { source, .. } = error else {
        panic!("{error:?}")
    };
    assert!(matches!(*source, BlobError::TooLarge { expected: 3 }));
}
