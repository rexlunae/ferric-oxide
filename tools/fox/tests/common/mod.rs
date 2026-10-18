//! Helpers shared by the `fox` integration tests: git repositories with
//! signed tags, an install root and a work directory.
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use fox_git::Repository;

pub const TAGGER: &str = "Fox Test <fox@example.com> 1767323045 +0000";

pub fn git(dir: &Path, args: &[&str]) {
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

pub fn signing_key(seed: u8) -> ssh_key::PrivateKey {
    ssh_key::PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]))
}

/// Writes the `tiny` package at `version` and commits it.
pub fn commit_version(dir: &Path, version: &str) {
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"tiny\"\nversion = \"{version}\"\nedition = \"2021\"\nlicense = \"MIT\"\n"
        ),
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    let status = Command::new("cargo")
        .current_dir(dir)
        .args(["generate-lockfile", "--offline"])
        .status()
        .unwrap();
    assert!(status.success());
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", version]);
}

pub fn tag(dir: &Path, name: &str, seed: u8) {
    let repo = Repository::open(dir).unwrap();
    let head = repo.resolve("HEAD").unwrap();
    repo.create_signed_tag(name, &head, TAGGER, "release", &signing_key(seed))
        .unwrap();
}

/// A source repository, an install root and a build work directory.
pub struct Env {
    pub src: tempfile::TempDir,
    pub root: tempfile::TempDir,
    pub work: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Self {
        let env = Self {
            src: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        };
        git(env.src.path(), &["init", "-q", "-b", "main"]);
        commit_version(env.src.path(), "0.3.0");
        env
    }

    pub fn src(&self) -> &str {
        self.src.path().to_str().unwrap()
    }

    /// Runs fox on `root`, giving `install`, `build` and `sync` the test's work
    /// directory.
    pub fn fox_in(&self, root: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fox"));
        command.arg("--root").arg(root).args(args);
        if args
            .iter()
            .any(|arg| matches!(*arg, "install" | "build" | "sync"))
        {
            command.arg("--work-dir").arg(self.work.path());
        }
        command.output().unwrap()
    }

    pub fn fox(&self, args: &[&str]) -> Output {
        self.fox_in(self.root.path(), args)
    }
}

pub fn fox_trust(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fox"))
        .arg("--root")
        .arg(root)
        .arg("trust")
        .args(args)
        .output()
        .unwrap()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn json(output: &Output) -> serde_json::Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_slice(&output.stdout).unwrap()
}

pub fn fails_with(output: &Output, text: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert!(stderr(output).contains(text), "{}", stderr(output));
}
