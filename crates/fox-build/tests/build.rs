//! End-to-end hermetic builds of a small git repository.
//!
//! The fixture is a workspace with a binary crate and a path library, no
//! registry dependencies, so `cargo fetch` needs no network.

use std::fs;
use std::path::Path;
use std::process::Command;

use fox_build::{Auditable, BuildError, BuildRequest, FeatureSelection, Signer, build, package};
use fox_core::archive::Archive;
use fox_core::attested::verify_archive_attestation;
use fox_core::fixtures::FixtureKey;
use fox_core::{CpuLevel, sbom_path};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Fox Test",
            "-c",
            "user.email=fox@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Writes and commits the fixture repository.
fn fixture(dir: &Path, with_lock: bool) {
    write(
        &dir.join("Cargo.toml"),
        r#"[workspace]
members = ["hello", "greet"]
resolver = "2"
"#,
    );
    write(
        &dir.join("hello/Cargo.toml"),
        r#"[package]
name = "hello"
version = "1.2.0"
edition = "2021"
license = "MIT"
description = "Says hello"

[features]
default = ["loud"]
loud = []
quiet = []

[dependencies]
greet = { path = "../greet" }

[package.metadata.fox]
runtime-deps = ["relibc"]

[package.metadata.fox.files]
"README" = "/usr/share/doc/hello/README"
"#,
    );
    write(
        &dir.join("hello/src/main.rs"),
        "fn main() { println!(\"{}\", greet::greeting()); if cfg!(feature = \"loud\") { println!(\"!\"); } }\n",
    );
    write(&dir.join("hello/README"), "Says hello.\n");
    write(
        &dir.join("greet/Cargo.toml"),
        "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlicense = \"MIT OR Apache-2.0\"\n",
    );
    write(
        &dir.join("greet/src/lib.rs"),
        "pub fn greeting() -> &'static str { \"hello\" }\n",
    );
    git(dir, &["init", "-q", "-b", "main"]);
    if with_lock {
        let status = Command::new("cargo")
            .current_dir(dir)
            .args(["generate-lockfile", "--offline"])
            .status()
            .unwrap();
        assert!(status.success());
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "hello"]);
}

fn request(repo: &Path, work: &Path) -> BuildRequest {
    let mut request = BuildRequest::new(repo, work);
    request.package_dir = "hello".into();
    request.git_url = Some("https://example.com/hello.git".into());
    request.auditable = Auditable::Off;
    request
}

#[test]
fn builds_packages_and_attests_reproducibly() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path(), true);
    // Uncommitted changes must not reach the build.
    write(&repo.path().join("hello/README"), "changed\n");

    let work_a = tempfile::tempdir().unwrap();
    let work_b = tempfile::tempdir().unwrap();
    let a = build(&request(repo.path(), work_a.path())).unwrap();
    let b = build(&request(repo.path(), work_b.path())).unwrap();

    assert_eq!(a.manifest.name.as_str(), "hello");
    assert_eq!(a.manifest.version, "1.2.0");
    assert_eq!(a.source.path.as_deref(), Some("hello"));
    assert_eq!(a.built.features, ["default", "loud"]);
    assert!(!a.built.auditable);
    if a.built.target.starts_with("x86_64-") {
        assert_eq!(a.built.cpu_level, Some(CpuLevel::X86_64V3));
    }
    // Reproducible: same files and SBOM from different work directories.
    assert_eq!(a.files, b.files);
    assert_eq!(a.sbom, b.sbom);
    let paths: Vec<&str> = a.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "usr/bin/hello",
            "usr/share/doc/hello/README",
            "usr/share/fox/manifest/hello.toml",
            "usr/share/fox/sbom/hello.cdx.json",
        ]
    );
    assert_eq!(
        fs::read_to_string(a.stage.join("usr/share/doc/hello/README")).unwrap(),
        "Says hello.\n"
    );
    let sbom: serde_json::Value = serde_json::from_slice(&a.sbom).unwrap();
    let names: Vec<&str> = sbom["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"greet"));
    assert!(names.contains(&"usr/bin/hello"));
    assert!(!names.contains(&"usr/share/fox/sbom/hello.cdx.json"));

    let keys = tempfile::tempdir().unwrap();
    let key = FixtureKey::generate(keys.path(), "ci").unwrap();
    let signer = Signer::load(&key.secret_path, "ferric-ci-1").unwrap();
    let out_a = tempfile::tempdir().unwrap();
    let out_b = tempfile::tempdir().unwrap();
    let packed_a = package(&a, &signer, out_a.path()).unwrap();
    let packed_b = package(&b, &signer, out_b.path()).unwrap();
    assert_eq!(
        packed_a.contents.output.pkgar_blake3,
        packed_b.contents.output.pkgar_blake3
    );
    assert_eq!(packed_a.contents.build.builder, "ferric-ci-1");
    assert_eq!(
        packed_a.contents.source.git,
        "https://example.com/hello.git"
    );

    // An independent verifier trusting the key accepts it.
    let archive = Archive::open(&packed_a.archive, &key.trusted()).unwrap();
    let verified =
        verify_archive_attestation(&archive, &packed_a.attestation, &key.trusted()).unwrap();
    assert_eq!(verified.attestation().package.version, "1.2.0");
    assert!(
        archive
            .files()
            .iter()
            .any(|f| f.path == sbom_path(&archive.manifest().name))
    );
    // The rebuild is byte-identical, so the first attestation holds for it.
    let rebuilt = Archive::open(&packed_b.archive, &key.trusted()).unwrap();
    verify_archive_attestation(&rebuilt, &packed_a.attestation, &key.trusted()).unwrap();
}

#[test]
fn features_change_the_identity() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path(), true);
    let work = tempfile::tempdir().unwrap();
    let quiet_work = tempfile::tempdir().unwrap();
    let default = build(&request(repo.path(), work.path())).unwrap();
    let mut quiet = request(repo.path(), quiet_work.path());
    quiet.features = FeatureSelection {
        default_features: false,
        features: vec!["quiet".into()],
    };
    let quiet = build(&quiet).unwrap();
    assert_eq!(quiet.built.features, ["quiet"]);
    assert_ne!(quiet.built.feature_set_hash, default.built.feature_set_hash);
    assert_eq!(quiet.source.tree_blake3, default.source.tree_blake3);

    // An attestation does not transfer to an archive of another variant.
    let keys = tempfile::tempdir().unwrap();
    let key = FixtureKey::generate(keys.path(), "ci").unwrap();
    let signer = Signer::load(&key.secret_path, "ci").unwrap();
    let out = tempfile::tempdir().unwrap();
    let default_packed = package(&default, &signer, out.path()).unwrap();
    let quiet_packed = package(&quiet, &signer, out.path()).unwrap();
    assert_ne!(default_packed.archive, quiet_packed.archive);
    let quiet_archive = Archive::open(&quiet_packed.archive, &key.trusted()).unwrap();
    assert!(
        verify_archive_attestation(&quiet_archive, &default_packed.attestation, &key.trusted())
            .is_err()
    );
}

#[test]
fn refuses_missing_lockfile_and_bad_requests() {
    let repo = tempfile::tempdir().unwrap();
    fixture(repo.path(), false);
    let work = tempfile::tempdir().unwrap();
    assert!(matches!(
        build(&request(repo.path(), work.path())),
        Err(BuildError::MissingLockfile { .. })
    ));

    let mut missing = request(repo.path(), work.path());
    missing.package_dir = "nope".into();
    assert!(matches!(
        build(&missing),
        Err(BuildError::NoManifest { .. })
    ));

    let mut escape = request(repo.path(), work.path());
    escape.package_dir = "../x".into();
    assert!(matches!(build(&escape), Err(BuildError::PackageDir { .. })));
}
