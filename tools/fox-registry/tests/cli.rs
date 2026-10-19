//! `fox-registry` end to end: create a registry, admit a package, record a
//! signed release from a local repository, sign as the owner and as CI,
//! check, verify and rotate keys. Tags are signed in Rust with fox-git,
//! since `ssh-keygen` may be missing.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use fox_git::Repository;

const TAGGER: &str = "Fox Test <fox@example.com> 1767323045 +0000";

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Fox Test",
            "-c",
            "user.email=fox@example.com",
        ])
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn owner_key(seed: u8) -> ssh_key::PrivateKey {
    ssh_key::PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]))
}

fn tag(dir: &Path, name: &str, seed: u8) {
    let repo = Repository::open(dir).unwrap();
    let head = repo.resolve("HEAD").unwrap();
    repo.create_signed_tag(name, &head, TAGGER, "release", &owner_key(seed))
        .unwrap();
}

struct Env {
    registry: tempfile::TempDir,
    keys: tempfile::TempDir,
    src: tempfile::TempDir,
    cache: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            registry: tempfile::tempdir().unwrap(),
            keys: tempfile::tempdir().unwrap(),
            src: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
        };
        let src = env.src.path();
        git(src, &["init", "-q", "-b", "main"]);
        fs::write(src.join("README"), "tiny\n").unwrap();
        git(src, &["add", "-A"]);
        git(src, &["commit", "-q", "-m", "first"]);
        let keys = env.keys.path().to_str().unwrap().to_owned();
        let out = env.ok(&["init", "--name", "test", "--keys", &keys]);
        assert!(out.contains("clients pin metadata/root/1.json"), "{out}");
        env
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fox-registry"));
        command.arg("-C").arg(self.registry.path()).args(args);
        if matches!(args.first(), Some(&"release" | &"check")) {
            command.arg("--cache").arg(self.cache.path());
        }
        command.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn key(&self, role: &str) -> String {
        self.keys
            .path()
            .join(format!("{role}.key"))
            .display()
            .to_string()
    }

    /// Runs `sign --complete` with these key files.
    fn sign(&self, keys: &[&str]) -> bool {
        let mut args = vec!["sign", "--complete"];
        for key in keys {
            args.extend(["--key", key]);
        }
        self.run(&args).status.success()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_release_goes_from_tag_to_verified_registry() {
    let env = Env::new();
    env.ok(&["verify"]);

    let owner = owner_key(5);
    let owner_line = owner.public_key().to_openssh().unwrap();
    let owner_path = env.keys.path().join("owner_ed25519");
    fs::write(
        &owner_path,
        owner.to_openssh(ssh_key::LineEnding::LF).unwrap(),
    )
    .unwrap();
    let owner_path = owner_path.display().to_string();
    let url = format!("file://{}", env.src.path().display());
    let owner_arg = format!("me={owner_line}");
    env.ok(&["admit", &url, "--name", "tiny", "--owner", &owner_arg]);
    let (targets, snapshot, timestamp) = (
        env.key("targets"),
        env.key("snapshot"),
        env.key("timestamp"),
    );

    // Admission also needs the owner's signature on the new delegation.
    assert!(!env.sign(&[&targets, &snapshot, &timestamp]));
    assert!(env.sign(&[&owner_path, &snapshot, &timestamp]));
    env.ok(&["verify"]);

    // A tag signed by someone else is refused.
    tag(env.src.path(), "v0.1.0", 6);
    let output = env.run(&["release", "tiny", "v0.1.0"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not an owner"),
        "{}",
        stderr(&output)
    );

    tag(env.src.path(), "v0.2.0", 5);
    let out = env.ok(&["release", "tiny", "v0.2.0"]);
    assert!(out.contains("recorded tiny 0.2.0"), "{out}");
    assert!(env.sign(&[&owner_path, &snapshot, &timestamp]));
    env.ok(&["verify"]);
    assert!(env.ok(&["check"]).contains("tiny 0.2.0: ok"));

    // Moving the tag to another commit fails the check.
    fs::write(env.src.path().join("README"), "changed\n").unwrap();
    git(env.src.path(), &["commit", "-q", "-am", "second"]);
    git(env.src.path(), &["tag", "-d", "v0.2.0"]);
    tag(env.src.path(), "v0.2.0", 5);
    let output = env.run(&["check", "tiny"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("commit"), "{}", stderr(&output));

    env.ok(&["yank", "tiny", "0.2.0"]);
    assert!(env.sign(&[&owner_path, &snapshot, &timestamp]));
    env.ok(&["verify"]);
}

#[test]
fn keys_rotate_through_a_new_root() {
    let env = Env::new();
    let keys = env.keys.path().to_str().unwrap().to_owned();
    let again = env.run(&["init", "--name", "again", "--keys", &keys]);
    assert!(!again.status.success());

    // Keys made for another registry serve as the new targets key.
    let other = tempfile::tempdir().unwrap();
    let other_registry = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fox-registry"))
        .arg("-C")
        .arg(other_registry.path())
        .args(["init", "--name", "other", "--single-key", "--keys"])
        .arg(other.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let new_pub = other.path().join("registry.pub").display().to_string();
    let new_key = other.path().join("registry.key").display().to_string();

    env.ok(&["set-role", "targets", "--key", &new_pub]);
    let (root, snapshot, timestamp) = (env.key("root"), env.key("snapshot"), env.key("timestamp"));
    // The old targets key no longer signs targets.
    assert!(!env.sign(&[&root, &env.key("targets"), &snapshot, &timestamp]));
    assert!(env.sign(&[&root, &new_key, &snapshot, &timestamp]));
    let out = env.ok(&["verify"]);
    assert!(out.contains("root v2"), "{out}");
}
