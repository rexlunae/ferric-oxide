//! Installing, verifying and removing real archives in a temporary root.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use fox_core::archive::Archive;
use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
use fox_core::{Blake3Digest, Manifest, PackageName, TrustedKeys, VerifyError};
use fox_store::{
    InstallOptions, MetadataKind, Origin, PackageState, ProblemKind, Store, StoreError,
};

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
        let options = InstallOptions {
            force,
            ..InstallOptions::default()
        };
        self.install_with(path, options)
    }

    fn install_with(
        &self,
        path: &Path,
        options: InstallOptions,
    ) -> Result<fox_store::InstallReport, StoreError> {
        let archive = Archive::open(path, &self.key.trusted()).unwrap();
        self.store.install(&archive, options)
    }

    /// A manifest-free archive with a cookbook metadata file claiming `name`.
    fn sidecar_package(&self, dir: &str, name: &str, entries: &[FixtureEntry<'_>]) -> PathBuf {
        let dir = self.work.path().join(dir);
        fs::create_dir_all(&dir).unwrap();
        let path = build_package(&dir, &self.key, None, entries, false).unwrap();
        // The cookbook writes the signed entry-table hash, header bytes 96..128.
        let bytes = fs::read(&path).unwrap();
        let mut hash = [0; 32];
        hash.copy_from_slice(&bytes[96..128]);
        let blake3 = Blake3Digest::from_bytes(hash);
        fs::write(
            path.with_extension("toml"),
            format!("name = \"{name}\"\nversion = \"9\"\ntarget = \"x\"\nblake3 = \"{blake3}\"\ndepends = []\n"),
        )
        .unwrap();
        path
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
fn records_the_registry_a_package_came_from_until_it_is_replaced() {
    let env = Env::new();
    let v1 = env.package(
        "hello",
        "1.0",
        &[],
        &[FixtureEntry::file("usr/bin/hello", b"v1", 0o755)],
    );
    let v2 = env.package(
        "hello",
        "2.0",
        &[],
        &[FixtureEntry::file("usr/bin/hello", b"v2", 0o755)],
    );
    let origin = Origin {
        registry: name("ferric"),
        root_blake3: Blake3Digest::of(b"root"),
    };
    let archive = Archive::open(&v1, &env.key.trusted()).unwrap();
    env.store
        .install_from(&archive, InstallOptions::default(), Some(origin.clone()))
        .unwrap();
    assert_eq!(
        env.store.get(&name("hello")).unwrap().unwrap().origin,
        Some(origin.clone())
    );
    // An archive from anywhere else is not that registry's package.
    env.install(&v2, false).unwrap();
    assert_eq!(env.store.get(&name("hello")).unwrap().unwrap().origin, None);

    // An origin is restored only for the archive that is installed, and
    // never over one already recorded.
    let v2_blake3 = env
        .store
        .get(&name("hello"))
        .unwrap()
        .unwrap()
        .archive_blake3
        .unwrap();
    let other = Origin {
        registry: name("other"),
        ..origin.clone()
    };
    assert!(
        !env.store
            .restore_origin(&name("hello"), Blake3Digest::of(b"v1"), origin.clone())
            .unwrap()
    );
    assert!(
        !env.store
            .restore_origin(&name("absent"), v2_blake3, origin.clone())
            .unwrap()
    );
    assert!(
        env.store
            .restore_origin(&name("hello"), v2_blake3, origin.clone())
            .unwrap()
    );
    assert!(
        !env.store
            .restore_origin(&name("hello"), v2_blake3, other)
            .unwrap()
    );
    assert_eq!(
        env.store.get(&name("hello")).unwrap().unwrap().origin,
        Some(origin)
    );
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

#[test]
fn refuses_reserved_paths() {
    let env = Env::new();
    for (pkg, entry) in [
        (
            "keys",
            FixtureEntry::file("etc/fox/keys/attacker.toml", b"key", 0o644),
        ),
        (
            "db",
            FixtureEntry::file("var/lib/fox/installed.toml", b"format = 1", 0o644),
        ),
        (
            "head",
            FixtureEntry::file("var/lib/fox/heads/x.pkgar_head", b"x", 0o644),
        ),
        ("redirect", FixtureEntry::symlink("var/lib", "/tmp")),
        ("redirect-keys", FixtureEntry::symlink("etc/fox", "/tmp")),
    ] {
        let path = env.package(pkg, "1", &[], &[entry]);
        match env.install(&path, true).unwrap_err() {
            StoreError::ReservedPath { .. } => {}
            other => panic!("{pkg}: unexpected error: {other}"),
        }
    }
    assert!(!env.root().join("etc/fox/keys/attacker.toml").exists());
    assert!(!env.root().join("var/lib").is_symlink());
    assert!(!env.root().join("etc/fox").is_symlink());
    assert!(env.store.list().unwrap().is_empty());
    // Look-alike names next to the reserved directories are ordinary.
    let ok = env.package(
        "ok",
        "1",
        &[],
        &[FixtureEntry::file("etc/foxrc", b"x", 0o644)],
    );
    env.install(&ok, false).unwrap();
}

#[test]
fn refuses_symlinked_root() {
    let env = Env::new();
    let link = env.work.path().join("root-link");
    symlink(env.root(), &link).unwrap();
    assert!(matches!(
        Store::open(&link),
        Err(StoreError::RootIsSymlink { .. })
    ));
    // A root reached through a relative or dotted path is canonicalized.
    let dotted = env.root().join("../").join(env.root().file_name().unwrap());
    let store = Store::open(&dotted).unwrap();
    assert_eq!(store.root(), fs::canonicalize(env.root()).unwrap());
}

#[test]
fn unsigned_names_need_opt_in_and_cannot_replace_signed_packages() {
    let env = Env::new();
    let entries = [FixtureEntry::file("usr/bin/tool", b"tool", 0o755)];
    let unsigned = env.sidecar_package("unsigned", "tool", &entries);
    assert!(matches!(
        env.install(&unsigned, true).unwrap_err(),
        StoreError::UnsignedMetadata { .. }
    ));
    assert!(!env.root().join("usr/bin/tool").exists());

    let allow = InstallOptions {
        allow_unsigned_metadata: true,
        ..InstallOptions::default()
    };
    env.install_with(&unsigned, allow).unwrap();
    assert_eq!(env.store.list().unwrap()[0].metadata, MetadataKind::Sidecar);
    env.store.remove(&name("tool"), false).unwrap();

    // A signed package cannot be replaced by an archive whose sidecar
    // merely claims its name.
    let signed = env.package(
        "hello",
        "1",
        &[],
        &[FixtureEntry::file("usr/bin/hello", b"hi", 0o755)],
    );
    env.install(&signed, false).unwrap();
    let impostor = env.sidecar_package("impostor", "hello", &entries);
    assert!(matches!(
        env.install_with(&impostor, allow).unwrap_err(),
        StoreError::UnsignedReplacesSigned { .. }
    ));
    assert_eq!(fs::read(env.root().join("usr/bin/hello")).unwrap(), b"hi");
    assert!(!env.root().join("usr/bin/tool").exists());
}

#[test]
fn installs_signed_modes_and_verifies_them() {
    let env = Env::new();
    let path = env.package(
        "hello",
        "1",
        &[],
        &[
            FixtureEntry::file("usr/bin/hello", b"hi", 0o775),
            FixtureEntry::file("usr/share/hello/data", b"d", 0o640),
        ],
    );
    env.install(&path, false).unwrap();
    let bin = env.root().join("usr/bin/hello");
    // Exact, whatever the umask of the installing process.
    assert_eq!(
        fs::metadata(&bin).unwrap().permissions().mode() & 0o7777,
        0o775
    );
    let data = env.root().join("usr/share/hello/data");
    assert_eq!(
        fs::metadata(&data).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    assert!(
        env.store
            .verify(&name("hello"), &env.key.trusted())
            .unwrap()
            .problems
            .is_empty()
    );

    fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
    let report = env
        .store
        .verify(&name("hello"), &env.key.trusted())
        .unwrap();
    assert_eq!(report.problems.len(), 1);
    assert_eq!(report.problems[0].path, "usr/bin/hello");
    assert_eq!(
        report.problems[0].kind,
        ProblemKind::Mode {
            expected: 0o775,
            found: 0o644
        }
    );
}

#[test]
fn failed_removal_is_recorded_and_can_be_finished() {
    let env = Env::new();
    let path = env.package(
        "hello",
        "1",
        &[],
        &[
            FixtureEntry::file("usr/bin/hello", b"hi", 0o755),
            FixtureEntry::file("usr/bin/other", b"o", 0o755),
        ],
    );
    env.install(&path, false).unwrap();
    // A non-empty directory where the stored head should be makes the
    // removal fail after the files are gone, even when run as root.
    let head = env.root().join("var/lib/fox/heads/hello.pkgar_head");
    fs::remove_file(&head).unwrap();
    fs::create_dir_all(head.join("blocker")).unwrap();

    assert!(matches!(
        env.store.remove(&name("hello"), false).unwrap_err(),
        StoreError::Io { .. }
    ));
    assert!(!env.root().join("usr/bin/hello").exists());
    let listed = env.store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, PackageState::Removing);
    assert!(matches!(
        env.store.verify(&name("hello"), &env.key.trusted()),
        Err(StoreError::RemovalIncomplete { .. })
    ));

    fs::remove_dir_all(&head).unwrap();
    let report = env.store.remove(&name("hello"), false).unwrap();
    assert_eq!(report.removed, 0);
    assert_eq!(report.missing, 3);
    assert!(env.store.list().unwrap().is_empty());

    // A half-removed package can also be reinstalled over.
    env.install(&path, false).unwrap();
    assert_eq!(env.store.list().unwrap()[0].state, PackageState::Installed);
}
