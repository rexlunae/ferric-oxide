//! Installing, verifying and removing real archives in a temporary root.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use fox_core::archive::Archive;
use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
use fox_core::{Manifest, PackageName, TrustedKeys, VerifyError};
use fox_store::{InstallOptions, ProblemKind, Store, StoreError};

struct Env {
    work: tempfile::TempDir,
    root: tempfile::TempDir,
    key: FixtureKey,
    store: Store,
}

impl Env {
    fn new() -> Self {
        let work = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let key = FixtureKey::generate(work.path(), "test").unwrap();
        let store = Store::open(root.path()).unwrap();
        Self {
            work,
            root,
            key,
            store,
        }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn package(
        &self,
        name: &str,
        version: &str,
        depends: &[&str],
        entries: &[FixtureEntry<'_>],
    ) -> PathBuf {
        let dir = self.work.path().join(format!("{name}-{version}"));
        fs::create_dir_all(&dir).unwrap();
        let mut manifest = Manifest::new(PackageName::new(name).unwrap(), version);
        manifest.depends = depends
            .iter()
            .map(|d| PackageName::new(*d).unwrap())
            .collect();
        build_package(&dir, &self.key, Some(&manifest), entries, true).unwrap()
    }

    fn install(&self, path: &Path, force: bool) -> Result<fox_store::InstallReport, StoreError> {
        let archive = Archive::open(path, &self.key.trusted()).unwrap();
        self.store.install(&archive, InstallOptions { force })
    }
}

fn name(name: &str) -> PackageName {
    PackageName::new(name).unwrap()
}

#[test]
fn installs_lists_verifies_and_removes() {
    let env = Env::new();
    let path = env.package(
        "hello",
        "1.0",
        &[],
        &[
            FixtureEntry::file("usr/bin/hello", b"hello v1\n", 0o755),
            FixtureEntry::symlink("usr/bin/hi", "hello"),
        ],
    );
    let report = env.install(&path, false).unwrap();
    assert_eq!(report.version, "1.0");
    assert_eq!(report.previous, None);
    // Two files plus the manifest.
    assert_eq!(report.installed, 3);

    let bin = env.root().join("usr/bin/hello");
    assert_eq!(fs::read(&bin).unwrap(), b"hello v1\n");
    assert_eq!(
        fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::read_link(env.root().join("usr/bin/hi")).unwrap(),
        Path::new("hello")
    );
    assert!(
        env.root()
            .join("usr/share/fox/manifest/hello.toml")
            .is_file()
    );
    assert!(
        env.root()
            .join("var/lib/fox/heads/hello.pkgar_head")
            .is_file()
    );

    let listed = env.store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, name("hello"));
    assert_eq!(listed[0].signer_label, "test");

    let verify = env
        .store
        .verify(&name("hello"), &env.key.trusted())
        .unwrap();
    assert_eq!(verify.checked, 3);
    assert!(verify.problems.is_empty());

    let removed = env.store.remove(&name("hello"), false).unwrap();
    assert_eq!(removed.removed, 3);
    assert!(!bin.exists());
    assert!(!env.root().join("usr/bin/hi").exists());
    assert!(env.store.list().unwrap().is_empty());
    assert!(
        !env.root()
            .join("var/lib/fox/heads/hello.pkgar_head")
            .exists()
    );
    assert!(matches!(
        env.store.remove(&name("hello"), false),
        Err(StoreError::NotInstalled(_))
    ));
}

#[test]
fn upgrade_replaces_files_and_drops_stale_ones() {
    let env = Env::new();
    let v1 = env.package(
        "hello",
        "1.0",
        &[],
        &[
            FixtureEntry::file("usr/bin/hello", b"v1", 0o755),
            FixtureEntry::file("usr/share/hello/old", b"old", 0o644),
            FixtureEntry::file("usr/share/hello/edited", b"orig", 0o644),
        ],
    );
    let v2 = env.package(
        "hello",
        "2.0",
        &[],
        &[FixtureEntry::file("usr/bin/hello", b"v2", 0o755)],
    );
    env.install(&v1, false).unwrap();
    fs::write(env.root().join("usr/share/hello/edited"), b"local change").unwrap();

    let report = env.install(&v2, false).unwrap();
    assert_eq!(report.previous.as_deref(), Some("1.0"));
    assert_eq!(report.removed, 1);
    assert_eq!(report.kept, vec!["usr/share/hello/edited".to_owned()]);
    assert_eq!(fs::read(env.root().join("usr/bin/hello")).unwrap(), b"v2");
    assert!(!env.root().join("usr/share/hello/old").exists());
    assert!(env.root().join("usr/share/hello/edited").exists());
    let listed = env.store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].version, "2.0");
}

#[test]
fn refuses_files_owned_by_another_package() {
    let env = Env::new();
    let a = env.package(
        "a",
        "1",
        &[],
        &[FixtureEntry::file("usr/bin/tool", b"a", 0o755)],
    );
    let b = env.package(
        "b",
        "1",
        &[],
        &[FixtureEntry::file("usr/bin/tool", b"b", 0o755)],
    );
    env.install(&a, false).unwrap();
    match env.install(&b, true).unwrap_err() {
        StoreError::FileConflict { path, owner } => {
            assert_eq!(path, "usr/bin/tool");
            assert_eq!(owner, name("a"));
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(fs::read(env.root().join("usr/bin/tool")).unwrap(), b"a");
}

#[test]
fn unowned_files_need_force_unless_identical() {
    let env = Env::new();
    let path = env.package(
        "hello",
        "1",
        &[],
        &[FixtureEntry::file("usr/bin/hello", b"new", 0o755)],
    );
    fs::create_dir_all(env.root().join("usr/bin")).unwrap();

    fs::write(env.root().join("usr/bin/hello"), b"new").unwrap();
    env.install(&path, false).unwrap();
    env.store.remove(&name("hello"), false).unwrap();

    fs::write(env.root().join("usr/bin/hello"), b"something else").unwrap();
    assert!(matches!(
        env.install(&path, false).unwrap_err(),
        StoreError::UnownedFile { .. }
    ));
    assert_eq!(
        fs::read(env.root().join("usr/bin/hello")).unwrap(),
        b"something else"
    );
    env.install(&path, true).unwrap();
    assert_eq!(fs::read(env.root().join("usr/bin/hello")).unwrap(), b"new");
}

#[test]
fn never_writes_through_symlinks_in_the_root() {
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(env.root().join("usr")).unwrap();
    symlink(outside.path(), env.root().join("usr/lib")).unwrap();
    let path = env.package(
        "evil",
        "1",
        &[],
        &[FixtureEntry::file("usr/lib/libc.so", b"x", 0o644)],
    );
    assert!(matches!(
        env.install(&path, true).unwrap_err(),
        StoreError::SymlinkInPath { .. }
    ));
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none());

    fs::create_dir_all(env.root().join("usr/bin/hello")).unwrap();
    let dir_in_way = env.package(
        "hello",
        "1",
        &[],
        &[FixtureEntry::file("usr/bin/hello", b"x", 0o755)],
    );
    assert!(matches!(
        env.install(&dir_in_way, true).unwrap_err(),
        StoreError::NotAFile { .. }
    ));
}

#[test]
fn verify_reports_changed_files_and_untrusted_heads() {
    let env = Env::new();
    let path = env.package(
        "hello",
        "1",
        &[],
        &[
            FixtureEntry::file("usr/bin/hello", b"hello", 0o755),
            FixtureEntry::file("usr/bin/other", b"other", 0o755),
            FixtureEntry::symlink("usr/bin/hi", "hello"),
        ],
    );
    env.install(&path, false).unwrap();
    fs::write(env.root().join("usr/bin/hello"), b"tampered").unwrap();
    fs::remove_file(env.root().join("usr/bin/other")).unwrap();
    fs::remove_file(env.root().join("usr/bin/hi")).unwrap();
    fs::write(env.root().join("usr/bin/hi"), b"hello").unwrap();

    let report = env
        .store
        .verify(&name("hello"), &env.key.trusted())
        .unwrap();
    let mut problems: Vec<_> = report
        .problems
        .iter()
        .map(|p| (p.path.as_str(), p.kind))
        .collect();
    problems.sort_by_key(|(path, _)| *path);
    assert_eq!(
        problems,
        vec![
            ("usr/bin/hello", ProblemKind::Modified),
            ("usr/bin/hi", ProblemKind::WrongType),
            ("usr/bin/other", ProblemKind::Missing),
        ]
    );

    let other = FixtureKey::generate(env.work.path(), "other").unwrap();
    assert!(matches!(
        env.store.verify(&name("hello"), &other.trusted()),
        Err(StoreError::Head {
            source: VerifyError::UntrustedSigner { .. },
            ..
        })
    ));
    assert!(matches!(
        env.store.verify(&name("hello"), &TrustedKeys::new()),
        Err(StoreError::Head {
            source: VerifyError::NoTrustedKeys,
            ..
        })
    ));
}

#[test]
fn remove_respects_dependents_and_local_changes() {
    let env = Env::new();
    let lib = env.package(
        "libfoo",
        "1",
        &[],
        &[FixtureEntry::file("usr/lib/libfoo.so", b"lib", 0o644)],
    );
    let app = env.package(
        "app",
        "1",
        &["libfoo", "libbar"],
        &[FixtureEntry::file("usr/bin/app", b"app", 0o755)],
    );
    env.install(&lib, false).unwrap();
    let report = env.install(&app, false).unwrap();
    assert_eq!(report.missing_depends, vec![name("libbar")]);

    match env.store.remove(&name("libfoo"), false).unwrap_err() {
        StoreError::RequiredBy { dependents, .. } => assert_eq!(dependents, vec![name("app")]),
        other => panic!("unexpected error: {other}"),
    }
    assert!(env.root().join("usr/lib/libfoo.so").exists());

    fs::write(env.root().join("usr/lib/libfoo.so"), b"patched").unwrap();
    let removed = env.store.remove(&name("libfoo"), true).unwrap();
    assert_eq!(removed.kept, vec!["usr/lib/libfoo.so".to_owned()]);
    assert!(env.root().join("usr/lib/libfoo.so").exists());
    assert_eq!(env.store.list().unwrap().len(), 1);
}

#[test]
fn open_requires_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Store::open(&dir.path().join("missing")),
        Err(StoreError::RootNotDir { .. })
    ));
}
