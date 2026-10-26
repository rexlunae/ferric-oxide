//! Installing a package together with the dependencies it is missing.
//!
//! [`plan`] takes the archives the caller asked for and asks a
//! [`PackageSource`] for every dependency that is neither installed nor
//! already in the plan, then for their dependencies, until nothing more can
//! be found. It returns the archives in installation order, dependencies
//! before the packages that need them, and the dependencies no source has.
//! [`Store::install_plan`] installs a plan in that order.
//!
//! [`LocalRepository`] is the source for a directory of archives such as the
//! cookbook's `repo/<target>/`, where `<name>.pkgar` holds package `name`.
//! The `fox` command adds registries as a further source.
//!
//! # Examples
//!
//! ```
//! use fox_core::archive::Archive;
//! use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
//! use fox_core::{Manifest, PackageName};
//! use fox_store::deps::{LocalRepository, plan};
//! use fox_store::{InstallOptions, Store};
//!
//! let repo = tempfile::tempdir()?;
//! let root = tempfile::tempdir()?;
//! let key = FixtureKey::generate(repo.path(), "test")?;
//! let lib = Manifest::new(PackageName::new("libhello")?, "1.0.0");
//! build_package(repo.path(), &key, Some(&lib), &[FixtureEntry::file("usr/lib/libhello.so", b"", 0o644)], false)?;
//! let mut app = Manifest::new(PackageName::new("hello")?, "1.0.0");
//! app.depends = vec![lib.name.clone()];
//! let path = build_package(repo.path(), &key, Some(&app), &[FixtureEntry::file("usr/bin/hello", b"", 0o755)], false)?;
//!
//! let store = Store::open(root.path())?;
//! let mut source = LocalRepository::new(repo.path(), key.trusted());
//! let plan = plan(&store, vec![(Archive::open(&path, &key.trusted())?, None)], &mut source)?;
//! assert!(plan.missing.is_empty());
//! store.install_plan(&plan, InstallOptions::default())?;
//! assert!(root.path().join("usr/lib/libhello.so").is_file());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fox_core::archive::{Archive, ArchiveError};
use fox_core::{PackageName, TrustedKeys};

use crate::{InstallOptions, InstallReport, Origin, Store, StoreError};

/// Somewhere dependencies can be found: a repository directory, a registry.
pub trait PackageSource {
    /// Why looking a package up failed.
    type Error: std::error::Error + 'static;

    /// Finds and verifies the archive of package `name`, with the registry
    /// it came from if it came from one. `Ok(None)` means this source does
    /// not have the package.
    ///
    /// # Errors
    ///
    /// Returns an error if the package is there but cannot be fetched or
    /// does not verify. A source that has nothing for `name` returns
    /// `Ok(None)` instead.
    fn find(
        &mut self,
        name: &PackageName,
    ) -> Result<Option<(Archive, Option<Origin>)>, Self::Error>;
}

/// Tries `self.0`, then `self.1`.
impl<A: PackageSource, B: PackageSource> PackageSource for (A, B) {
    type Error = ChainError<A::Error, B::Error>;

    fn find(
        &mut self,
        name: &PackageName,
    ) -> Result<Option<(Archive, Option<Origin>)>, Self::Error> {
        if let Some(found) = self.0.find(name).map_err(ChainError::First)? {
            return Ok(Some(found));
        }
        self.1.find(name).map_err(ChainError::Second)
    }
}

/// Tries each source in turn.
impl<S: PackageSource> PackageSource for Vec<S> {
    type Error = S::Error;

    fn find(
        &mut self,
        name: &PackageName,
    ) -> Result<Option<(Archive, Option<Origin>)>, Self::Error> {
        for source in self {
            if let Some(found) = source.find(name)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }
}

/// The error of a pair of sources.
#[derive(Debug, thiserror::Error)]
pub enum ChainError<A, B> {
    /// The first source failed.
    #[error(transparent)]
    First(A),
    /// The second source failed.
    #[error(transparent)]
    Second(B),
}

/// A source with nothing in it, for installing without dependencies.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSource;

impl PackageSource for NoSource {
    type Error = std::convert::Infallible;

    fn find(&mut self, _: &PackageName) -> Result<Option<(Archive, Option<Origin>)>, Self::Error> {
        Ok(None)
    }
}

/// A directory of archives named after their packages (`<name>.pkgar`,
/// with the cookbook's `<name>.toml` beside an archive that has no signed
/// manifest), such as the cookbook's `repo/<target>/`.
#[derive(Debug, Clone)]
pub struct LocalRepository {
    dir: PathBuf,
    keys: TrustedKeys,
}

/// A repository archive that is not the package its file name says.
#[derive(Debug, thiserror::Error)]
pub enum LocalRepositoryError {
    /// The archive did not verify.
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    /// The archive names another package.
    #[error("{} contains package {found}, not {expected}", path.display())]
    WrongPackage {
        /// The archive.
        path: PathBuf,
        /// The name its file has.
        expected: PackageName,
        /// The package it holds.
        found: PackageName,
    },
}

impl LocalRepository {
    /// A repository in `dir` whose archives must verify against `keys`.
    pub fn new(dir: impl Into<PathBuf>, keys: TrustedKeys) -> Self {
        Self {
            dir: dir.into(),
            keys,
        }
    }

    /// The repository directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The archive file package `name` would have.
    pub fn archive_path(&self, name: &PackageName) -> PathBuf {
        self.dir.join(format!("{name}.pkgar"))
    }
}

impl PackageSource for LocalRepository {
    type Error = LocalRepositoryError;

    fn find(
        &mut self,
        name: &PackageName,
    ) -> Result<Option<(Archive, Option<Origin>)>, Self::Error> {
        let path = self.archive_path(name);
        if !path.is_file() {
            return Ok(None);
        }
        let archive = Archive::open(&path, &self.keys)?;
        let found = &archive.manifest().name;
        if found != name {
            return Err(LocalRepositoryError::WrongPackage {
                path,
                expected: name.clone(),
                found: found.clone(),
            });
        }
        Ok(Some((archive, None)))
    }
}

/// A dependency no source has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDependency {
    /// The package that needs it.
    pub package: PackageName,
    /// The package it needs.
    pub dependency: PackageName,
}

/// Archives in installation order, from [`plan`].
#[derive(Debug)]
pub struct Plan {
    /// The archives, each before every planned package that depends on it,
    /// with the registry each came from.
    pub install: Vec<(Archive, Option<Origin>)>,
    /// Dependencies that are neither installed, nor requested, nor found.
    pub missing: Vec<MissingDependency>,
}

/// Why a plan could not be made.
#[derive(Debug, thiserror::Error)]
pub enum PlanError<E> {
    /// The installed-package database could not be read.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The source failed to look a dependency up.
    #[error("cannot get {dependency}, needed by {package}")]
    Source {
        /// The package that needs it.
        package: PackageName,
        /// The dependency.
        dependency: PackageName,
        /// The source's error.
        #[source]
        source: E,
    },
    /// Two requested archives are the same package.
    #[error("{0} is requested more than once")]
    Duplicate(PackageName),
}

/// Adds the missing dependencies of `requested` from `source`, and orders
/// everything for installation.
///
/// A dependency that is installed is not fetched again, even in another
/// version. Dependency cycles are allowed; the packages in a cycle are
/// installed in the order they were found.
///
/// # Errors
///
/// - [`PlanError::Store`] if the installed packages cannot be read;
/// - [`PlanError::Duplicate`] if two requested archives are one package;
/// - [`PlanError::Source`] if the source fails for a dependency.
pub fn plan<S: PackageSource>(
    store: &Store,
    requested: Vec<(Archive, Option<Origin>)>,
    source: &mut S,
) -> Result<Plan, PlanError<S::Error>> {
    let installed: BTreeSet<PackageName> = store
        .list()?
        .into_iter()
        .filter(|package| package.state.is_installed())
        .map(|package| package.name)
        .collect();
    let mut found: BTreeMap<PackageName, (Archive, Option<Origin>)> = BTreeMap::new();
    let mut roots = Vec::new();
    for (archive, origin) in requested {
        let name = archive.manifest().name.clone();
        if found.insert(name.clone(), (archive, origin)).is_some() {
            return Err(PlanError::Duplicate(name));
        }
        roots.push(name);
    }

    let mut missing = Vec::new();
    let mut queue = roots.clone();
    while let Some(name) = queue.pop() {
        let depends = found[&name].0.manifest().depends.clone();
        for dependency in depends {
            if installed.contains(&dependency) || found.contains_key(&dependency) {
                continue;
            }
            match source.find(&dependency) {
                Ok(Some(archive)) => {
                    found.insert(dependency.clone(), archive);
                    queue.push(dependency);
                }
                Ok(None) => {
                    let entry = MissingDependency {
                        package: name.clone(),
                        dependency,
                    };
                    if !missing.contains(&entry) {
                        missing.push(entry);
                    }
                }
                Err(error) => {
                    return Err(PlanError::Source {
                        package: name,
                        dependency,
                        source: error,
                    });
                }
            }
        }
    }

    let mut order = Vec::with_capacity(found.len());
    let mut visited = BTreeSet::new();
    for name in &roots {
        visit(name, &found, &mut visited, &mut order);
    }
    let install = order
        .into_iter()
        .filter_map(|name| found.remove(&name))
        .collect();
    Ok(Plan { install, missing })
}

/// Depth-first post-order: dependencies first. A package already visited
/// (including one on the current path, in a cycle) is not visited again.
fn visit(
    name: &PackageName,
    found: &BTreeMap<PackageName, (Archive, Option<Origin>)>,
    visited: &mut BTreeSet<PackageName>,
    order: &mut Vec<PackageName>,
) {
    let Some((archive, _)) = found.get(name) else {
        return;
    };
    if !visited.insert(name.clone()) {
        return;
    }
    for dependency in &archive.manifest().depends {
        visit(dependency, found, visited, order);
    }
    order.push(name.clone());
}

impl Store {
    /// Installs every archive of `plan` in order with
    /// [`Store::install_from`], stopping at the first failure.
    ///
    /// # Errors
    ///
    /// As [`Store::install`]; the packages before the failing one stay
    /// installed.
    pub fn install_plan(
        &self,
        plan: &Plan,
        options: InstallOptions,
    ) -> Result<Vec<InstallReport>, StoreError> {
        plan.install
            .iter()
            .map(|(archive, origin)| self.install_from(archive, options, origin.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use fox_core::Manifest;
    use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};

    use super::*;

    struct Repo {
        dir: tempfile::TempDir,
        key: FixtureKey,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let key = FixtureKey::generate(dir.path(), "test").unwrap();
            Self { dir, key }
        }

        fn add(&self, name: &str, depends: &[&str]) -> PathBuf {
            let mut manifest = Manifest::new(PackageName::new(name).unwrap(), "1.0.0");
            manifest.depends = depends
                .iter()
                .map(|d| PackageName::new(*d).unwrap())
                .collect();
            let file = format!("usr/share/{name}");
            let entries = [FixtureEntry::file(&file, name.as_bytes(), 0o644)];
            build_package(self.dir.path(), &self.key, Some(&manifest), &entries, false).unwrap()
        }

        fn open(&self, name: &str) -> (Archive, Option<Origin>) {
            let path = self.dir.path().join(format!("{name}.pkgar"));
            (Archive::open(&path, &self.key.trusted()).unwrap(), None)
        }

        fn source(&self) -> LocalRepository {
            LocalRepository::new(self.dir.path(), self.key.trusted())
        }
    }

    fn names(plan: &Plan) -> Vec<&str> {
        plan.install
            .iter()
            .map(|(archive, _)| archive.manifest().name.as_str())
            .collect()
    }

    #[test]
    fn dependencies_come_first() {
        let repo = Repo::new();
        repo.add("c", &[]);
        repo.add("b", &["c"]);
        repo.add("a", &["b", "c"]);
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let plan = plan(&store, vec![repo.open("a")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["c", "b", "a"]);
        assert!(plan.missing.is_empty());
        store
            .install_plan(&plan, InstallOptions::default())
            .unwrap();
        assert_eq!(store.list().unwrap().len(), 3);
    }

    #[test]
    fn installed_dependencies_are_not_fetched() {
        let repo = Repo::new();
        repo.add("lib", &[]);
        repo.add("app", &["lib"]);
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        store
            .install(&repo.open("lib").0, InstallOptions::default())
            .unwrap();
        let plan = plan(&store, vec![repo.open("app")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["app"]);
    }

    #[test]
    fn missing_dependencies_are_reported() {
        let repo = Repo::new();
        repo.add("app", &["gone", "lib"]);
        repo.add("lib", &["gone"]);
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let plan = plan(&store, vec![repo.open("app")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["lib", "app"]);
        let missing: Vec<_> = plan
            .missing
            .iter()
            .map(|m| (m.package.as_str(), m.dependency.as_str()))
            .collect();
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&("app", "gone")) && missing.contains(&("lib", "gone")));
    }

    #[test]
    fn cycles_terminate() {
        let repo = Repo::new();
        repo.add("a", &["b"]);
        repo.add("b", &["a"]);
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let plan = plan(&store, vec![repo.open("a")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["b", "a"]);
    }

    #[test]
    fn no_source_installs_only_what_was_asked() {
        let repo = Repo::new();
        repo.add("lib", &[]);
        repo.add("app", &["lib"]);
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let plan = plan(&store, vec![repo.open("app")], &mut NoSource).unwrap();
        assert_eq!(names(&plan), ["app"]);
        assert_eq!(plan.missing.len(), 1);
    }

    #[test]
    fn duplicates_are_refused() {
        let repo = Repo::new();
        repo.add("a", &[]);
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let error = plan(&store, vec![repo.open("a"), repo.open("a")], &mut NoSource).unwrap_err();
        assert!(matches!(error, PlanError::Duplicate(name) if name.as_str() == "a"));
    }

    #[test]
    fn misnamed_archives_are_refused() {
        let repo = Repo::new();
        repo.add("app", &["lib"]);
        let real = repo.add("other", &[]);
        std::fs::copy(&real, repo.dir.path().join("lib.pkgar")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let error = plan(&store, vec![repo.open("app")], &mut repo.source()).unwrap_err();
        assert!(matches!(
            error,
            PlanError::Source {
                source: LocalRepositoryError::WrongPackage { .. },
                ..
            }
        ));
    }

    #[test]
    fn chained_sources() {
        let first = Repo::new();
        let second = Repo::new();
        first.add("app", &["lib"]);
        second.add("lib", &[]);
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let mut source = (first.source(), second.source());
        let plan = plan(&store, vec![first.open("app")], &mut source);
        // `lib` is signed by the second repository's key only.
        assert_eq!(names(&plan.unwrap()), ["lib", "app"]);
    }
}
