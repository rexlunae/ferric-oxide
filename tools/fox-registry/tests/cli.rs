//! `fox-registry` end to end: create a registry, admit a package, record a
//! signed release from a local repository, sign as the owner and as CI,
//! check, verify and rotate keys, and build artifacts as a registry builder. Tags are signed in Rust with fox-git,
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
        if matches!(args.first(), Some(&"release" | &"check" | &"build")) {
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
        stderr(&output).contains("may not sign"),
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

/// Makes the source repository a buildable crate at `version`.
fn commit_crate(dir: &Path, version: &str) {
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

#[test]
fn init_github_writes_the_repository_template() {
    let registry = tempfile::tempdir().unwrap();
    let keys = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fox-registry"))
        .arg("-C")
        .arg(registry.path())
        .args(["init", "--name", "gh", "--github", "--keys"])
        .arg(keys.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    for file in ["check", "sign", "timestamp"] {
        let path = registry
            .path()
            .join(format!(".github/workflows/{file}.yml"));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("fox-registry"), "{}", path.display());
    }
    let readme = fs::read_to_string(registry.path().join("README.md")).unwrap();
    assert!(readme.contains("BUILDER_KEY"));
}

#[test]
fn a_builder_records_attested_artifacts() {
    let env = Env::new();
    commit_crate(env.src.path(), "0.1.0");
    let owner = owner_key(5);
    let owner_path = env.keys.path().join("owner_ed25519");
    fs::write(
        &owner_path,
        owner.to_openssh(ssh_key::LineEnding::LF).unwrap(),
    )
    .unwrap();
    let owner_path = owner_path.display().to_string();
    let url = format!("file://{}", env.src.path().display());
    let owner_arg = format!("me={}", owner.public_key().to_openssh().unwrap());
    env.ok(&["admit", &url, "--name", "tiny", "--owner", &owner_arg]);
    tag(env.src.path(), "v0.1.0", 5);
    env.ok(&["release", "tiny", "v0.1.0"]);
    let (targets, snapshot, timestamp) = (
        env.key("targets"),
        env.key("snapshot"),
        env.key("timestamp"),
    );
    assert!(env.sign(&[&owner_path, &targets, &snapshot, &timestamp]));

    // A `pkgar-keys` pair from another registry serves as the builder key.
    let builder = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fox-registry"))
        .arg("-C")
        .arg(tempfile::tempdir().unwrap().path())
        .args(["init", "--name", "b", "--single-key", "--keys"])
        .arg(builder.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let builder_key = builder.path().join("registry.key").display().to_string();
    let builder_pub = builder.path().join("registry.pub").display().to_string();
    let blobs = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let blobs_arg = blobs.path().to_str().unwrap();
    let work_arg = work.path().to_str().unwrap();
    let build = [
        "build",
        "--key",
        &builder_key,
        "--label",
        "ci",
        "--blobs",
        blobs_arg,
        "--work-dir",
        work_arg,
    ];

    // Only builders listed in registry.toml may record artifacts.
    let output = env.run(&build);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not listed"),
        "{}",
        stderr(&output)
    );
    let output = env.run(&["configure", "--remove-builder", "ci"]);
    assert!(
        stderr(&output).contains("no builder ci"),
        "{}",
        stderr(&output)
    );
    let builder_arg = format!("ci={builder_pub}");
    let blob_url = format!("file://{blobs_arg}");
    env.ok(&[
        "configure",
        "--builder",
        &builder_arg,
        "--artifact-url",
        &blob_url,
    ]);

    let out = env.ok(&build);
    assert!(out.contains("built tiny 0.1.0"), "{out}");
    // The archive, the attestation and its signature.
    assert_eq!(
        fs::read_dir(blobs.path().join("blake3")).unwrap().count(),
        3
    );
    let artifacts =
        fs::read_to_string(env.registry.path().join("artifacts/ti/tiny/0.1.0.toml")).unwrap();
    assert!(artifacts.contains("builder = \"ci\""), "{artifacts}");
    assert!(env.sign(&[&targets, &snapshot, &timestamp]));
    env.ok(&["verify"]);

    // Nothing is left to build.
    assert!(!env.ok(&build).contains("built"));

    // An independent rebuilder, in another work directory, reproduces the
    // archive and adds its own attestation to the same artifact.
    let rebuilder = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fox-registry"))
        .arg("-C")
        .arg(tempfile::tempdir().unwrap().path())
        .args(["init", "--name", "r", "--single-key", "--keys"])
        .arg(rebuilder.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let rebuilder_key = rebuilder.path().join("registry.key").display().to_string();
    let rebuilder_pub = rebuilder.path().join("registry.pub").display().to_string();
    let rework = tempfile::tempdir().unwrap();
    let rebuild = [
        "rebuild",
        "--key",
        &rebuilder_key,
        "--label",
        "rebuilder",
        "--blobs",
        blobs_arg,
        "--work-dir",
        rework.path().to_str().unwrap(),
    ];
    let output = env.run(&rebuild);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not listed"),
        "{}",
        stderr(&output)
    );
    let rebuilder_arg = format!("rebuilder={rebuilder_pub}");
    env.ok(&["configure", "--builder", &rebuilder_arg]);
    let out = env.ok(&rebuild);
    assert!(out.contains("reproduced tiny 0.1.0"), "{out}");
    let artifacts =
        fs::read_to_string(env.registry.path().join("artifacts/ti/tiny/0.1.0.toml")).unwrap();
    assert_eq!(artifacts.matches("[[artifact]]").count(), 1, "{artifacts}");
    assert!(artifacts.contains("builder = \"rebuilder\""), "{artifacts}");
    assert!(env.sign(&[&targets, &snapshot, &timestamp]));
    env.ok(&["verify"]);
    // Nothing is left to rebuild, and naming the release again fails.
    assert!(!env.ok(&rebuild).contains("reproduced"));
    let mut again: Vec<&str> = rebuild.to_vec();
    again.push("tiny@0.1.0");
    let output = env.run(&again);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no artifact of tiny 0.1.0"),
        "{}",
        stderr(&output)
    );

    // Replacing the rebuilder's key makes its attestation stale: clients
    // would no longer count it, so the rebuilder attests again.
    let rotated = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fox-registry"))
        .arg("-C")
        .arg(tempfile::tempdir().unwrap().path())
        .args(["init", "--name", "r2", "--single-key", "--keys"])
        .arg(rotated.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let rotated_key = rotated.path().join("registry.key").display().to_string();
    let rotated_arg = format!(
        "rebuilder={}",
        rotated.path().join("registry.pub").display()
    );
    env.ok(&["configure", "--builder", &rotated_arg]);
    let mut rotated_rebuild: Vec<&str> = rebuild.to_vec();
    rotated_rebuild[2] = &rotated_key;
    let out = env.ok(&rotated_rebuild);
    assert!(out.contains("reproduced tiny 0.1.0"), "{out}");
    let artifacts =
        fs::read_to_string(env.registry.path().join("artifacts/ti/tiny/0.1.0.toml")).unwrap();
    assert_eq!(
        artifacts.matches("builder = \"rebuilder\"").count(),
        1,
        "{artifacts}"
    );
    assert!(env.sign(&[&targets, &snapshot, &timestamp]));
    env.ok(&["verify"]);
    assert!(!env.ok(&rotated_rebuild).contains("reproduced"));

    // Each x86-64 level is its own artifact.
    if cfg!(target_arch = "x86_64") {
        let v4: Vec<&str> = build.iter().copied().chain(["--level", "v4"]).collect();
        let out = env.ok(&v4);
        assert!(out.contains("built tiny 0.1.0"), "{out}");
        let artifacts =
            fs::read_to_string(env.registry.path().join("artifacts/ti/tiny/0.1.0.toml")).unwrap();
        assert!(artifacts.contains("\"x86_64-v4\""), "{artifacts}");
        assert!(!env.ok(&v4).contains("built"));
        assert!(!env.ok(&build).contains("built"));
    }
}

#[test]
fn advisories_are_published_and_signed() {
    let env = Env::new();
    let file = env.keys.path().join("advisory.toml");
    fs::write(
        &file,
        "[advisory]\nid = \"FOX-2026-0001\"\npackage = \"tiny\"\ndate = \"2026-10-01\"\n\
         title = \"tiny is too small\"\n\n[versions]\npatched = [\">= 0.2.0\"]\n",
    )
    .unwrap();
    let out = env.ok(&["advise", file.to_str().unwrap()]);
    assert!(out.contains("advisories/FOX-2026-0001.toml"), "{out}");
    assert!(!env.sign(&[&env.key("snapshot"), &env.key("timestamp")]));
    assert!(env.sign(&[
        &env.key("targets"),
        &env.key("snapshot"),
        &env.key("timestamp")
    ]));
    env.ok(&["verify"]);
    let stored =
        fs::read_to_string(env.registry.path().join("advisories/FOX-2026-0001.toml")).unwrap();
    assert!(stored.contains("tiny is too small"));

    fs::write(
        &file,
        "[advisory]\nid = \"../escape\"\npackage = \"tiny\"\ndate = \"2026-10-01\"\n",
    )
    .unwrap();
    let output = env.run(&["advise", file.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("may only hold"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn cookbook_recipes_become_recipe_packages() {
    let env = Env::new();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);
    for (dir, text) in [
        (
            "recipes/core/hello",
            "[source]\ngit = \"https://example.com/hello.git\"\n\n[build]\ntemplate = \"cargo\"\n",
        ),
        (
            "recipes/libs/zlib",
            "[source]\ntar = \"https://example.com/zlib.tar.gz\"\n\n[package]\ndescription = \"Compression library\"\n",
        ),
        ("recipes/libs/bad@name", "[build]\ntemplate = \"custom\"\n"),
    ] {
        fs::create_dir_all(repo.path().join(dir)).unwrap();
        fs::write(repo.path().join(dir).join("recipe.toml"), text).unwrap();
    }
    let owner = owner_key(5);
    let owner_path = env.keys.path().join("owner_ed25519");
    fs::write(
        &owner_path,
        owner.to_openssh(ssh_key::LineEnding::LF).unwrap(),
    )
    .unwrap();
    let owner_arg = format!("me={}", owner.public_key().to_openssh().unwrap());
    let recipes = repo.path().join("recipes");
    let import = [
        "import-recipes",
        recipes.to_str().unwrap(),
        "--git",
        "https://example.com/ferric-oxide.git",
        "--owner",
        &owner_arg,
    ];
    let output = env.run(&import);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("admitted 2 recipes (0 already"), "{out}");
    assert!(stderr(&output).contains("bad@name"), "{}", stderr(&output));

    let hello = fs::read_to_string(env.registry.path().join("packages/he/hello.toml")).unwrap();
    assert!(hello.contains("kind = \"recipe\""), "{hello}");
    assert!(hello.contains("path = \"recipes/core/hello\""), "{hello}");
    assert!(
        hello.contains("Cookbook recipe for https://example.com/hello.git"),
        "{hello}"
    );
    let zlib = fs::read_to_string(env.registry.path().join("packages/zl/zlib.toml")).unwrap();
    assert!(zlib.contains("Compression library"), "{zlib}");

    let owner_path = owner_path.display().to_string();
    assert!(env.sign(&[
        &owner_path,
        &env.key("targets"),
        &env.key("snapshot"),
        &env.key("timestamp")
    ]));
    env.ok(&["verify"]);
    // Without releases there is nothing to check against the (unreachable)
    // repository, so check passes without fetching it.
    let checked = env.ok(&["check"]);
    assert!(checked.contains("hello: no releases"), "{checked}");
    // Importing again admits nothing new.
    assert!(env.ok(&import).contains("admitted 0 recipes (2 already"));
}
