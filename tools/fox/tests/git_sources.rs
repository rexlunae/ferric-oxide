//! `fox install`, `fox build` and `fox trust` with git sources: signed
//! release tags, trust on first use, local builds and trusted prebuilts.
//! Tags are signed in Rust with fox-git, since `ssh-keygen` may be missing.

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
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn signing_key(seed: u8) -> ssh_key::PrivateKey {
    ssh_key::PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]))
}

/// Writes the `tiny` package at `version` and commits it.
fn commit_version(dir: &Path, version: &str) {
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

fn tag(dir: &Path, name: &str, seed: u8) {
    let repo = Repository::open(dir).unwrap();
    let head = repo.resolve("HEAD").unwrap();
    repo.create_signed_tag(name, &head, TAGGER, "release", &signing_key(seed))
        .unwrap();
}

/// A source repository, an install root and a build work directory.
struct Env {
    src: tempfile::TempDir,
    root: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            src: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        };
        git(env.src.path(), &["init", "-q", "-b", "main"]);
        commit_version(env.src.path(), "0.3.0");
        env
    }

    fn src(&self) -> &str {
        self.src.path().to_str().unwrap()
    }

    /// Runs fox on `root`, giving `install` and `build` the test's work
    /// directory.
    fn fox_in(&self, root: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fox"));
        command.arg("--root").arg(root).args(args);
        if args.iter().any(|arg| matches!(*arg, "install" | "build")) {
            command.arg("--work-dir").arg(self.work.path());
        }
        command.output().unwrap()
    }

    fn fox(&self, args: &[&str]) -> Output {
        self.fox_in(self.root.path(), args)
    }
}

fn fox_trust(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fox"))
        .arg("--root")
        .arg(root)
        .arg("trust")
        .args(args)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn fails_with(output: &Output, text: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert!(stderr(output).contains(text), "{}", stderr(output));
}

#[test]
fn installs_signed_releases_and_pins_the_author_on_first_use() {
    let env = Env::new();
    tag(env.src.path(), "v0.3.0", 1);

    // Not a terminal and no --trust-new-key: the new key is not accepted.
    fails_with(
        &env.fox(&["install", env.src()]),
        "has no pinned author key",
    );

    let installed = json(&env.fox(&["--json", "install", "--trust-new-key", env.src()]));
    assert_eq!(installed[0]["name"], "tiny");
    assert_eq!(installed[0]["version"], "0.3.0");
    let list = json(&env.fox(&["--json", "list"]));
    assert_eq!(list[0]["signer_label"], "local-builder");
    assert!(env.fox(&["verify", "--installed"]).status.success());

    let author = fox_core::trust::AuthorKey::new(signing_key(1).public_key().clone()).unwrap();
    let trust = json(&fox_trust(env.root.path(), &["--json", "list"]));
    assert_eq!(trust["authors"][0]["fingerprint"], author.fingerprint());
    let url = trust["authors"][0]["git"].as_str().unwrap().to_owned();
    assert!(url.starts_with("file:///"), "{url}");

    // A release signed by another key is refused, flag or not.
    commit_version(env.src.path(), "0.4.0");
    tag(env.src.path(), "v0.4.0", 2);
    fails_with(
        &env.fox(&["install", "--trust-new-key", env.src()]),
        "but the repository is pinned to",
    );
    // Removing the pin lets the new key be pinned.
    assert!(
        fox_trust(env.root.path(), &["remove-author", &url])
            .status
            .success()
    );
    let upgraded = json(&env.fox(&["--json", "install", "--trust-new-key", env.src()]));
    assert_eq!(upgraded[0]["previous"], "0.3.0");
    assert_eq!(upgraded[0]["version"], "0.4.0");
}

#[test]
fn unsigned_and_mismatched_sources_are_refused() {
    let env = Env::new();
    let out = env.work.path().join("out");
    let out = out.to_str().unwrap();
    fails_with(
        &env.fox(&["build", "--out", out, env.src()]),
        "revision HEAD of",
    );
    let built = json(&env.fox(&[
        "--json",
        "build",
        "--allow-unsigned",
        "--out",
        out,
        env.src(),
    ]));
    assert_eq!(built["rev"], "HEAD");
    assert!(built["author_key"].is_null());
    assert!(Path::new(built["archive"].as_str().unwrap()).is_file());

    // A tag whose version is not the package's.
    tag(env.src.path(), "v9.0.0", 1);
    fails_with(
        &env.fox(&["build", "--trust-new-key", "--out", out, env.src()]),
        "names version 9.0.0, but the package's version is 0.3.0",
    );

    // An unsigned annotated tag needs the flag too.
    git(env.src.path(), &["tag", "-a", "v0.3.0", "-m", "unsigned"]);
    let spec = format!("{}#v0.3.0", env.src());
    fails_with(&env.fox(&["build", "--out", out, &spec]), "tag v0.3.0 of");
}

#[test]
fn prebuilt_archives_need_a_trusted_builder_and_the_same_source() {
    let env = Env::new();
    tag(env.src.path(), "v0.3.0", 1);
    let builder_root = tempfile::tempdir().unwrap();
    let out = env.work.path().join("prebuilt");
    let built = json(&env.fox_in(
        builder_root.path(),
        &[
            "--json",
            "build",
            "--trust-new-key",
            "--out",
            out.to_str().unwrap(),
            env.src(),
        ],
    ));
    let archive = built["archive"].as_str().unwrap();

    let install = |extra: &[&str]| {
        let mut args = vec![
            "--json",
            "install",
            "--trust-new-key",
            "--prebuilt",
            archive,
        ];
        args.extend_from_slice(extra);
        env.fox(&args)
    };
    fails_with(&install(&[env.src()]), "no trusted signing keys");

    let builder_key = builder_root
        .path()
        .join("var/lib/fox/local-builder.pub.toml");
    let added = fox_trust(
        env.root.path(),
        &["add-builder", "example-ci", builder_key.to_str().unwrap()],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let installed = json(&install(&[env.src()]));
    assert_eq!(installed[0]["version"], "0.3.0");
    // No local build happened: the root has no builder key of its own.
    assert!(
        !env.root
            .path()
            .join("var/lib/fox/local-builder.toml")
            .exists()
    );

    // The same archive offered for another commit is refused.
    commit_version(env.src.path(), "0.3.1");
    let spec = format!("{}#HEAD", env.src());
    fails_with(
        &install(&["--allow-unsigned", &spec]),
        "was built from a different commit",
    );
    fails_with(
        &install(&[env.src(), env.src()]),
        "--prebuilt needs exactly one git source",
    );
}
