//! Resolving, hashing and exporting commits of real repositories made with
//! the `git` command.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use fox_core::Blake3Digest;
use fox_git::{GitError, Repository};

/// Runs git in `dir` with a fixed identity and no signing, so the user's
/// configuration cannot change the result.
fn git(dir: &Path, args: &[&str]) -> String {
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
fn sample_repo(dir: &Path) {
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

/// The `fox-tree/1` encoding written out independently of the crate, for
/// the sample repository.
fn expected_sample_hash() -> Blake3Digest {
    let mut bytes = b"fox-tree/1\0".to_vec();
    let mut add = |kind: u8, path: &str, content: &[u8]| {
        bytes.push(kind);
        bytes.extend_from_slice(&(path.len() as u64).to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.extend_from_slice(&(content.len() as u64).to_le_bytes());
        bytes.extend_from_slice(content);
    };
    // Byte order of full paths: "LINK" < "README" < "src/bin/run.sh" < "src/lib.rs".
    add(b'l', "LINK", b"README");
    add(b'f', "README", b"hello\n");
    add(b'x', "src/bin/run.sh", b"#!/bin/sh\n");
    add(b'f', "src/lib.rs", b"pub fn f() {}\n");
    Blake3Digest::of(&bytes)
}

#[test]
fn hashes_the_tree_in_the_documented_format() {
    let dir = tempfile::tempdir().unwrap();
    sample_repo(dir.path());
    let repo = Repository::discover(&dir.path().join("src")).unwrap();
    let commit = repo.resolve("HEAD").unwrap();
    assert_eq!(commit.id(), git(dir.path(), &["rev-parse", "HEAD"]));
    assert_eq!(commit.committer_time(), 1_767_323_045);
    let hash = repo.tree_blake3(&commit).unwrap();
    assert_eq!(hash, expected_sample_hash());
    // Pinned so a change to the format cannot go unnoticed.
    assert_eq!(
        hash.to_string(),
        "4f2d062696fd9fe55d95df2133a01dbcbbc39afedffcb684ccaebc1913be51fc"
    );
}

#[test]
fn hash_ignores_commit_metadata_but_not_content() {
    let dir = tempfile::tempdir().unwrap();
    sample_repo(dir.path());
    let repo = Repository::open(dir.path()).unwrap();
    let first = repo.resolve("HEAD").unwrap();
    git(
        dir.path(),
        &["commit", "-q", "--allow-empty", "-m", "same tree"],
    );
    let repo = Repository::open(dir.path()).unwrap();
    let second = repo.resolve("HEAD").unwrap();
    assert_ne!(first, second);
    assert_eq!(
        repo.tree_blake3(&first).unwrap(),
        repo.tree_blake3(&second).unwrap()
    );

    fs::set_permissions(
        dir.path().join("src/lib.rs"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    git(dir.path(), &["commit", "-q", "-am", "mode change"]);
    let repo = Repository::open(dir.path()).unwrap();
    let third = repo.resolve("HEAD").unwrap();
    assert_ne!(
        repo.tree_blake3(&second).unwrap(),
        repo.tree_blake3(&third).unwrap()
    );
}

#[test]
fn exports_files_modes_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    sample_repo(dir.path());
    // Uncommitted changes are not exported.
    fs::write(dir.path().join("README"), "changed\n").unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.resolve("main").unwrap();
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("src");
    let export = repo.export(&commit, &dest).unwrap();
    assert_eq!(export.entries, 4);
    assert_eq!(export.tree_blake3, expected_sample_hash());
    assert_eq!(fs::read(dest.join("README")).unwrap(), b"hello\n");
    assert!(!dest.join(".git").exists());
    let mode = |p: &str| fs::metadata(dest.join(p)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("src/bin/run.sh"), 0o755);
    assert_eq!(mode("src/lib.rs"), 0o644);
    assert_eq!(
        fs::read_link(dest.join("LINK")).unwrap(),
        Path::new("README")
    );

    assert!(matches!(
        repo.export(&commit, &dest),
        Err(GitError::ExportNotEmpty { .. })
    ));
}

#[test]
fn resolves_annotated_tags_and_origin() {
    let dir = tempfile::tempdir().unwrap();
    sample_repo(dir.path());
    git(dir.path(), &["tag", "-a", "v1.0.0", "-m", "release"]);
    git(
        dir.path(),
        &["remote", "add", "origin", "https://example.com/sample.git"],
    );
    let repo = Repository::open(dir.path()).unwrap();
    let tagged = repo.resolve("v1.0.0").unwrap();
    assert_eq!(tagged, repo.resolve("HEAD").unwrap());
    assert_eq!(
        repo.origin_url().as_deref(),
        Some("https://example.com/sample.git")
    );
    assert!(matches!(
        repo.resolve("no-such-branch"),
        Err(GitError::Resolve { .. })
    ));
}

#[test]
fn refuses_unsafe_tree_entries() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let blob = git(dir.path(), &["hash-object", "-w", "--stdin"]);
    // `git mktree` takes the entry list on stdin; build a tree holding a
    // `..` entry, which git itself would never write from a work tree.
    let mut child = Command::new("git")
        .current_dir(dir.path())
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    writeln!(child.stdin.take().unwrap(), "100644 blob {blob}\t..").unwrap();
    let output = child.wait_with_output().unwrap();
    let tree = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    if !output.status.success() || tree.is_empty() {
        // This git refuses to write such a tree, which is as good.
        return;
    }
    let commit = git(dir.path(), &["commit-tree", &tree, "-m", "evil"]);
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.resolve(&commit).unwrap();
    assert!(matches!(
        repo.tree_blake3(&commit),
        Err(GitError::UnsafePath { .. })
    ));
    let out = tempfile::tempdir().unwrap();
    assert!(matches!(
        repo.export(&commit, &out.path().join("x")),
        Err(GitError::UnsafePath { .. })
    ));
}
