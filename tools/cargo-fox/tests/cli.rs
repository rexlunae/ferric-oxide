//! `cargo fox keygen`, `build` and `package` on a small git repository.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use fox_core::TrustedKeys;
use fox_core::archive::Archive;
use fox_core::attested::verify_archive_attestation;
use fox_core::keys::load_key_file;

fn cargo_fox(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-fox"))
        .current_dir(dir)
        .arg("fox")
        .args(args)
        .output()
        .unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Fox Test",
            "-c",
            "user.email=fox@example.com",
        ])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn repo(dir: &Path) {
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.3.0\"\nedition = \"2021\"\nlicense = \"MIT\"\n",
    )
    .unwrap();
    fs::create_dir(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    let status = Command::new("cargo")
        .current_dir(dir)
        .args(["generate-lockfile", "--offline"])
        .status()
        .unwrap();
    assert!(status.success());
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "tiny"]);
    git(
        dir,
        &["remote", "add", "origin", "https://example.com/tiny.git"],
    );
}

fn json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn keygen_build_and_package() {
    let src = tempfile::tempdir().unwrap();
    repo(src.path());
    let keys = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let work = work.path().to_str().unwrap();

    let key = json(&cargo_fox(
        src.path(),
        &[
            "keygen",
            keys.path().to_str().unwrap(),
            "--name",
            "ci",
            "--json",
        ],
    ));
    assert_eq!(key["key"].as_str().unwrap().len(), 64);
    // Keys are never overwritten.
    let again = cargo_fox(
        src.path(),
        &["keygen", keys.path().to_str().unwrap(), "--name", "ci"],
    );
    assert!(!again.status.success());

    let built = json(&cargo_fox(
        src.path(),
        &["build", "--json", "--auditable", "off", "--work-dir", work],
    ));
    assert_eq!(built["name"], "tiny");
    assert_eq!(built["version"], "0.3.0");
    assert_eq!(built["files"], 3);

    let out = src.path().join("out");
    let packaged = json(&cargo_fox(
        src.path(),
        &[
            "package",
            "--json",
            "--auditable",
            "off",
            "--work-dir",
            work,
            "--key",
            keys.path().join("ci.toml").to_str().unwrap(),
            "--label",
            "ferric-ci-1",
            "--out",
            out.to_str().unwrap(),
        ],
    ));
    let archive_path = Path::new(packaged["archive"].as_str().unwrap());
    assert!(archive_path.starts_with(&out));
    assert_eq!(packaged["tree_blake3"], built["tree_blake3"]);

    let public = load_key_file(&keys.path().join("ci.pub.toml")).unwrap();
    let mut trusted = TrustedKeys::new();
    trusted.insert(public.label, public.key);
    let archive = Archive::open(archive_path, &trusted).unwrap();
    let verified = verify_archive_attestation(
        &archive,
        Path::new(packaged["attestation"].as_str().unwrap()),
        &trusted,
    )
    .unwrap();
    assert_eq!(verified.attestation().build.builder, "ferric-ci-1");
    assert_eq!(
        verified.attestation().source.git,
        "https://example.com/tiny.git"
    );
}

#[test]
fn reports_errors_with_their_causes() {
    let dir = tempfile::tempdir().unwrap();
    let output = cargo_fox(dir.path(), &["build"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("cargo fox: cannot find Cargo.toml"),
        "{stderr}"
    );
    assert!(stderr.contains("caused by:"), "{stderr}");
}
