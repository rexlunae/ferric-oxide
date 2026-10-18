//! `fox registry`, `fox search`, `fox info` and `fox install <name>`
//! against a local registry repository: builds from an owner-signed tag,
//! prebuilt artifacts attested by the registry's builders, and the policy
//! that chooses between them.

use std::fs;
use std::path::Path;

use fox_core::{Blake3Digest, PackageName};
use fox_git::Repository;
use fox_index::edit::{Keyring, Registry, SignOptions, single_key_roles};
use fox_index::layout::{
    Artifact, ArtifactAttestation, ArtifactList, Builder, PackageEntry, Release,
};
use pkgar_core::dryoc::classic::crypto_sign::crypto_sign_seed_keypair;
use semver::Version;

mod common;
use common::*;

const OWNER: u8 = 5;

fn maintainer() -> [u8; 64] {
    crypto_sign_seed_keypair(&[1; 32]).1
}

fn owner_secret() -> [u8; 64] {
    crypto_sign_seed_keypair(&[OWNER; 32]).1
}

fn tiny() -> PackageName {
    PackageName::new("tiny").unwrap()
}

/// A registry repository holding `tiny`, released at the source's tag
/// `v0.3.0`.
struct Reg {
    dir: tempfile::TempDir,
    blobs: tempfile::TempDir,
}

impl Reg {
    fn new(src: &Path) -> Self {
        let reg = Self {
            dir: tempfile::tempdir().unwrap(),
            blobs: tempfile::tempdir().unwrap(),
        };
        let now = time::OffsetDateTime::now_utc();
        let mut keys = Keyring::new();
        let key = keys.add(maintainer());
        let mut registry =
            Registry::init(reg.dir.path(), "test", &single_key_roles(key), now).unwrap();
        let mut config = registry.config().unwrap();
        config.artifact_base_urls = vec![format!("file://{}", reg.blobs.path().display())];
        registry.set_config(&config).unwrap();
        let owner = signing_key(OWNER).public_key().to_openssh().unwrap();
        let text = format!(
            "name = \"tiny\"\nkind = \"app\"\nsummary = \"A tiny test program\"\n\
             git = \"file://{}\"\n\n[[owners]]\nid = \"me\"\nkey = \"{owner}\"\n",
            src.display()
        );
        let entry = PackageEntry::from_toml(&text, "packages/ti/tiny.toml").unwrap();
        registry.admit(&entry).unwrap();
        let repo = Repository::open(src).unwrap();
        let tag = repo.tag("v0.3.0").unwrap();
        registry
            .add_release(
                &tiny(),
                Release {
                    version: Version::new(0, 3, 0),
                    tag: "v0.3.0".to_owned(),
                    commit: tag.commit().id(),
                    tree_blake3: repo.tree_blake3(tag.commit()).unwrap(),
                    yanked: false,
                },
            )
            .unwrap();
        git(reg.dir.path(), &["init", "-q", "-b", "main"]);
        reg.sign_and_commit();
        reg
    }

    fn sign_and_commit(&self) {
        let mut keys = Keyring::new();
        keys.add(maintainer());
        keys.add(owner_secret());
        let mut registry = Registry::open(self.dir.path()).unwrap();
        let report = registry
            .sign(
                &keys,
                time::OffsetDateTime::now_utc(),
                &SignOptions::default(),
            )
            .unwrap();
        assert!(report.complete(), "{report:?}");
        git(self.dir.path(), &["add", "-A"]);
        git(self.dir.path(), &["commit", "-q", "-m", "update"]);
    }

    fn root_blake3(&self) -> String {
        Blake3Digest::of(&fs::read(self.dir.path().join("metadata/root/1.json")).unwrap())
            .to_string()
    }

    fn url(&self) -> String {
        format!("file://{}", self.dir.path().display())
    }

    /// Stores a blob and returns its hash.
    fn blob(&self, path: &Path) -> Blake3Digest {
        let bytes = fs::read(path).unwrap();
        let hash = Blake3Digest::of(&bytes);
        fs::create_dir_all(self.blobs.path().join("blake3")).unwrap();
        fs::write(self.blobs.path().join(format!("blake3/{hash}")), bytes).unwrap();
        hash
    }

    /// Publishes `fox build --json` output as the release's artifact,
    /// attested by the builder whose public key file is `builder_key`, and
    /// lists that builder in `registry.toml` with `min_builders`.
    fn publish(&self, built: &serde_json::Value, builder_key: &Path, min_builders: u32) {
        let archive = Path::new(built["archive"].as_str().unwrap());
        let attestation = Path::new(built["attestation"].as_str().unwrap());
        let signature = attestation.with_extension("toml.sig");
        let parsed =
            fox_core::Attestation::from_toml(&fs::read_to_string(attestation).unwrap()).unwrap();
        let registry = Registry::open(self.dir.path()).unwrap();
        let mut config = registry.config().unwrap();
        config.builders = vec![Builder {
            label: "ci".to_owned(),
            key: fox_core::keys::load_key_file(builder_key).unwrap().key,
        }];
        config.min_builder_signatures = min_builders;
        registry.set_config(&config).unwrap();
        let mut list = ArtifactList {
            name: tiny(),
            version: Version::new(0, 3, 0),
            artifacts: Vec::new(),
        };
        self.blob(archive);
        list.add_attestation(
            Artifact::from_attestation(&parsed, true, Vec::new()),
            ArtifactAttestation {
                builder: "ci".to_owned(),
                attestation_blake3: self.blob(attestation),
                signature_blake3: self.blob(&signature),
            },
        );
        registry.write_artifacts(&list).unwrap();
        self.sign_and_commit();
    }
}

fn add_registry(env: &Env, reg: &Reg) {
    let out = env.fox(&[
        "registry",
        "add",
        "test",
        &reg.url(),
        "--root-blake3",
        &reg.root_blake3(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn installs_by_name_from_an_owner_signed_tag() {
    let env = Env::new();
    tag(env.src.path(), "v0.3.0", OWNER);
    let reg = Reg::new(env.src.path());

    // A wrong pin is refused before anything is trusted.
    let wrong = Blake3Digest::of(b"other root").to_string();
    fails_with(
        &env.fox(&[
            "registry",
            "add",
            "test",
            &reg.url(),
            "--root-blake3",
            &wrong,
        ]),
        "not the pinned",
    );
    add_registry(&env, &reg);
    let list = json(&env.fox(&["--json", "registry", "list"]));
    assert_eq!(list[0]["name"], "test");

    let found = json(&env.fox(&["--json", "search", "TINY"]));
    assert_eq!(found[0]["name"], "tiny");
    assert_eq!(found[0]["version"], "0.3.0");
    let info = json(&env.fox(&["--json", "info", "test/tiny"]));
    assert_eq!(info["releases"][0]["tag"], "v0.3.0");

    // No artifacts: built from the tag, with no trust-on-first-use prompt
    // because the registry names the owners.
    let installed = json(&env.fox(&["--json", "install", "tiny@^0.3"]));
    assert_eq!(installed[0]["version"], "0.3.0");
    fails_with(&env.fox(&["install", "tiny@^9"]), "no release matching");
    fails_with(&env.fox(&["install", "other"]), "no registry has a package");

    // The policy can forbid building.
    fs::create_dir_all(env.root.path().join("etc/fox")).unwrap();
    fs::write(
        env.root.path().join("etc/fox/policy.toml"),
        "build = \"never\"\n",
    )
    .unwrap();
    fails_with(&env.fox(&["install", "tiny"]), "never to build");

    // A release whose tag moved no longer matches the registry.
    fs::remove_file(env.root.path().join("etc/fox/policy.toml")).unwrap();
    commit_version(env.src.path(), "0.3.0-moved");
    git(env.src.path(), &["tag", "-d", "v0.3.0"]);
    tag(env.src.path(), "v0.3.0", OWNER);
    fails_with(
        &env.fox(&["install", "tiny"]),
        "does not match its repository",
    );

    let removed = env.fox(&["registry", "remove", "test"]);
    assert!(removed.status.success());
    assert!(!env.root.path().join("var/lib/fox/registries/test").exists());
}

#[test]
fn installs_prebuilt_artifacts_attested_by_enough_builders() {
    let env = Env::new();
    tag(env.src.path(), "v0.3.0", OWNER);
    let reg = Reg::new(env.src.path());

    // The registry's CI is another root's local builder.
    let ci = tempfile::tempdir().unwrap();
    let out = env.work.path().join("ci-out");
    let built = json(&env.fox_in(
        ci.path(),
        &[
            "--json",
            "build",
            "--trust-new-key",
            "--out",
            out.to_str().unwrap(),
            &format!("{}#v0.3.0", env.src()),
        ],
    ));
    let ci_key = ci.path().join("var/lib/fox/local-builder.pub.toml");
    reg.publish(&built, &ci_key, 2);
    add_registry(&env, &reg);

    let never = env.root.path().join("etc/fox/policy.toml");
    fs::create_dir_all(never.parent().unwrap()).unwrap();
    fs::write(&never, "build = \"never\"\n").unwrap();
    // The registry asks for two builders and has one.
    fails_with(
        &env.fox(&["install", "tiny"]),
        "1 of the registry's builders",
    );

    reg.publish(&built, &ci_key, 1);
    assert!(env.fox(&["registry", "update"]).status.success());
    let installed = json(&env.fox(&["--json", "install", "tiny"]));
    assert_eq!(installed[0]["version"], "0.3.0");
    let list = json(&env.fox(&["--json", "list"]));
    assert_eq!(list[0]["signer_label"], "ci");
    // Installed files verify against the registry's builder key.
    let verified = env.fox(&["verify", "--installed"]);
    assert!(verified.status.success(), "{}", stderr(&verified));

    // A tampered blob is refused, and with building forbidden nothing
    // else will do.
    let pkgar = built["pkgar_blake3"].as_str().unwrap();
    fs::write(
        reg.blobs.path().join(format!("blake3/{pkgar}")),
        b"tampered",
    )
    .unwrap();
    fails_with(&env.fox(&["install", "tiny"]), "never to build");
}

#[test]
fn audit_reports_advised_packages_and_system_sboms_list_them() {
    let env = Env::new();
    tag(env.src.path(), "v0.3.0", OWNER);
    let reg = Reg::new(env.src.path());
    let advisory = fox_index::advisory::Advisory::parse(
        "[advisory]\nid = \"FOX-2026-0001\"\npackage = \"tiny\"\ndate = \"2026-10-01\"\n\
         title = \"tiny is too small\"\n\n[versions]\npatched = [\">= 0.3.1\"]\n",
    )
    .unwrap();
    Registry::open(reg.dir.path())
        .unwrap()
        .write_advisory(&advisory)
        .unwrap();
    reg.sign_and_commit();
    add_registry(&env, &reg);

    // Nothing installed, nothing found.
    let clean = env.fox(&["audit"]);
    assert!(clean.status.success(), "{}", stderr(&clean));
    let install = env.fox(&["--json", "install", "tiny"]);
    assert!(
        stderr(&install).contains("affected by FOX-2026-0001"),
        "{}",
        stderr(&install)
    );
    assert_eq!(json(&install)[0]["version"], "0.3.0");

    let audit = env.fox(&["--json", "audit"]);
    assert!(!audit.status.success());
    let report: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert_eq!(report["findings"][0]["advisory"], "FOX-2026-0001");
    assert_eq!(report["findings"][0]["source"], "test");
    assert_eq!(report["findings"][0]["patched"][0], ">=0.3.1");
    assert_eq!(report["advisories"], 1);

    let sbom = json(&env.fox(&["sbom", "--system", "--name", "test-image"]));
    assert_eq!(sbom["metadata"]["component"]["name"], "test-image");
    let tiny = &sbom["components"][0];
    assert_eq!(tiny["name"], "tiny");
    assert_eq!(tiny["purl"], "pkg:generic/tiny@0.3.0");
    let nested = tiny["components"].as_array().unwrap();
    assert!(nested.iter().any(|c| c["type"] == "file"));

    // An SBOM changed after installation is not believed.
    let path = env.root.path().join("usr/share/fox/sbom/tiny.cdx.json");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push(' ');
    fs::write(&path, text).unwrap();
    fails_with(&env.fox(&["sbom", "--system"]), "has changed");
    fails_with(&env.fox(&["audit"]), "has changed");
}

#[test]
fn audit_needs_advisories() {
    let env = Env::new();
    fails_with(&env.fox(&["audit"]), "no advisories");
}

#[test]
fn sync_installs_locks_and_removes_a_package_set() {
    let env = Env::new();
    tag(env.src.path(), "v0.3.0", OWNER);
    let reg = Reg::new(env.src.path());
    let set = env.work.path().join("set/packages.toml");
    fs::create_dir_all(set.parent().unwrap()).unwrap();
    let registry = format!(
        "[[registry]]\nname = \"test\"\ngit = \"{}\"\nroot-blake3 = \"{}\"\n\n\
         [policy]\nallow-yanked = false\n\n",
        reg.url(),
        reg.root_blake3()
    );
    let set_arg = set.to_str().unwrap();
    let lock = set.with_file_name("fox.lock");

    fs::write(
        &set,
        format!("{registry}[packages]\ntiny = {{ version = \"^0.3\" }}\n"),
    )
    .unwrap();
    // Nothing locked yet, so --locked refuses.
    fails_with(
        &env.fox(&["sync", "--locked", "--config", set_arg]),
        "not pinned",
    );
    // ...before it adds a registry or writes the policy.
    assert!(!env.root.path().join("etc/fox/policy.toml").exists());
    assert!(
        json(&env.fox(&["--json", "registry", "list"]))
            .as_array()
            .unwrap()
            .is_empty()
    );
    let first = json(&env.fox(&["--json", "sync", "--config", set_arg]));
    assert_eq!(first[0]["action"], "installed");
    assert!(env.root.path().join("etc/fox/policy.toml").exists());
    let text = fs::read_to_string(&lock).unwrap();
    assert!(text.contains("registry = \"test\""), "{text}");
    assert!(text.contains("version = \"0.3.0\""), "{text}");

    // The lock is satisfied and the archive is the locked one.
    let again = json(&env.fox(&["--json", "sync", "--locked", "--config", set_arg]));
    assert_eq!(again[0]["action"], "unchanged");

    // A package the set drops is removed; one installed by hand is not.
    fs::write(&set, format!("{registry}[packages]\n")).unwrap();
    let removed = json(&env.fox(&["--json", "sync", "--config", set_arg]));
    assert_eq!(removed[0]["action"], "removed");
    let list = json(&env.fox(&["--json", "list"]));
    assert!(list.as_array().unwrap().is_empty());
    assert!(!fs::read_to_string(&lock).unwrap().contains("tiny"));

    // A package replaced by hand after the sync is not sync's to remove.
    fs::write(
        &set,
        format!("{registry}[packages]\ntiny = {{ version = \"^0.3\" }}\n"),
    )
    .unwrap();
    assert_eq!(
        json(&env.fox(&["--json", "sync", "--config", set_arg]))[0]["action"],
        "installed"
    );
    fs::write(env.src.path().join("NOTES"), "by hand\n").unwrap();
    git(env.src.path(), &["add", "NOTES"]);
    git(env.src.path(), &["commit", "-q", "-m", "by hand"]);
    git(env.src.path(), &["branch", "by-hand"]);
    let by_hand = env.fox(&[
        "install",
        "--allow-unsigned",
        &format!("{}#by-hand", env.src()),
    ]);
    assert!(by_hand.status.success(), "{}", stderr(&by_hand));
    fs::write(&set, format!("{registry}[packages]\n")).unwrap();
    let kept = json(&env.fox(&["--json", "sync", "--config", set_arg]));
    assert_eq!(kept[0]["action"], "kept");
    assert_eq!(json(&env.fox(&["--json", "list"]))[0]["name"], "tiny");
    assert!(env.fox(&["remove", "tiny"]).status.success());

    // The same package from git is pinned to its commit.
    fs::write(
        &set,
        format!(
            "[packages]\ntiny = {{ git = \"{}\", rev = \"v0.3.0\" }}\n",
            env.src()
        ),
    )
    .unwrap();
    let built = json(&env.fox(&["--json", "sync", "--trust-new-key", "--config", set_arg]));
    assert_eq!(built[0]["action"], "installed");
    let text = fs::read_to_string(&lock).unwrap();
    assert!(text.contains("commit = "), "{text}");
    fails_with(
        &env.fox(&["sync", "--config", set_arg, "--locked", "--update"]),
        "cannot be used with",
    );

    // A moved tag no longer gives the locked commit once the package has
    // to be installed again.
    fs::write(env.src.path().join("README"), "moved\n").unwrap();
    git(env.src.path(), &["add", "README"]);
    git(env.src.path(), &["commit", "-q", "-m", "moved"]);
    git(env.src.path(), &["tag", "-d", "v0.3.0"]);
    tag(env.src.path(), "v0.3.0", OWNER);
    let unchanged = json(&env.fox(&["--json", "sync", "--config", set_arg]));
    assert_eq!(unchanged[0]["action"], "unchanged");
    assert!(env.fox(&["remove", "tiny"]).status.success());
    fails_with(&env.fox(&["sync", "--config", set_arg]), "the lock pins");
    // --update takes the new commit.
    let updated = json(&env.fox(&["--json", "sync", "--update", "--config", set_arg]));
    assert_eq!(updated[0]["action"], "installed");
}
