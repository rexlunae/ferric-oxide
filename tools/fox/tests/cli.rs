//! End-to-end tests of the `fox` binary with fixture packages.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
use fox_core::{Manifest, PackageName};

struct Env {
    work: tempfile::TempDir,
    root: tempfile::TempDir,
    key: FixtureKey,
}

impl Env {
    fn new() -> Self {
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let key = FixtureKey::generate(work.path(), "ci").unwrap();
        Self { work, root, key }
    }

    /// Trust the fixture key the way a system does: a key file under the root.
    fn trust_in_root(&self) {
        let dir = self.root.path().join("etc/fox/keys");
        fs::create_dir_all(&dir).unwrap();
        fs::copy(&self.key.public_path, dir.join("ferric-ci-1.toml")).unwrap();
    }

    fn package(&self, name: &str, version: &str) -> PathBuf {
        let dir = self.work.path().join(format!("{name}-{version}"));
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = Manifest::new(PackageName::new(name).unwrap(), version);
        manifest.license = Some("MIT".into());
        let bin = format!("usr/bin/{name}");
        let entries = [FixtureEntry::file(&bin, b"#!/bin/sh\necho fox\n", 0o755)];
        build_package(&dir, &self.key, Some(&manifest), &entries, true).unwrap()
    }

    fn fox(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fox"))
            .arg("--root")
            .arg(self.root.path())
            .args(args)
            .output()
            .unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn verify_accepts_trusted_and_rejects_untrusted_or_tampered() {
    let env = Env::new();
    let archive = env.package("hello", "1.0.0");

    let untrusted = env.fox(&["verify", path_str(&archive)]);
    assert!(!untrusted.status.success());
    assert!(stdout(&untrusted).contains("no trusted signing keys"));

    env.trust_in_root();
    let ok = env.fox(&["verify", path_str(&archive)]);
    assert!(ok.status.success());
    assert!(stdout(&ok).starts_with("ok "));
    assert!(stdout(&ok).contains("signed by ferric-ci-1"));

    let report = json(&env.fox(&["--json", "verify", path_str(&archive)]));
    assert_eq!(report[0]["ok"], true);
    assert_eq!(report[0]["package"]["name"], "hello");
    assert_eq!(report[0]["package"]["metadata"], "embedded");

    let mut bytes = fs::read(&archive).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    let tampered = env.work.path().join("tampered.pkgar");
    fs::write(&tampered, bytes).unwrap();
    let bad = env.fox(&["verify", path_str(&archive), path_str(&tampered)]);
    assert!(!bad.status.success());
    let text = stdout(&bad);
    assert!(text.contains("ok "));
    assert!(text.contains("FAILED"));
}

#[test]
fn key_flag_trusts_a_key_file() {
    let env = Env::new();
    let archive = env.package("hello", "1.0.0");
    let ok = env.fox(&[
        "--key",
        path_str(&env.key.public_path),
        "verify",
        path_str(&archive),
    ]);
    assert!(ok.status.success(), "{}", stdout(&ok));
}

#[test]
fn sbom_writes_cyclonedx_json() {
    let env = Env::new();
    env.trust_in_root();
    let archive = env.package("hello", "1.0.0");
    let sbom = json(&env.fox(&["sbom", path_str(&archive)]));
    assert_eq!(sbom["bomFormat"], "CycloneDX");
    assert_eq!(sbom["metadata"]["component"]["name"], "hello");
    assert_eq!(
        sbom["metadata"]["component"]["licenses"][0]["expression"],
        "MIT"
    );
    let names: Vec<_> = sbom["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(names.contains(&"usr/bin/hello".to_owned()));

    let out = env.work.path().join("hello.cdx.json");
    assert!(
        env.fox(&["sbom", path_str(&archive), "-o", path_str(&out)])
            .status
            .success()
    );
    let from_file: serde_json::Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
    assert_eq!(from_file, sbom);
}

#[test]
fn install_list_verify_remove_round_trip() {
    let env = Env::new();
    env.trust_in_root();
    let hello = env.package("hello", "1.0.0");
    let world = env.package("world", "0.1.0");

    let installed = env.fox(&["install", path_str(&hello), path_str(&world)]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(stdout(&installed).contains("installed hello 1.0.0"));
    assert!(env.root.path().join("usr/bin/hello").is_file());

    let listed = json(&env.fox(&["--json", "list"]));
    let names: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].clone())
        .collect();
    assert_eq!(names, ["hello", "world"]);
    assert_eq!(listed[0]["signer_label"], "ferric-ci-1");

    assert!(env.fox(&["verify", "--installed"]).status.success());
    fs::write(env.root.path().join("usr/bin/world"), b"changed").unwrap();
    let changed = env.fox(&["verify", "--installed", "world"]);
    assert!(!changed.status.success());
    assert!(stdout(&changed).contains("usr/bin/world: modified"));

    let upgrade = env.package("hello", "2.0.0");
    let upgraded = env.fox(&["install", path_str(&upgrade)]);
    assert!(stdout(&upgraded).contains("upgraded hello 1.0.0 -> 2.0.0"));

    let removed = env.fox(&["remove", "hello", "world"]);
    assert!(removed.status.success());
    assert!(stdout(&removed).contains("kept locally changed usr/bin/world"));
    assert!(!env.root.path().join("usr/bin/hello").exists());
    assert_eq!(json(&env.fox(&["--json", "list"])), serde_json::json!([]));
}

#[test]
fn install_verifies_every_archive_before_installing_any() {
    let env = Env::new();
    env.trust_in_root();
    let good = env.package("hello", "1.0.0");
    let other_key = FixtureKey::generate(env.work.path(), "other").unwrap();
    let dir = env.work.path().join("evil");
    fs::create_dir_all(&dir).unwrap();
    let manifest = Manifest::new(PackageName::new("evil").unwrap(), "1");
    let evil = build_package(&dir, &other_key, Some(&manifest), &[], false).unwrap();

    let output = env.fox(&["install", path_str(&good), path_str(&evil)]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("untrusted key"), "{stderr}");
    assert!(!env.root.path().join("usr/bin/hello").exists());
}

#[test]
fn remove_rejects_bad_and_unknown_names() {
    let env = Env::new();
    let bad = env.fox(&["remove", "../etc"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8(bad.stderr)
            .unwrap()
            .contains("invalid package name")
    );
    let unknown = env.fox(&["remove", "nothing"]);
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8(unknown.stderr)
            .unwrap()
            .contains("not installed")
    );
}

#[test]
fn unsigned_metadata_needs_a_flag_and_symlinked_roots_are_refused() {
    let env = Env::new();
    env.trust_in_root();
    let dir = env.work.path().join("unsigned");
    fs::create_dir_all(&dir).unwrap();
    let entries = [FixtureEntry::file("usr/bin/tool", b"tool", 0o755)];
    let archive = build_package(&dir, &env.key, None, &entries, false).unwrap();
    let bytes = fs::read(&archive).unwrap();
    let mut hash = [0; 32];
    hash.copy_from_slice(&bytes[96..128]);
    let blake3 = fox_core::Blake3Digest::from_bytes(hash);
    fs::write(
        archive.with_extension("toml"),
        format!("name = \"tool\"\nversion = \"1\"\ntarget = \"x\"\nblake3 = \"{blake3}\"\ndepends = []\n"),
    )
    .unwrap();

    let refused = env.fox(&["install", path_str(&archive)]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("--allow-unsigned-metadata"), "{stderr}");
    assert!(!env.root.path().join("usr/bin/tool").exists());

    let allowed = env.fox(&["install", "--allow-unsigned-metadata", path_str(&archive)]);
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert!(env.root.path().join("usr/bin/tool").is_file());

    let link = env.work.path().join("root-link");
    std::os::unix::fs::symlink(env.root.path(), &link).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fox"))
        .arg("--root")
        .arg(&link)
        .arg("list")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("is a symlink")
    );
}
