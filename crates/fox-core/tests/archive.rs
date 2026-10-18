//! Opening real archives built with a test key.

use std::fs;
use std::path::{Path, PathBuf};

use fox_core::archive::{Archive, ArchiveError, MetadataSource};
use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
use fox_core::{
    Blake3Digest, FileKind, HEADER_SIZE, Manifest, PackageName, TrustedKeys, VerifyError,
};

const HELLO: &[u8] = b"#!/bin/sh\necho hello from fox\n";

fn manifest() -> Manifest {
    let mut manifest = Manifest::new(PackageName::new("hello").unwrap(), "1.0.0");
    manifest.license = Some("MIT".into());
    manifest
}

fn entries() -> [FixtureEntry<'static>; 3] {
    [
        FixtureEntry::file("usr/bin/hello", HELLO, 0o755),
        FixtureEntry::file("usr/share/doc/hello/README", b"read me", 0o644),
        FixtureEntry::symlink("usr/bin/hi", "hello"),
    ]
}

fn build(dir: &Path, compressed: bool) -> (FixtureKey, PathBuf) {
    let key = FixtureKey::generate(dir, "test").unwrap();
    let path = build_package(dir, &key, Some(&manifest()), &entries(), compressed).unwrap();
    (key, path)
}

fn flip_byte(path: &Path, offset: usize) {
    let mut bytes = fs::read(path).unwrap();
    bytes[offset] ^= 0x01;
    fs::write(path, bytes).unwrap();
}

#[test]
fn opens_uncompressed_and_compressed_archives() {
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (key, path) = build(dir.path(), compressed);
        let archive = Archive::open(&path, &key.trusted()).unwrap();
        assert_eq!(archive.manifest(), &manifest());
        assert_eq!(archive.metadata_source(), &MetadataSource::Embedded);
        assert_eq!(archive.head().signer().label, "test");
        assert_eq!(
            archive.blake3(),
            Blake3Digest::of(&fs::read(&path).unwrap())
        );

        let hello = archive
            .files()
            .iter()
            .find(|f| f.path == "usr/bin/hello")
            .unwrap();
        assert_eq!(hello.blake3, Blake3Digest::of(HELLO));
        assert_eq!(hello.size, HELLO.len() as u64);
        assert_eq!(hello.mode, 0o755);
        let link = archive
            .files()
            .iter()
            .find(|f| f.path == "usr/bin/hi")
            .unwrap();
        assert_eq!(link.kind, FileKind::Symlink);
        assert_eq!(link.blake3, Blake3Digest::of(b"hello"));
        // Three files plus the manifest.
        assert_eq!(archive.files().len(), 4);
    }
}

#[test]
fn rejects_untrusted_signer() {
    let dir = tempfile::tempdir().unwrap();
    let (_, path) = build(dir.path(), false);
    let other = FixtureKey::generate(dir.path(), "other").unwrap();
    let error = Archive::open(&path, &other.trusted()).unwrap_err();
    assert!(matches!(
        error,
        ArchiveError::Verify {
            source: VerifyError::UntrustedSigner { .. },
            ..
        }
    ));
    let error = Archive::open(&path, &TrustedKeys::new()).unwrap_err();
    assert!(matches!(
        error,
        ArchiveError::Verify {
            source: VerifyError::NoTrustedKeys,
            ..
        }
    ));
}

#[test]
fn rejects_tampered_header_table_and_data() {
    let dir = tempfile::tempdir().unwrap();
    let (key, path) = build(dir.path(), false);
    let pristine = fs::read(&path).unwrap();

    // Signed part of the header (the entry count).
    flip_byte(&path, 128);
    assert!(matches!(
        Archive::open(&path, &key.trusted()).unwrap_err(),
        ArchiveError::Verify {
            source: VerifyError::BadSignature(_),
            ..
        }
    ));

    // The first entry's path in the entry table.
    fs::write(&path, &pristine).unwrap();
    flip_byte(&path, HEADER_SIZE + 60);
    assert!(matches!(
        Archive::open(&path, &key.trusted()).unwrap_err(),
        ArchiveError::Verify {
            source: VerifyError::EntriesHash(_),
            ..
        }
    ));

    // File contents in the data section.
    fs::write(&path, &pristine).unwrap();
    let at = pristine
        .windows(HELLO.len())
        .position(|w| w == HELLO)
        .unwrap();
    flip_byte(&path, at + 3);
    match Archive::open(&path, &key.trusted()).unwrap_err() {
        ArchiveError::EntryHash { entry, .. } => assert_eq!(entry, "usr/bin/hello"),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn rejects_truncated_archive() {
    let dir = tempfile::tempdir().unwrap();
    let (key, path) = build(dir.path(), false);
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..HEADER_SIZE + 10]).unwrap();
    assert!(matches!(
        Archive::open(&path, &key.trusted()).unwrap_err(),
        ArchiveError::Verify {
            source: VerifyError::Truncated { .. },
            ..
        }
    ));
}

#[test]
fn rejects_manifest_with_wrong_name() {
    let dir = tempfile::tempdir().unwrap();
    let key = FixtureKey::generate(dir.path(), "test").unwrap();
    let text = manifest().to_toml().unwrap().replace("hello", "other");
    let entries = [FixtureEntry::file(
        "usr/share/fox/manifest/hello.toml",
        text.as_bytes(),
        0o644,
    )];
    let path = build_package(dir.path(), &key, None, &entries, false).unwrap();
    assert!(matches!(
        Archive::open(&path, &key.trusted()).unwrap_err(),
        ArchiveError::ManifestNameMismatch { .. }
    ));
}

#[test]
fn reads_cookbook_sidecar_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let key = FixtureKey::generate(dir.path(), "test").unwrap();
    let path = build_package(dir.path(), &key, None, &entries(), true).unwrap();

    assert!(matches!(
        Archive::open(&path, &key.trusted()).unwrap_err(),
        ArchiveError::NoMetadata { .. }
    ));

    let head_blake3 = {
        // The cookbook writes the signed entry-table hash into the sidecar.
        let bytes = fs::read(&path).unwrap();
        let mut hash = [0; 32];
        hash.copy_from_slice(&bytes[96..128]);
        Blake3Digest::from_bytes(hash)
    };
    let sidecar = path.with_extension("toml");
    fs::write(
        &sidecar,
        format!(
            "name = \"hello\"\nversion = \"2.0\"\ntarget = \"x86_64-unknown-redox\"\n\
             blake3 = \"{head_blake3}\"\ndepends = [\"relibc\"]\n"
        ),
    )
    .unwrap();
    let archive = Archive::open(&path, &key.trusted()).unwrap();
    assert_eq!(
        archive.metadata_source(),
        &MetadataSource::Sidecar(sidecar.clone())
    );
    assert_eq!(archive.manifest().name.as_str(), "hello");
    assert_eq!(archive.manifest().version, "2.0");
    assert_eq!(archive.manifest().depends[0].as_str(), "relibc");

    fs::write(
        &sidecar,
        format!(
            "name = \"hello\"\ntarget = \"x\"\nblake3 = \"{}\"\ndepends = []\n",
            "0".repeat(64)
        ),
    )
    .unwrap();
    assert!(matches!(
        Archive::open(&path, &key.trusted()).unwrap_err(),
        ArchiveError::SidecarMismatch { .. }
    ));
}
