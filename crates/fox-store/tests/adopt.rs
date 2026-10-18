//! Adopting roots that upstream `pkg` populated.

use std::fs;
use std::path::{Path, PathBuf};

use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
use fox_core::{Manifest, PackageName, TrustedKeys};
use fox_store::{
    AdoptOptions, InstallOptions, MetadataKind, PKG_HEADS_DIR, PKG_STATE, ProblemKind, SkipReason,
    Store,
};

struct Env {
    work: tempfile::TempDir,
    root: tempfile::TempDir,
    key: FixtureKey,
    store: Store,
    state: String,
}

impl Env {
    fn new() -> Self {
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let key = FixtureKey::generate(work.path(), "ci").unwrap();
        let store = Store::open(root.path()).unwrap();
        Self {
            work,
            root,
            key,
            store,
            state: String::new(),
        }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn archive(
        &self,
        key: &FixtureKey,
        name: &str,
        signed: bool,
        entries: &[FixtureEntry<'_>],
    ) -> PathBuf {
        let dir = self.work.path().join(format!("pkg-{name}"));
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = Manifest::new(PackageName::new(name).unwrap(), "1.0");
        manifest.target = Some("x86_64-unknown-redox".into());
        build_package(&dir, key, signed.then_some(&manifest), entries, true).unwrap()
    }

    /// Installs `archive` the way `pkg` does: extract it, keep its head and
    /// list it in `etc/pkg/packages.toml`.
    fn pkg_install(&mut self, key: &FixtureKey, name: &str, archive: &Path, deps: &[&str]) {
        pkgar::extract(&key.public_path, archive, self.root()).unwrap();
        let pkey = pkgar_keys::PublicKeyFile::open(&key.public_path)
            .unwrap()
            .pkey;
        let heads = self.root().join(PKG_HEADS_DIR);
        fs::create_dir_all(&heads).unwrap();
        let mut package = pkgar::PackageFile::new(archive, &pkey).unwrap();
        package
            .split(heads.join(format!("{name}.pkgar_head")), None::<&Path>)
            .unwrap();
        signed_modes(self.root(), &heads.join(format!("{name}.pkgar_head")), key);
        let deps: Vec<String> = deps.iter().map(|d| format!("{d:?}")).collect();
        self.state.push_str(&format!(
            "[installed.{name:?}]\nremote = \"local\"\nblake3 = \"\"\nmanual = true\ndependencies = [{}]\n",
            deps.join(", ")
        ));
        let state = self.root().join(PKG_STATE);
        fs::create_dir_all(state.parent().unwrap()).unwrap();
        fs::write(state, &self.state).unwrap();
    }

    fn adopt(&self, keys: &TrustedKeys, allow_unsigned: bool) -> fox_store::AdoptReport {
        let options = AdoptOptions {
            allow_unsigned_metadata: allow_unsigned,
            ..AdoptOptions::default()
        };
        self.store.adopt_pkg(keys, options).unwrap()
    }
}

/// pkgar extracts through the process umask; give the files their signed
/// modes, as an extraction without a umask would, so the tests do not
/// depend on the environment's umask.
fn signed_modes(root: &Path, head: &Path, key: &FixtureKey) {
    use std::os::unix::fs::PermissionsExt;
    let head = fox_core::verify_head(&fs::read(head).unwrap(), &key.trusted()).unwrap();
    for entry in head.entries() {
        if entry.kind == fox_core::FileKind::File {
            let mode = fs::Permissions::from_mode(entry.mode);
            fs::set_permissions(root.join(entry.path), mode).unwrap();
        }
    }
}

fn name(name: &str) -> PackageName {
    PackageName::new(name).unwrap()
}

#[test]
fn adopts_signed_packages_once() {
    let mut env = Env::new();
    let key = FixtureKey::generate(env.work.path(), "build").unwrap();
    let hello = env.archive(
        &key,
        "hello",
        true,
        &[
            FixtureEntry::file("usr/bin/hello", b"hello", 0o755),
            FixtureEntry::symlink("usr/bin/hi", "hello"),
        ],
    );
    env.pkg_install(&key, "hello", &hello, &["relibc"]);

    let report = env.adopt(&key.trusted(), false);
    assert!(!report.already_adopted);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(report.adopted.len(), 1);
    assert_eq!(report.adopted[0].metadata, MetadataKind::Embedded);
    assert_eq!(report.adopted[0].version, "1.0");

    let installed = env.store.get(&name("hello")).unwrap().unwrap();
    assert_eq!(installed.archive_blake3, None);
    assert_eq!(installed.target.as_deref(), Some("x86_64-unknown-redox"));
    assert_eq!(installed.signer_label, "build");
    // Two files plus the manifest, with the installed sizes.
    assert_eq!(installed.files.len(), 3);
    let bin = installed
        .files
        .iter()
        .find(|f| f.path == "usr/bin/hello")
        .unwrap();
    assert_eq!(bin.size, 5);
    let verify = env.store.verify(&name("hello"), &key.trusted()).unwrap();
    assert!(verify.problems.is_empty(), "{:?}", verify.problems);

    // Removed with fox, it is not brought back by pkg's stale state.
    env.store.remove(&name("hello"), false).unwrap();
    assert!(env.adopt(&key.trusted(), true).already_adopted);
    assert!(env.store.list().unwrap().is_empty());
    let again = AdoptOptions {
        again: true,
        ..AdoptOptions::default()
    };
    // Adopting again is possible, but the removal took the signed manifest
    // with it, so pkg's name alone is no longer enough.
    let report = env.store.adopt_pkg(&key.trusted(), again).unwrap();
    assert!(matches!(
        report.skipped[0].reason,
        SkipReason::UnsignedMetadata
    ));
}

#[test]
fn unsigned_names_need_opt_in() {
    let mut env = Env::new();
    let key = FixtureKey::generate(env.work.path(), "build").unwrap();
    let tool = env.archive(
        &key,
        "tool",
        false,
        &[FixtureEntry::file("usr/bin/tool", b"t", 0o755)],
    );
    env.pkg_install(&key, "tool", &tool, &["relibc", "not a name"]);

    let report = env.adopt(&key.trusted(), false);
    assert!(report.adopted.is_empty());
    assert!(matches!(
        report.skipped[0].reason,
        SkipReason::UnsignedMetadata
    ));

    let again = AdoptOptions {
        allow_unsigned_metadata: true,
        again: true,
    };
    let report = env.store.adopt_pkg(&key.trusted(), again).unwrap();
    assert_eq!(report.adopted[0].metadata, MetadataKind::Legacy);
    let installed = env.store.get(&name("tool")).unwrap().unwrap();
    assert_eq!(installed.version, "unknown");
    assert_eq!(installed.depends, vec![name("relibc")]);
}

#[test]
fn changed_manifest_is_not_a_signed_identity() {
    let mut env = Env::new();
    let key = FixtureKey::generate(env.work.path(), "build").unwrap();
    let hello = env.archive(
        &key,
        "hello",
        true,
        &[FixtureEntry::file("usr/bin/hello", b"h", 0o755)],
    );
    env.pkg_install(&key, "hello", &hello, &[]);
    fs::write(
        env.root().join("usr/share/fox/manifest/hello.toml"),
        "format = \"fox-manifest/1\"\nname = \"hello\"\nversion = \"99\"\n",
    )
    .unwrap();
    let report = env.adopt(&key.trusted(), false);
    assert!(matches!(
        report.skipped[0].reason,
        SkipReason::UnsignedMetadata
    ));
}

#[test]
fn skips_untrusted_conflicting_and_reserved_packages() {
    let mut env = Env::new();
    let key = FixtureKey::generate(env.work.path(), "build").unwrap();
    let other = FixtureKey::generate(env.work.path(), "other").unwrap();

    // Already managed by fox: its files and name are taken.
    let owned = env.archive(
        &env.key,
        "owned",
        true,
        &[FixtureEntry::file("usr/bin/owned", b"o", 0o755)],
    );
    let archive = fox_core::archive::Archive::open(&owned, &env.key.trusted()).unwrap();
    env.store
        .install(&archive, InstallOptions::default())
        .unwrap();

    let untrusted = env.archive(
        &other,
        "untrusted",
        true,
        &[FixtureEntry::file("usr/bin/u", b"u", 0o755)],
    );
    env.pkg_install(&other, "untrusted", &untrusted, &[]);
    let clash = env.archive(
        &key,
        "clash",
        true,
        &[FixtureEntry::file("usr/bin/owned", b"o", 0o755)],
    );
    env.pkg_install(&key, "clash", &clash, &[]);
    let keys = env.archive(
        &key,
        "keys",
        true,
        &[FixtureEntry::file("etc/fox/keys/evil.toml", b"k", 0o644)],
    );
    env.pkg_install(&key, "keys", &keys, &[]);
    let fox_key = env.key.clone();
    env.pkg_install(&fox_key, "owned", &owned, &[]);

    let report = env.adopt(&key.trusted(), true);
    assert!(report.adopted.is_empty(), "{:?}", report.adopted);
    let reason = |n: &str| &report.skipped.iter().find(|s| s.name == n).unwrap().reason;
    assert!(matches!(reason("untrusted"), SkipReason::Head(_)));
    assert!(matches!(reason("clash"), SkipReason::FileConflict { .. }));
    assert!(matches!(reason("keys"), SkipReason::ReservedPath { .. }));
    assert!(matches!(reason("owned"), SkipReason::AlreadyInstalled));
    assert_eq!(env.store.list().unwrap().len(), 1);
}

#[test]
fn no_pkg_state_adopts_nothing() {
    let env = Env::new();
    let report = env.adopt(&env.key.trusted(), true);
    assert!(report.adopted.is_empty() && report.skipped.is_empty());
    assert!(env.adopt(&env.key.trusted(), true).already_adopted);
}

#[test]
fn skipped_packages_are_retried_without_reviving_removed_ones() {
    let mut env = Env::new();
    let key = FixtureKey::generate(env.work.path(), "build").unwrap();
    let hello = env.archive(
        &key,
        "hello",
        true,
        &[FixtureEntry::file("usr/bin/hello", b"hello", 0o755)],
    );
    env.pkg_install(&key, "hello", &hello, &[]);
    let late = env.archive(
        &key,
        "late",
        true,
        &[FixtureEntry::file("usr/bin/late", b"late", 0o755)],
    );
    env.pkg_install(&key, "late", &late, &[]);

    // late's head is missing at first, so only hello is adopted.
    let head = env.root().join(PKG_HEADS_DIR).join("late.pkgar_head");
    let aside = env.work.path().join("late.pkgar_head");
    fs::rename(&head, &aside).unwrap();
    let report = env.adopt(&key.trusted(), false);
    assert_eq!(report.adopted.len(), 1);
    assert!(matches!(report.skipped[0].reason, SkipReason::NoHead));

    // While it stays missing, retries keep it pending.
    let report = env.adopt(&key.trusted(), false);
    assert!(!report.already_adopted);
    assert!(report.adopted.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].name, "late");

    // hello is removed with fox; once late's head is back, a plain retry
    // adopts late alone and does not bring hello back.
    env.store.remove(&name("hello"), false).unwrap();
    fs::rename(&aside, &head).unwrap();
    let report = env.adopt(&key.trusted(), false);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(report.adopted.len(), 1);
    assert_eq!(report.adopted[0].name, name("late"));
    let names: Vec<_> = env
        .store
        .list()
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, vec![name("late")]);

    // Nothing is pending any more.
    assert!(env.adopt(&key.trusted(), false).already_adopted);
}

#[test]
fn installing_a_pending_name_retires_it() {
    let mut env = Env::new();
    let hello = env.archive(
        &env.key,
        "hello",
        true,
        &[FixtureEntry::file("usr/bin/hello", b"hello", 0o755)],
    );
    let key = env.key.clone();
    env.pkg_install(&key, "hello", &hello, &[]);
    let head = env.root().join(PKG_HEADS_DIR).join("hello.pkgar_head");
    let aside = env.work.path().join("hello.pkgar_head");
    fs::rename(&head, &aside).unwrap();
    let report = env.adopt(&key.trusted(), true);
    assert!(matches!(report.skipped[0].reason, SkipReason::NoHead));

    // fox takes the name over, then the operator removes it.
    let archive = fox_core::archive::Archive::open(&hello, &key.trusted()).unwrap();
    env.store
        .install(&archive, InstallOptions::default())
        .unwrap();
    env.store.remove(&name("hello"), false).unwrap();

    // pkg's old head coming back does not undo the removal.
    fs::rename(&aside, &head).unwrap();
    assert!(env.adopt(&key.trusted(), true).already_adopted);
    assert!(env.store.list().unwrap().is_empty());
}

#[test]
fn retry_checks_files_left_by_a_removed_owner() {
    let mut env = Env::new();
    let owner = env.archive(
        &env.key,
        "owner",
        true,
        &[FixtureEntry::file("usr/bin/shared", b"s", 0o755)],
    );
    let archive = fox_core::archive::Archive::open(&owner, &env.key.trusted()).unwrap();
    env.store
        .install(&archive, InstallOptions::default())
        .unwrap();
    let bar = env.archive(
        &env.key,
        "bar",
        true,
        &[FixtureEntry::file("usr/bin/shared", b"s", 0o755)],
    );
    let key = env.key.clone();
    env.pkg_install(&key, "bar", &bar, &[]);
    let report = env.adopt(&key.trusted(), false);
    assert!(matches!(
        report.skipped[0].reason,
        SkipReason::FileConflict { .. }
    ));

    // Removing the owner deletes the shared file, so bar's signed entry no
    // longer matches what is installed: it stays pending.
    env.store.remove(&name("owner"), false).unwrap();
    assert!(!env.root().join("usr/bin/shared").exists());
    for _ in 0..2 {
        let report = env.adopt(&key.trusted(), false);
        assert!(report.adopted.is_empty());
        let reason = &report.skipped[0].reason;
        assert!(
            matches!(reason, SkipReason::Changed { path, problem: ProblemKind::Missing } if path == "usr/bin/shared"),
            "{reason:?}"
        );
    }

    // Once the file is back as signed, the retry adopts bar.
    pkgar::extract(&key.public_path, &bar, env.root()).unwrap();
    signed_modes(
        env.root(),
        &env.root().join(PKG_HEADS_DIR).join("bar.pkgar_head"),
        &key,
    );
    let report = env.adopt(&key.trusted(), false);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(report.adopted[0].name, name("bar"));
    assert!(env.adopt(&key.trusted(), false).already_adopted);
}
