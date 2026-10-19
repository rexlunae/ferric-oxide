//! `fox install`, `fox build` and `fox trust` with git sources: signed
//! release tags, trust on first use, local builds and trusted prebuilts.
//! Tags are signed in Rust with fox-git, since `ssh-keygen` may be missing.

use std::path::Path;

mod common;
use common::*;

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
