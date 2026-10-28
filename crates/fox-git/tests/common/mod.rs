//! Helpers shared by the integration tests: repositories made with the
//! `git` command.

#![allow(dead_code)]

pub mod http_server;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

/// Runs git in `dir` with a fixed identity and no signing, so the user's
/// configuration cannot change the result.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Fox Test",
            "-c",
            "user.email=fox@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "core.symlinks=true",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A repository with a file, an executable, a nested file and a symlink.
pub fn sample_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("README"), "hello\n").unwrap();
    fs::create_dir_all(dir.join("src/bin")).unwrap();
    fs::write(dir.join("src/bin/run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        dir.join("src/bin/run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    symlink("README", dir.join("LINK")).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "first"]);
}

/// Runs git in `dir` like [`git`], feeding `input` on standard input.
pub fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git runs");
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A deterministic Ed25519 signing key.
pub fn signing_key(seed: u8) -> ssh_key::PrivateKey {
    ssh_key::PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]))
}

/// The tagger identity tests sign with.
pub const TAGGER: &str = "Fox Test <fox@example.com> 1767323045 +0000";
