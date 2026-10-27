//! `redoxer pkg`: a sysroot of target packages for native dependencies.
//!
//! Ferric Oxide: packages come from a cookbook checkout's built repository
//! (`<cookbook>/repo/<target>/`), signed by its key, and fox's store
//! installs and records them. Nothing is downloaded.

use anyhow::{bail, Context};
use fox_core::PackageName;
use fox_store::deps::{Found, LocalRepository, PackageSource, Request};
use std::{
    env, fs, io,
    os::unix,
    path::{Path, PathBuf},
    process,
};

use crate::{cookbook_dir, target};

enum PkgCommand {
    Install,
    Update,
    Remove,
}

/// The cookbook's signing key, the one its packages are trusted under.
fn cookbook_keys(cookbook: &Path) -> anyhow::Result<fox_core::TrustedKeys> {
    let key_path = cookbook.join(fox_core::cookbook::PUBLIC_KEY);
    let key = fox_core::keys::load_key_file(&key_path)?;
    let mut keys = fox_core::TrustedKeys::new();
    keys.insert(fox_core::keys::COOKBOOK_LABEL, key.key);
    Ok(keys)
}

/// A repository over the cookbook's built packages, trusting its key.
fn cookbook_repository(cookbook: &Path) -> anyhow::Result<LocalRepository> {
    Ok(LocalRepository::new(
        cookbook.join("repo").join(target()),
        cookbook_keys(cookbook)?,
    ))
}

fn pkg_inner(
    sysroot: &Path,
    cookbook: impl Fn() -> anyhow::Result<PathBuf>,
    cmd: PkgCommand,
    pkgs: Vec<PackageName>,
) -> anyhow::Result<()> {
    pkg_dir_init(sysroot).context("Initializing package sysroot")?;
    let store = fox_store::Store::open(sysroot)?;
    adopt_pkg_sysroot(&store, &pkgs, &cookbook)?;
    match cmd {
        PkgCommand::Install => {
            let mut repository = cookbook_repository(&cookbook()?)?;
            install_from(
                &store,
                &mut repository,
                pkgs.into_iter().map(Request::Name).collect(),
            )
        }
        PkgCommand::Update => {
            // Only installed packages are updated, and an update can bring
            // new dependencies, so it is planned like an install.
            let mut repository = cookbook_repository(&cookbook()?)?;
            let mut requested = Vec::with_capacity(pkgs.len());
            for name in pkgs {
                if !store
                    .get(&name)?
                    .is_some_and(|package| package.state == fox_store::PackageState::Installed)
                {
                    bail!("{name} is not installed: install it instead");
                }
                match repository.find(&name)? {
                    Some(Found::Archive(..)) => requested.push(Request::Name(name)),
                    Some(Found::Meta(_)) => bail!("{name} is a meta-package: install it instead"),
                    None => bail!("{name} is not in {}", repository.dir().display()),
                }
            }
            install_from(&store, &mut repository, requested)
        }
        // Removal only touches the sysroot, so it needs no cookbook.
        PkgCommand::Remove => {
            for name in pkgs {
                let report = store.remove(&name, false)?;
                println!("redoxer pkg: removed {} {}", report.name, report.version);
            }
            Ok(())
        }
    }
}

/// Adopts the packages an earlier redoxer installed with `pkg`, so that fox
/// can update and remove them. Only runs while one of `pkgs` is listed in
/// `pkg`'s state but not recorded by fox, so other sysroots need no cookbook
/// to remove packages.
fn adopt_pkg_sysroot(
    store: &fox_store::Store,
    pkgs: &[PackageName],
    cookbook: &impl Fn() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<()> {
    if !store.root().join(fox_store::PKG_STATE).is_file() {
        return Ok(());
    }
    let mut unrecorded = false;
    for name in pkgs {
        unrecorded |= store.get(name)?.is_none();
    }
    if !unrecorded {
        return Ok(());
    }
    let keys = cookbook_keys(&cookbook()?)?;
    let options = fox_store::AdoptOptions {
        allow_unsigned_metadata: true,
        ..fox_store::AdoptOptions::default()
    };
    let report = store.adopt_pkg(&keys, options)?;
    for package in &report.adopted {
        println!("redoxer pkg: adopted {} {}", package.name, package.version);
    }
    for skipped in &report.skipped {
        eprintln!(
            "redoxer pkg: not adopting {}: {}",
            skipped.name, skipped.reason
        );
    }
    Ok(())
}

/// Installs `requested` and the dependencies they lack from `repository`.
fn install_from(
    store: &fox_store::Store,
    repository: &mut LocalRepository,
    requested: Vec<Request>,
) -> anyhow::Result<()> {
    // Without features.toml packages.signed_manifests, cookbook archives
    // carry only the unsigned metadata file beside them.
    let options = fox_store::InstallOptions {
        force: false,
        allow_unsigned_metadata: true,
    };
    let plan = fox_store::deps::plan(store, requested, repository)?;
    if !plan.missing.is_empty() {
        let missing: Vec<_> = plan.missing.iter().map(|m| m.dependency.as_str()).collect();
        bail!("missing dependencies: {}", missing.join(", "));
    }
    for report in store.install_plan(&plan, options)? {
        println!("redoxer pkg: installed {} {}", report.name, report.version);
    }
    Ok(())
}

fn pkg_dir_init(sysroot: &Path) -> io::Result<()> {
    let usr = sysroot.join("usr");
    if !usr.is_dir() {
        println!("redoxer: building pkg sysroot");
        fs::create_dir_all(&usr)?;
        for folder in ["bin", "include", "lib", "share"] {
            fs::create_dir(usr.join(folder))?;
            unix::fs::symlink(format!("usr/{folder}"), sysroot.join(folder))?;
        }
    }

    Ok(())
}

pub fn main(args: &[String]) {
    let args: Vec<String> = args.iter().skip(2).cloned().collect();

    let mut i = 0;
    let mut pkgs = Vec::new();
    let mut cmd = PkgCommand::Install;
    let mut cmd_set = false;
    while i < args.len() {
        match (args[i].as_str(), cmd_set) {
            ("install", false) => {
                cmd = PkgCommand::Install;
                cmd_set = true;
            }
            ("update", false) => {
                cmd = PkgCommand::Update;
                cmd_set = true;
            }
            ("remove", false) => {
                cmd = PkgCommand::Remove;
                cmd_set = true;
            }
            ("--help", _) => {
                pkg_usage();
            }
            (arg, _) => {
                pkgs.push(arg);
            }
        }
        i += 1;
    }

    if pkgs.is_empty() {
        pkg_usage();
    }

    let sysroot = get_sysroot()
        .or_else(get_cargo_sysroot_default_path)
        .expect("Please define REDOXER_SYSROOT as destination to install packages");

    let pkgs = pkgs
        .iter()
        .map(|p| PackageName::new(*p).unwrap_or_else(|_| panic!("Invalid pkg name: {p}")))
        .collect();

    match pkg_inner(&sysroot, cookbook_dir, cmd, pkgs) {
        Ok(()) => {
            process::exit(0);
        }
        Err(err) => {
            eprintln!("redoxer pkg: {err:#}");
            process::exit(1);
        }
    }
}

pub fn get_sysroot() -> Option<PathBuf> {
    env::var("REDOXER_SYSROOT")
        .ok()
        .map(|p| Path::new(&p).to_owned())
        .or_else(|| {
            let path = get_cargo_sysroot_default_path()?;
            if path.join("lib").is_dir() {
                return path.canonicalize().ok();
            }
            None
        })
}

fn get_cargo_sysroot_default_path() -> Option<PathBuf> {
    if Path::new("Cargo.toml").is_file() {
        Some(Path::new(&format!("target/{}/sysroot", target())).to_path_buf())
    } else {
        None
    }
}

fn pkg_usage() {
    eprintln!("redoxer pkg [install|remove|update] pkg-1 pkg-2 ...");
    eprintln!(" arguments:");
    eprintln!("   [install|remove|update]  whether to install, update, or remove pkg (optional, default is install)");
    eprintln!();
    eprintln!(" environment variables:");
    eprintln!("   REDOXER_SYSROOT          where to install sysroot (required when no Cargo.toml)");
    eprintln!("   REDOXER_COOKBOOK         checkout whose repo/<target> has the built packages (required to install or update)");
    eprintln!();
    process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_core::fixtures::{build_package, FixtureEntry, FixtureKey};
    use fox_core::Manifest;

    /// A cookbook whose repository has `app`, which needs `lib`.
    fn cookbook() -> (tempfile::TempDir, FixtureKey) {
        let cookbook = tempfile::tempdir().unwrap();
        let build = cookbook.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let key = FixtureKey::generate(&build, "id_ed25519").unwrap();
        let repo = cookbook.path().join("repo").join(target());
        fs::create_dir_all(&repo).unwrap();
        let lib = Manifest::new(PackageName::new("lib").unwrap(), "1.0");
        let lib_file = [FixtureEntry::file("usr/lib/liblib.a", b"lib", 0o644)];
        build_package(&repo, &key, Some(&lib), &lib_file, false).unwrap();
        let mut app = Manifest::new(PackageName::new("app").unwrap(), "1.0");
        app.depends = vec![lib.name.clone()];
        let app_file = [FixtureEntry::file("usr/include/app.h", b"app", 0o644)];
        build_package(&repo, &key, Some(&app), &app_file, false).unwrap();
        (cookbook, key)
    }

    fn names(list: &[&str]) -> Vec<PackageName> {
        list.iter().map(|n| PackageName::new(*n).unwrap()).collect()
    }

    fn run(root: &Path, cookbook: &Path, cmd: PkgCommand, list: &[&str]) -> anyhow::Result<()> {
        pkg_inner(root, || Ok(cookbook.to_path_buf()), cmd, names(list))
    }

    fn installed(root: &Path) -> Vec<PackageName> {
        let store = fox_store::Store::open(root).unwrap();
        store.list().unwrap().into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn install_update_and_remove_from_the_cookbook() {
        let (cookbook, _) = cookbook();
        let sysroot = tempfile::tempdir().unwrap();
        let root = sysroot.path();

        run(root, cookbook.path(), PkgCommand::Install, &["app"]).unwrap();
        assert!(root.join("include/app.h").is_file(), "usr is linked");
        assert!(
            root.join("usr/lib/liblib.a").is_file(),
            "dependency installed"
        );

        run(root, cookbook.path(), PkgCommand::Update, &["app"]).unwrap();
        run(root, cookbook.path(), PkgCommand::Remove, &["app"]).unwrap();
        assert!(!root.join("usr/include/app.h").exists());
        assert_eq!(installed(root), names(&["lib"]));

        assert!(run(root, cookbook.path(), PkgCommand::Update, &["nope"]).is_err());
        assert!(
            run(root, cookbook.path(), PkgCommand::Update, &["app"]).is_err(),
            "only installed packages are updated"
        );
    }

    #[test]
    fn update_installs_new_dependencies() {
        let (cookbook, key) = cookbook();
        let sysroot = tempfile::tempdir().unwrap();
        let root = sysroot.path();
        run(root, cookbook.path(), PkgCommand::Install, &["lib"]).unwrap();
        // Rebuilt so that it depends on a package not installed yet.
        let repo = cookbook.path().join("repo").join(target());
        let new = Manifest::new(PackageName::new("libnew").unwrap(), "1.0");
        let new_file = [FixtureEntry::file("usr/lib/libnew.a", b"new", 0o644)];
        build_package(&repo, &key, Some(&new), &new_file, false).unwrap();
        let mut lib = Manifest::new(PackageName::new("lib").unwrap(), "2.0");
        lib.depends = vec![new.name.clone()];
        let lib_file = [FixtureEntry::file("usr/lib/liblib.a", b"lib2", 0o644)];
        build_package(&repo, &key, Some(&lib), &lib_file, false).unwrap();

        run(root, cookbook.path(), PkgCommand::Update, &["lib"]).unwrap();
        assert!(root.join("usr/lib/libnew.a").is_file());
        assert_eq!(installed(root), names(&["lib", "libnew"]));
    }

    /// Installs `name` from the cookbook the way `pkg` did: extract it, keep
    /// its head and list it in `pkg`'s state, with no fox record.
    fn pkg_install(root: &Path, cookbook: &Path, key: &FixtureKey, name: &str) {
        let archive = cookbook
            .join("repo")
            .join(target())
            .join(format!("{name}.pkgar"));
        pkgar::extract(&key.public_path, &archive, root).unwrap();
        let pkey = pkgar_keys::PublicKeyFile::open(&key.public_path)
            .unwrap()
            .pkey;
        let heads = root.join(fox_store::PKG_HEADS_DIR);
        fs::create_dir_all(&heads).unwrap();
        pkgar::PackageFile::new(&archive, &pkey)
            .unwrap()
            .split(heads.join(format!("{name}.pkgar_head")), None::<&Path>)
            .unwrap();
        let state = root.join(fox_store::PKG_STATE);
        fs::create_dir_all(state.parent().unwrap()).unwrap();
        let mut text = fs::read_to_string(&state).unwrap_or_default();
        text.push_str(&format!(
            "[installed.{name:?}]\nremote = \"local\"\nblake3 = \"\"\nmanual = true\ndependencies = []\n"
        ));
        fs::write(state, text).unwrap();
    }

    #[test]
    fn sysroots_populated_by_pkg_are_adopted() {
        let (cookbook, key) = cookbook();
        let sysroot = tempfile::tempdir().unwrap();
        let root = sysroot.path();
        pkg_dir_init(root).unwrap();
        pkg_install(root, cookbook.path(), &key, "lib");
        pkg_install(root, cookbook.path(), &key, "app");

        run(root, cookbook.path(), PkgCommand::Update, &["lib"]).unwrap();
        assert_eq!(installed(root), names(&["app", "lib"]));
        run(root, cookbook.path(), PkgCommand::Remove, &["app"]).unwrap();
        assert!(!root.join("usr/include/app.h").exists());

        let no_cookbook = || anyhow::bail!("no cookbook");
        pkg_inner(root, no_cookbook, PkgCommand::Remove, names(&["lib"])).unwrap();
        assert!(installed(root).is_empty(), "adoption runs once");
    }

    #[test]
    fn remove_needs_no_cookbook() {
        let (cookbook, _) = cookbook();
        let sysroot = tempfile::tempdir().unwrap();
        let root = sysroot.path();
        run(root, cookbook.path(), PkgCommand::Install, &["lib"]).unwrap();
        let no_cookbook = || anyhow::bail!("no cookbook");
        pkg_inner(root, no_cookbook, PkgCommand::Remove, names(&["lib"])).unwrap();
        assert!(installed(root).is_empty());
    }
}
