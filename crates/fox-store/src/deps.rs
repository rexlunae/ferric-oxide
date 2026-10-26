//! Installing packages together with the dependencies they are missing.
//!
//! [`plan`] takes what the caller asked for, archives or package names, and
//! asks a [`PackageSource`] for every package named and every dependency
//! that is neither installed nor already in the plan, then for their
//! dependencies, until nothing more can be found. It returns the archives in
//! installation order, dependencies before the packages that need them, and
//! the dependencies no source has. [`Store::install_plan`] installs a plan
//! in that order.
//!
//! A source can also answer with a meta-package: a name that stands for a
//! list of other packages and has no files, such as the cookbook's
//! `recipes/groups/`. Its packages join the plan; the meta-package itself
//! is not recorded as installed.
//!
//! [`LocalRepository`] is the source for a directory of archives such as the
//! cookbook's `repo/<target>/`, where `<name>.pkgar` holds package `name`
//! and a `<name>.toml` with no archive beside it is a meta-package. The
//! `fox` command adds registries as a further source.
//!
//! # Examples
//!
//! ```
//! use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
//! use fox_core::{Manifest, PackageName};
//! use fox_store::deps::{LocalRepository, Request, plan};
//! use fox_store::{InstallOptions, Store};
//!
//! let repo = tempfile::tempdir()?;
//! let root = tempfile::tempdir()?;
//! let key = FixtureKey::generate(repo.path(), "test")?;
//! let lib = Manifest::new(PackageName::new("libhello")?, "1.0.0");
//! let lib_files = [FixtureEntry::file("usr/lib/libhello.so", b"", 0o644)];
//! build_package(repo.path(), &key, Some(&lib), &lib_files, false)?;
//! let mut app = Manifest::new(PackageName::new("hello")?, "1.0.0");
//! app.depends = vec![lib.name.clone()];
//! let app_files = [FixtureEntry::file("usr/bin/hello", b"", 0o755)];
//! build_package(repo.path(), &key, Some(&app), &app_files, false)?;
//!
//! let store = Store::open(root.path())?;
//! let mut source = LocalRepository::new(repo.path(), key.trusted());
//! let plan = plan(&store, vec![Request::Name(app.name)], &mut source)?;
//! assert!(plan.missing.is_empty());
//! store.install_plan(&plan, InstallOptions::default())?;
//! assert!(root.path().join("usr/lib/libhello.so").is_file());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fox_core::archive::{Archive, ArchiveError};
use fox_core::cookbook::{CookbookError, PackageInfo};
use fox_core::{NameError, PackageName, TrustedKeys};

use crate::{InstallOptions, InstallReport, Origin, Store, StoreError};

/// What a [`PackageSource`] has under a name.
#[derive(Debug)]
pub enum Found {
    /// A verified archive, with the registry it came from if it came from
    /// one.
    Archive(Box<Archive>, Option<Origin>),
    /// A meta-package: it installs nothing itself, only these packages.
    Meta(Vec<PackageName>),
}

/// Somewhere packages can be found: a repository directory, a registry.
pub trait PackageSource {
    /// Why looking a package up failed.
    type Error: std::error::Error + 'static;

    /// Finds and verifies package `name`. `Ok(None)` means this source does
    /// not have it.
    ///
    /// # Errors
    ///
    /// Returns an error if the package is there but cannot be fetched or
    /// does not verify. A source that has nothing for `name` returns
    /// `Ok(None)` instead.
    fn find(&mut self, name: &PackageName) -> Result<Option<Found>, Self::Error>;
}

/// Tries `self.0`, then `self.1`.
impl<A: PackageSource, B: PackageSource> PackageSource for (A, B) {
    type Error = ChainError<A::Error, B::Error>;

    fn find(&mut self, name: &PackageName) -> Result<Option<Found>, Self::Error> {
        if let Some(found) = self.0.find(name).map_err(ChainError::First)? {
            return Ok(Some(found));
        }
        self.1.find(name).map_err(ChainError::Second)
    }
}

/// Tries each source in turn.
impl<S: PackageSource> PackageSource for Vec<S> {
    type Error = S::Error;

    fn find(&mut self, name: &PackageName) -> Result<Option<Found>, Self::Error> {
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

    fn find(&mut self, _: &PackageName) -> Result<Option<Found>, Self::Error> {
        Ok(None)
    }
}

/// A directory of archives named after their packages (`<name>.pkgar`,
/// with the cookbook's `<name>.toml` beside an archive that has no signed
/// manifest), such as the cookbook's `repo/<target>/`. A `<name>.toml`
/// without an archive is a meta-package.
#[derive(Debug, Clone)]
pub struct LocalRepository {
    dir: PathBuf,
    keys: TrustedKeys,
}

/// Why a package in a [`LocalRepository`] cannot be used.
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
    /// A meta-package's metadata file cannot be read.
    #[error(transparent)]
    Metadata(#[from] CookbookError),
    /// A meta-package's metadata file names another package.
    #[error("{} describes package {found}, not {expected}", path.display())]
    WrongMetadata {
        /// The metadata file.
        path: PathBuf,
        /// The name its file has.
        expected: PackageName,
        /// The package it describes.
        found: String,
    },
    /// A meta-package lists a dependency that is not a package name.
    #[error("{} lists dependency {dependency:?}", path.display())]
    Dependency {
        /// The metadata file.
        path: PathBuf,
        /// The dependency as written.
        dependency: String,
        /// Why it is not a package name.
        #[source]
        source: NameError,
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

    /// Whether the repository has an archive or meta-package called `name`,
    /// without verifying it.
    pub fn contains(&self, name: &PackageName) -> bool {
        self.archive_path(name).is_file() || self.metadata_path(name).is_file()
    }

    fn metadata_path(&self, name: &PackageName) -> PathBuf {
        self.dir.join(format!("{name}.toml"))
    }

    fn meta(&self, name: &PackageName, path: PathBuf) -> Result<Found, LocalRepositoryError> {
        let info = PackageInfo::from_file(&path)?;
        if info.name.as_str() != name.as_str() {
            return Err(LocalRepositoryError::WrongMetadata {
                path,
                expected: name.clone(),
                found: info.name.into(),
            });
        }
        let depends = info
            .depends
            .iter()
            .map(|dep| {
                dep.package_name(Some(name))
                    .map_err(|source| LocalRepositoryError::Dependency {
                        path: path.clone(),
                        dependency: dep.to_string(),
                        source,
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(Found::Meta(depends))
    }
}

impl PackageSource for LocalRepository {
    type Error = LocalRepositoryError;

    fn find(&mut self, name: &PackageName) -> Result<Option<Found>, Self::Error> {
        let path = self.archive_path(name);
        if !path.is_file() {
            let metadata = self.metadata_path(name);
            if metadata.is_file() {
                return self.meta(name, metadata).map(Some);
            }
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
        Ok(Some(Found::Archive(Box::new(archive), None)))
    }
}

/// Something to install.
#[derive(Debug)]
pub enum Request {
    /// A verified archive, with the registry it came from, if any.
    Archive(Box<Archive>, Option<Origin>),
    /// A package to look up in the source.
    Name(PackageName),
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
    /// The meta-packages whose packages were added.
    pub meta: Vec<PackageName>,
    /// Dependencies that are neither installed, nor requested, nor found.
    pub missing: Vec<MissingDependency>,
}

/// Why a plan could not be made.
#[derive(Debug, thiserror::Error)]
pub enum PlanError<E> {
    /// The installed-package database could not be read.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The source failed to look a package up.
    #[error("cannot get {name}")]
    Source {
        /// The package.
        name: PackageName,
        /// The package that needs it, unless it was requested.
        needed_by: Option<PackageName>,
        /// The source's error.
        #[source]
        source: E,
    },
    /// A requested package is in no source.
    #[error("no package named {0}")]
    NotFound(PackageName),
    /// A package is requested more than once.
    #[error("{0} is requested more than once")]
    Duplicate(PackageName),
}

/// What [`plan`] has collected under a name.
enum Node {
    Archive(Box<Archive>, Option<Origin>),
    Meta(Vec<PackageName>),
}

impl Node {
    fn depends(&self) -> &[PackageName] {
        match self {
            Self::Archive(archive, _) => &archive.manifest().depends,
            Self::Meta(depends) => depends,
        }
    }
}

impl From<Found> for Node {
    fn from(found: Found) -> Self {
        match found {
            Found::Archive(archive, origin) => Self::Archive(archive, origin),
            Found::Meta(depends) => Self::Meta(depends),
        }
    }
}

/// Resolves `requested` and adds their missing dependencies from `source`,
/// ordering everything for installation.
///
/// A dependency that is installed is not fetched again, even in another
/// version; a requested name or archive is installed whether or not it is.
/// Dependency cycles are allowed; the packages in a cycle are installed in
/// the order they were found.
///
/// # Errors
///
/// - [`PlanError::Store`] if the installed packages cannot be read;
/// - [`PlanError::Duplicate`] if a package is requested twice;
/// - [`PlanError::NotFound`] if no source has a requested name;
/// - [`PlanError::Source`] if the source fails for a package.
pub fn plan<S: PackageSource>(
    store: &Store,
    requested: Vec<Request>,
    source: &mut S,
) -> Result<Plan, PlanError<S::Error>> {
    let installed: BTreeSet<PackageName> = store
        .list()?
        .into_iter()
        .filter(|package| package.state.is_installed())
        .map(|package| package.name)
        .collect();
    let mut found: BTreeMap<PackageName, Node> = BTreeMap::new();
    let mut roots = Vec::new();
    for request in requested {
        let (name, node) = match request {
            Request::Archive(archive, origin) => (
                archive.manifest().name.clone(),
                Node::Archive(archive, origin),
            ),
            Request::Name(name) => {
                if found.contains_key(&name) {
                    return Err(PlanError::Duplicate(name));
                }
                let node = source
                    .find(&name)
                    .map_err(|error| PlanError::Source {
                        name: name.clone(),
                        needed_by: None,
                        source: error,
                    })?
                    .ok_or_else(|| PlanError::NotFound(name.clone()))?;
                (name, node.into())
            }
        };
        if found.insert(name.clone(), node).is_some() {
            return Err(PlanError::Duplicate(name));
        }
        roots.push(name);
    }

    let mut missing = Vec::new();
    let mut queue = roots.clone();
    while let Some(name) = queue.pop() {
        let depends = found[&name].depends().to_vec();
        for dependency in depends {
            if installed.contains(&dependency) || found.contains_key(&dependency) {
                continue;
            }
            match source.find(&dependency) {
                Ok(Some(node)) => {
                    found.insert(dependency.clone(), node.into());
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
                        name: dependency,
                        needed_by: Some(name),
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
    let mut install = Vec::with_capacity(order.len());
    let mut meta = Vec::new();
    for name in order {
        match found.remove(&name) {
            Some(Node::Archive(archive, origin)) => install.push((*archive, origin)),
            Some(Node::Meta(_)) => meta.push(name),
            None => {}
        }
    }
    Ok(Plan {
        install,
        meta,
        missing,
    })
}

/// Depth-first post-order: dependencies first. A package already visited
/// (including one on the current path, in a cycle) is not visited again.
fn visit(
    name: &PackageName,
    found: &BTreeMap<PackageName, Node>,
    visited: &mut BTreeSet<PackageName>,
    order: &mut Vec<PackageName>,
) {
    let Some(node) = found.get(name) else {
        return;
    };
    if !visited.insert(name.clone()) {
        return;
    }
    for dependency in node.depends() {
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

        fn add_meta(&self, name: &str, depends: &[&str]) {
            let depends: Vec<String> = depends.iter().map(|d| format!("{d:?}")).collect();
            let text = format!(
                "name = {name:?}\ntarget = \"x86_64-unknown-redox\"\ndepends = [{}]\n",
                depends.join(", ")
            );
            std::fs::write(self.dir.path().join(format!("{name}.toml")), text).unwrap();
        }

        fn open(&self, name: &str) -> Request {
            let path = self.dir.path().join(format!("{name}.pkgar"));
            Request::Archive(
                Box::new(Archive::open(&path, &self.key.trusted()).unwrap()),
                None,
            )
        }

        fn source(&self) -> LocalRepository {
            LocalRepository::new(self.dir.path(), self.key.trusted())
        }
    }

    fn name(text: &str) -> Request {
        Request::Name(PackageName::new(text).unwrap())
    }

    fn names(plan: &Plan) -> Vec<&str> {
        plan.install
            .iter()
            .map(|(archive, _)| archive.manifest().name.as_str())
            .collect()
    }

    fn store() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        (root, store)
    }

    #[test]
    fn dependencies_come_first() {
        let repo = Repo::new();
        repo.add("c", &[]);
        repo.add("b", &["c"]);
        repo.add("a", &["b", "c"]);
        let (_root, store) = store();
        let plan = plan(&store, vec![repo.open("a")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["c", "b", "a"]);
        assert!(plan.missing.is_empty());
        store
            .install_plan(&plan, InstallOptions::default())
            .unwrap();
        assert_eq!(store.list().unwrap().len(), 3);
    }

    #[test]
    fn names_are_looked_up() {
        let repo = Repo::new();
        repo.add("lib", &[]);
        repo.add("app", &["lib"]);
        let (_root, store) = store();
        let plan = plan(&store, vec![name("app")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["lib", "app"]);
        let error = super::plan(&store, vec![name("absent")], &mut repo.source()).unwrap_err();
        assert!(matches!(error, PlanError::NotFound(n) if n.as_str() == "absent"));
    }

    #[test]
    fn meta_packages_add_their_packages() {
        let repo = Repo::new();
        repo.add("lib", &[]);
        repo.add("tool", &["lib"]);
        repo.add_meta("inner", &["lib"]);
        repo.add_meta("group", &["tool", "inner", "gone"]);
        let (_root, store) = store();
        let plan = plan(&store, vec![name("group")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["lib", "tool"]);
        let meta: Vec<_> = plan.meta.iter().map(PackageName::as_str).collect();
        assert_eq!(meta, ["inner", "group"]);
        assert_eq!(
            plan.missing,
            [MissingDependency {
                package: PackageName::new("group").unwrap(),
                dependency: PackageName::new("gone").unwrap(),
            }]
        );
    }

    #[test]
    fn meta_metadata_must_name_its_package() {
        let repo = Repo::new();
        repo.add_meta("group", &[]);
        std::fs::rename(
            repo.dir.path().join("group.toml"),
            repo.dir.path().join("other.toml"),
        )
        .unwrap();
        let (_root, store) = store();
        let error = plan(&store, vec![name("other")], &mut repo.source()).unwrap_err();
        assert!(matches!(
            error,
            PlanError::Source {
                source: LocalRepositoryError::WrongMetadata { .. },
                ..
            }
        ));
    }

    #[test]
    fn installed_dependencies_are_not_fetched() {
        let repo = Repo::new();
        repo.add("lib", &[]);
        repo.add("app", &["lib"]);
        let (_root, store) = store();
        let Request::Archive(lib, _) = repo.open("lib") else {
            unreachable!()
        };
        store.install(&lib, InstallOptions::default()).unwrap();
        let plan = plan(&store, vec![repo.open("app")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["app"]);
    }

    #[test]
    fn missing_dependencies_are_reported() {
        let repo = Repo::new();
        repo.add("app", &["gone", "lib"]);
        repo.add("lib", &["gone"]);
        let (_root, store) = store();
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
        let (_root, store) = store();
        let plan = plan(&store, vec![repo.open("a")], &mut repo.source()).unwrap();
        assert_eq!(names(&plan), ["b", "a"]);
    }

    #[test]
    fn no_source_installs_only_what_was_asked() {
        let repo = Repo::new();
        repo.add("lib", &[]);
        repo.add("app", &["lib"]);
        let (_root, store) = store();
        let plan = plan(&store, vec![repo.open("app")], &mut NoSource).unwrap();
        assert_eq!(names(&plan), ["app"]);
        assert_eq!(plan.missing.len(), 1);
    }

    #[test]
    fn duplicates_are_refused() {
        let repo = Repo::new();
        repo.add("a", &[]);
        let (_root, store) = store();
        for requested in [
            vec![repo.open("a"), repo.open("a")],
            vec![name("a"), repo.open("a")],
            vec![repo.open("a"), name("a")],
        ] {
            let error = plan(&store, requested, &mut repo.source()).unwrap_err();
            assert!(matches!(error, PlanError::Duplicate(n) if n.as_str() == "a"));
        }
    }

    #[test]
    fn misnamed_archives_are_refused() {
        let repo = Repo::new();
        repo.add("app", &["lib"]);
        let real = repo.add("other", &[]);
        std::fs::copy(&real, repo.dir.path().join("lib.pkgar")).unwrap();
        let (_root, store) = store();
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
        let (_root, store) = store();
        // `lib` is signed by the second repository's key only.
        let mut source = (first.source(), second.source());
        let plan = plan(&store, vec![first.open("app")], &mut source).unwrap();
        assert_eq!(names(&plan), ["lib", "app"]);
        let mut both = vec![first.source(), second.source()];
        let plan = super::plan(&store, vec![name("app")], &mut both).unwrap();
        assert_eq!(names(&plan), ["lib", "app"]);
    }
}
