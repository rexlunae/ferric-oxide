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

impl Env {
    /// A cookbook-style repository directory: `<name>.pkgar` per package.
    fn repo_package(&self, name: &str, depends: &[&str]) -> PathBuf {
        let dir = self.work.path().join("repo");
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = Manifest::new(PackageName::new(name).unwrap(), "1.0.0");
        manifest.depends = depends
            .iter()
            .map(|dep| PackageName::new(*dep).unwrap())
            .collect();
        let file = format!("usr/share/{name}");
        let entries = [FixtureEntry::file(&file, name.as_bytes(), 0o644)];
        build_package(&dir, &self.key, Some(&manifest), &entries, false).unwrap();
        dir
    }
}

#[test]
fn install_takes_names_and_dependencies_from_repositories() {
    let env = Env::new();
    env.trust_in_root();
    env.repo_package("libc", &[]);
    env.repo_package("libgui", &["libc"]);
    let repo = env.repo_package("app", &["libgui", "libc", "absent"]);

    let output = env.fox(&["--json", "install", "--repo", path_str(&repo), "app"]);
    let installed = json(&output);
    let order: Vec<_> = installed
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["libc", "libgui", "app"]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("app needs absent"), "{stderr}");
    assert!(env.root.path().join("usr/share/libgui").is_file());
}

#[test]
fn install_no_deps_installs_only_what_is_named() {
    let env = Env::new();
    env.trust_in_root();
    env.repo_package("lib", &[]);
    let repo = env.repo_package("app", &["lib"]);

    let output = env.fox(&["install", "--no-deps", "--repo", path_str(&repo), "app"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(env.root.path().join("usr/share/app").is_file());
    assert!(!env.root.path().join("usr/share/lib").exists());
}

#[test]
fn install_expands_meta_packages_from_repositories() {
    let env = Env::new();
    env.trust_in_root();
    env.repo_package("lib", &[]);
    let repo = env.repo_package("tool", &["lib"]);
    fs::write(
        repo.join("group.toml"),
        "name = \"group\"\ntarget = \"x86_64-unknown-redox\"\ndepends = [\"tool\"]\n",
    )
    .unwrap();

    let output = env.fox(&["install", "--repo", path_str(&repo), "group"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout(&output).contains("meta-package group"));
    assert!(env.root.path().join("usr/share/tool").is_file());
    assert!(env.root.path().join("usr/share/lib").is_file());

    let no_deps = env.fox(&["install", "--no-deps", "--repo", path_str(&repo), "group"]);
    assert!(!no_deps.status.success());
    let unknown = env.fox(&["install", "--repo", path_str(&repo), "absent"]);
    assert!(!unknown.status.success());
}

#[test]
fn install_refuses_untrusted_dependencies() {
    let env = Env::new();
    env.trust_in_root();
    let repo = env.repo_package("app", &["lib"]);
    let other = FixtureKey::generate(env.work.path(), "other").unwrap();
    let manifest = Manifest::new(PackageName::new("lib").unwrap(), "1");
    build_package(&repo, &other, Some(&manifest), &[], false).unwrap();

    let output = env.fox(&["install", "--repo", path_str(&repo), "app"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("cannot get lib, needed by app"), "{stderr}");
    assert!(!env.root.path().join("usr/share/app").exists());
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

#[test]
fn adopt_pkg_takes_over_a_pkg_root_once() {
    let env = Env::new();
    env.trust_in_root();
    let archive = env.package("hello", "1.0.0");
    let root = env.root.path();
    // What the installer's pkg library leaves behind.
    pkgar::extract(&env.key.public_path, &archive, root).unwrap();
    let heads = root.join("var/lib/packages");
    fs::create_dir_all(&heads).unwrap();
    let pkey = pkgar_keys::PublicKeyFile::open(&env.key.public_path)
        .unwrap()
        .pkey;
    pkgar::PackageFile::new(&archive, &pkey)
        .unwrap()
        .split(heads.join("hello.pkgar_head"), None::<&Path>)
        .unwrap();
    // pkgar extracts through the umask; set the signed modes so the test
    // does not depend on the environment's umask.
    let head_bytes = fs::read(heads.join("hello.pkgar_head")).unwrap();
    let head = fox_core::verify_head(&head_bytes, &env.key.trusted()).unwrap();
    for entry in head.entries() {
        if entry.kind == fox_core::FileKind::File {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::Permissions::from_mode(entry.mode);
            fs::set_permissions(root.join(entry.path), mode).unwrap();
        }
    }
    fs::create_dir_all(root.join("etc/pkg")).unwrap();
    fs::write(
        root.join("etc/pkg/packages.toml"),
        "[installed.hello]\nremote = \"local\"\nblake3 = \"\"\n",
    )
    .unwrap();

    let adopted = env.fox(&["adopt-pkg"]);
    assert!(adopted.status.success(), "{}", stdout(&adopted));
    assert!(stdout(&adopted).contains("adopted hello 1.0.0 (2 files, embedded)"));
    let listed = env.fox(&["list"]);
    assert_eq!(
        stdout(&listed),
        "hello 1.0.0 (2 files, signed by ferric-ci-1)\n"
    );
    let json = json(&env.fox(&["--json", "list"]));
    assert_eq!(json[0]["metadata"], "embedded");
    assert_eq!(json[0]["archive_blake3"], serde_json::Value::Null);
    assert!(env.fox(&["verify", "--installed"]).status.success());
    assert!(stdout(&env.fox(&["adopt-pkg"])).contains("already adopted"));
}

/// Writes a signed attestation for `archive` beside it, as a builder would.
fn attest(env: &Env, archive: &Path) {
    use fox_core::archive::Archive;
    use fox_core::attested::{artifact_facts, attestation_path, signature_path};
    use fox_core::{
        Attestation, AttestationFormat, AttestationSignature, AttestedPackage, Blake3Digest,
        BuildRecord, OutputRecord, Profile, SourceRecord, feature_set_hash, sbom_path,
    };

    let opened = Archive::open(archive, &env.key.trusted()).unwrap();
    let facts = artifact_facts(&opened);
    let sbom = facts
        .files
        .iter()
        .find(|f| f.path == sbom_path(facts.name))
        .unwrap()
        .blake3;
    let lock = Blake3Digest::of(b"Cargo.lock");
    let attestation = Attestation {
        format: AttestationFormat::V1,
        package: AttestedPackage {
            name: facts.name.clone(),
            version: facts.version.to_owned(),
        },
        source: SourceRecord {
            git: "https://example.com/attested.git".into(),
            commit: "ab".repeat(20),
            tree_blake3: Blake3Digest::of(b"tree"),
            cargo_lock_blake3: lock,
            path: None,
        },
        build: BuildRecord {
            target: "x86_64-unknown-redox".into(),
            cpu_level: Some(fox_core::CpuLevel::X86_64V3),
            features: vec!["default".into()],
            feature_set_hash: feature_set_hash(&["default"], &lock),
            toolchain_id: "rust-1.93.0:00".into(),
            profile: Profile::Release,
            auditable: false,
            builder: "ferric-ci-1".into(),
            builder_key: env.key.key,
            built_at: "2026-10-17T02:00:00Z".parse().unwrap(),
        },
        output: OutputRecord {
            pkgar_blake3: facts.pkgar_blake3,
            pkgar_size: facts.pkgar_size,
            pkgar_header_signer: facts.header_signer,
            entries_blake3: facts.entries_blake3,
            sbom_blake3: sbom,
        },
    };
    let text = attestation.to_toml().unwrap();
    let secret = pkgar_keys::SecretKeyFile::open(&env.key.secret_path)
        .unwrap()
        .secret_key()
        .unwrap();
    let signature = AttestationSignature::sign(text.as_bytes(), &secret).unwrap();
    let path = attestation_path(archive);
    fs::write(&path, text).unwrap();
    fs::write(signature_path(&path), signature.to_toml().unwrap()).unwrap();
}

#[test]
fn verify_checks_build_attestations() {
    let env = Env::new();
    env.trust_in_root();
    let dir = env.work.path().join("attested");
    fs::create_dir_all(&dir).unwrap();
    let manifest = Manifest::new(PackageName::new("attested").unwrap(), "1.0.0");
    let entries = [
        FixtureEntry::file("usr/bin/attested", b"bin", 0o755),
        FixtureEntry::file("usr/share/fox/sbom/attested.cdx.json", b"{}", 0o644),
    ];
    let archive = build_package(&dir, &env.key, Some(&manifest), &entries, false).unwrap();
    let archive_arg = path_str(&archive);

    // Without an attestation, plain verify passes and the strict one fails.
    assert!(env.fox(&["verify", archive_arg]).status.success());
    let strict = env.fox(&["verify", "--require-attestation", archive_arg]);
    assert!(!strict.status.success());
    assert!(stdout(&strict).contains("no build attestation"));

    attest(&env, &archive);
    let ok = env.fox(&["verify", "--require-attestation", archive_arg]);
    assert!(ok.status.success(), "{}", stdout(&ok));
    assert!(stdout(&ok).contains("built from abab"));
    let report = json(&env.fox(&["--json", "verify", archive_arg]));
    assert_eq!(report[0]["attestation"]["builder"], "ferric-ci-1");
    assert_eq!(report[0]["attestation"]["cpu_level"], "x86_64-v3");

    // A tampered attestation fails even without --require-attestation.
    let attestation = archive.with_extension("attest.toml");
    let text = fs::read_to_string(&attestation).unwrap();
    fs::write(&attestation, text.replace("1.0.0", "1.0.1")).unwrap();
    let tampered = env.fox(&["verify", archive_arg]);
    assert!(!tampered.status.success());
    assert!(stdout(&tampered).contains("bad attestation signature"));
}
