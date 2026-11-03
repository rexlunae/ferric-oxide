//! Ferric Oxide: the Cargo inputs of an in-tree path source.
//!
//! A recipe with a `[source] path` snapshots only that directory, but the
//! in-tree Cargo packages in it also build from their `path` dependencies (for
//! example `tools/fox` from `crates/fox-*`) and from their workspace's root
//! manifest and lockfile. [`cargo_inputs`] finds those inputs so the build
//! cache check can compare their modification times against the staged
//! package.
//!
//! Every `Cargo.toml` in the path source counts, not only the top-level one,
//! so workspace members and separately built crates (such as
//! `userspace/base/bootstrap`) are covered without parsing `members` globs.
//! Build caches and `.git` directories are skipped as the snapshot skips them.
//!
//! The manifests are read directly rather than through `cargo metadata`: the
//! walk needs no toolchain, network or lockfile resolution, and it gives the
//! same answer on every run. It follows the rules of the
//! [Cargo manifest format](https://doc.rust-lang.org/cargo/reference/manifest.html):
//!
//! - `path` keys in `[dependencies]`, `[build-dependencies]` and their
//!   `[target.<cfg>.*]` forms, followed recursively. Optional dependencies
//!   count whatever features the recipe enables. `[dev-dependencies]` do not
//!   change the built package and are skipped.
//! - `<name>.workspace = true`, resolved through `[workspace.dependencies]` of
//!   the package's workspace root: the manifest named by `package.workspace`,
//!   or the nearest ancestor (including the package itself) whose manifest has
//!   a `[workspace]` table.
//! - `path` entries in the workspace root's `[patch.<source>]` tables.
//!
//! Each workspace root contributes its `Cargo.toml` and, when present, its
//! `Cargo.lock`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use toml::{Table, Value};
use walkdir::WalkDir;

use crate::cook::fs;

const MANIFEST: &str = "Cargo.toml";
const LOCKFILE: &str = "Cargo.lock";
const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "build-dependencies", "build_dependencies"];

/// Why the Cargo inputs of a path source could not be determined.
#[derive(Debug, thiserror::Error)]
pub enum CargoInputsError {
    /// A manifest or package directory could not be read.
    #[error("cannot read {}", .path.display())]
    Read {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The path source could not be searched for manifests.
    #[error("cannot search the path source for Cargo manifests")]
    Walk(#[from] walkdir::Error),
    /// A manifest is not valid TOML.
    #[error("cannot parse Cargo manifest {}", .path.display())]
    Parse {
        /// The manifest.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// A package inherits a dependency from a workspace, but has no
    /// workspace root.
    #[error("{} inherits {name:?} from a workspace, but has no workspace root", .manifest.display())]
    NoWorkspace {
        /// The package manifest.
        manifest: PathBuf,
        /// The inherited dependency.
        name: String,
    },
    /// A package inherits a dependency its workspace root does not declare.
    #[error("{} inherits {name:?}, which {} does not declare", .manifest.display(), .workspace.display())]
    MissingWorkspaceDependency {
        /// The package manifest.
        manifest: PathBuf,
        /// The workspace root manifest.
        workspace: PathBuf,
        /// The inherited dependency.
        name: String,
    },
}

/// The files and directories an in-tree Cargo package builds from.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CargoInputs {
    /// Package directories, canonicalized: those with a manifest in the path
    /// source and, recursively, their path dependencies.
    pub packages: BTreeSet<PathBuf>,
    /// Workspace root manifests and lockfiles, canonicalized.
    pub files: BTreeSet<PathBuf>,
}

impl CargoInputs {
    /// Returns the newest modification time among the inputs, or `None`
    /// when there are none. Package directories skip the build caches and
    /// `.git` directories a path-source snapshot skips.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if an input or its metadata cannot be read.
    pub fn newest_modified(&self) -> crate::Result<Option<SystemTime>> {
        let packages = self
            .packages
            .iter()
            .map(|dir| fs::modified_path_source(dir));
        let files = self.files.iter().map(|file| fs::modified(file));
        packages
            .chain(files)
            .try_fold(None, |newest, modified| Ok(newest.max(Some(modified?))))
    }
}

/// Finds the Cargo inputs of the packages in the path source `source_dir`.
///
/// Returns empty inputs when `source_dir` contains no `Cargo.toml`, since a
/// path source need not hold Cargo packages.
///
/// # Errors
///
/// Returns [`CargoInputsError::Walk`] if `source_dir` cannot be searched,
/// [`CargoInputsError::Read`] if a manifest or dependency directory
/// cannot be read, [`CargoInputsError::Parse`] if a manifest is not TOML, and
/// [`CargoInputsError::NoWorkspace`] or
/// [`CargoInputsError::MissingWorkspaceDependency`] if an inherited dependency
/// cannot be resolved. Cargo itself would reject such a package.
pub fn cargo_inputs(source_dir: &Path) -> Result<CargoInputs, CargoInputsError> {
    cargo_inputs_below(source_dir, Path::new("/"))
}

/// [`cargo_inputs`], searching for workspace roots only at or below the
/// canonical directory `ceiling`, so tests are independent of where their
/// temporary directory is.
fn cargo_inputs_below(source_dir: &Path, ceiling: &Path) -> Result<CargoInputs, CargoInputsError> {
    let mut inputs = CargoInputs::default();
    let mut walk = Walk {
        ceiling,
        queue: VecDeque::new(),
        workspaces: BTreeMap::new(),
    };
    let entries = WalkDir::new(source_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(fs::path_source_filter(source_dir));
    for entry in entries {
        let entry = entry?;
        if entry.file_name() == MANIFEST && entry.file_type().is_file() {
            let package = entry.path().parent().unwrap_or(source_dir);
            walk.queue.push_back(canonical(package)?);
        }
    }
    while let Some(dir) = walk.queue.pop_front() {
        if !inputs.packages.insert(dir.clone()) {
            continue;
        }
        walk.visit_package(&dir, &mut inputs)?;
    }
    Ok(inputs)
}

struct Walk<'a> {
    ceiling: &'a Path,
    queue: VecDeque<PathBuf>,
    /// Parsed workspace roots by directory, so each is read once.
    workspaces: BTreeMap<PathBuf, Table>,
}

impl Walk<'_> {
    fn visit_package(
        &mut self,
        dir: &Path,
        inputs: &mut CargoInputs,
    ) -> Result<(), CargoInputsError> {
        let manifest_path = dir.join(MANIFEST);
        let manifest = read_manifest(&manifest_path)?;
        let workspace_root = workspace_root(dir, &manifest, self.ceiling)?;
        if let Some(root) = &workspace_root {
            self.load_workspace(root, inputs)?;
        }
        for (name, spec) in dependencies(&manifest) {
            let Some(spec) = spec.as_table() else {
                continue;
            };
            if let Some(path) = path_key(spec) {
                self.queue.push_back(canonical(&dir.join(path))?);
            } else if spec.get("workspace").and_then(Value::as_bool) == Some(true) {
                let Some(root) = &workspace_root else {
                    return Err(CargoInputsError::NoWorkspace {
                        manifest: manifest_path,
                        name: name.clone(),
                    });
                };
                if let Some(path) = self.workspace_dependency_path(root, &manifest_path, name)? {
                    self.queue.push_back(canonical(&root.join(path))?);
                }
            }
        }
        Ok(())
    }

    /// Records a workspace root's files and queues its `[patch]` paths the
    /// first time the root is seen.
    fn load_workspace(
        &mut self,
        root: &Path,
        inputs: &mut CargoInputs,
    ) -> Result<(), CargoInputsError> {
        if self.workspaces.contains_key(root) {
            return Ok(());
        }
        let manifest_path = root.join(MANIFEST);
        let manifest = read_manifest(&manifest_path)?;
        inputs.files.insert(manifest_path);
        let lockfile = root.join(LOCKFILE);
        if lockfile.is_file() {
            inputs.files.insert(lockfile);
        }
        let patches = manifest
            .get("patch")
            .and_then(Value::as_table)
            .into_iter()
            .flat_map(Table::values)
            .filter_map(Value::as_table)
            .flat_map(Table::values)
            .filter_map(Value::as_table)
            .filter_map(path_key);
        for path in patches {
            self.queue.push_back(canonical(&root.join(path))?);
        }
        self.workspaces.insert(root.to_path_buf(), manifest);
        Ok(())
    }

    fn workspace_dependency_path(
        &self,
        root: &Path,
        manifest: &Path,
        name: &str,
    ) -> Result<Option<&str>, CargoInputsError> {
        let spec = self
            .workspaces
            .get(root)
            .and_then(|workspace| workspace.get("workspace"))
            .and_then(|workspace| workspace.get("dependencies"))
            .and_then(|dependencies| dependencies.get(name))
            .ok_or_else(|| CargoInputsError::MissingWorkspaceDependency {
                manifest: manifest.to_path_buf(),
                workspace: root.join(MANIFEST),
                name: name.to_owned(),
            })?;
        Ok(spec.as_table().and_then(path_key))
    }
}

fn read_manifest(path: &Path) -> Result<Table, CargoInputsError> {
    let text = std::fs::read_to_string(path).map_err(|source| CargoInputsError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    text.parse().map_err(|source| CargoInputsError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn canonical(path: &Path) -> Result<PathBuf, CargoInputsError> {
    path.canonicalize()
        .map_err(|source| CargoInputsError::Read {
            path: path.to_path_buf(),
            source,
        })
}

fn path_key(spec: &Table) -> Option<&str> {
    spec.get("path").and_then(Value::as_str)
}

/// The workspace root directory of the canonical package directory `dir`,
/// following Cargo's discovery rules.
fn workspace_root(
    dir: &Path,
    manifest: &Table,
    ceiling: &Path,
) -> Result<Option<PathBuf>, CargoInputsError> {
    if manifest.contains_key("workspace") {
        return Ok(Some(dir.to_path_buf()));
    }
    if let Some(root) = manifest
        .get("package")
        .and_then(|package| package.get("workspace"))
        .and_then(Value::as_str)
    {
        return canonical(&dir.join(root)).map(Some);
    }
    for ancestor in dir
        .ancestors()
        .skip(1)
        .take_while(|ancestor| ancestor.starts_with(ceiling))
    {
        let candidate = ancestor.join(MANIFEST);
        if candidate.is_file() && read_manifest(&candidate)?.contains_key("workspace") {
            return Ok(Some(ancestor.to_path_buf()));
        }
    }
    Ok(None)
}

/// Every dependency that affects the built package: normal and build
/// dependencies, including target-specific ones.
fn dependencies(manifest: &Table) -> impl Iterator<Item = (&String, &Value)> {
    let targets = manifest
        .get("target")
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(Table::values)
        .filter_map(Value::as_table);
    std::iter::once(manifest)
        .chain(targets)
        .flat_map(|table| DEPENDENCY_TABLES.iter().filter_map(|key| table.get(*key)))
        .filter_map(Value::as_table)
        .flat_map(Table::iter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn package(root: &Path, relative: &str, dependencies: &str) {
        write(
            root,
            &format!("{relative}/Cargo.toml"),
            &format!("[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n{dependencies}"),
        );
    }

    fn canon(root: &Path, relative: &str) -> PathBuf {
        root.join(relative).canonicalize().unwrap()
    }

    fn inputs(root: &Path, relative: &str) -> Result<CargoInputs, CargoInputsError> {
        cargo_inputs_below(&root.join(relative), &root.canonicalize().unwrap())
    }

    #[test]
    fn non_cargo_source_has_no_inputs() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(inputs(temp.path(), ".").unwrap(), CargoInputs::default());
    }

    #[test]
    fn follows_direct_workspace_target_and_patch_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "Cargo.toml",
            "[workspace]\nmembers = []\n\
             [workspace.dependencies]\nshared = { path = \"crates/shared\" }\nserde = \"1\"\n\
             [patch.crates-io]\npatched = { path = \"crates/patched\" }\n",
        );
        write(root, "Cargo.lock", "");
        package(
            root,
            "tools/app",
            "[dependencies]\ncore = { path = \"../../crates/core\", optional = true }\n\
             shared.workspace = true\nserde.workspace = true\nlog = \"0.4\"\n\
             [target.'cfg(target_os = \"redox\")'.dependencies]\nos = { path = \"../../crates/os\" }\n\
             [build-dependencies]\ngen = { path = \"../../crates/gen\" }\n\
             [dev-dependencies]\nfixture = { path = \"../../crates/fixture\" }\n",
        );
        package(
            root,
            "crates/core",
            "[dependencies]\ninner = { path = \"../inner\" }\n",
        );
        package(
            root,
            "crates/inner",
            "[dependencies]\ncore = { path = \"../core\" }\n",
        );
        for leaf in ["shared", "os", "gen", "patched", "fixture"] {
            package(root, &format!("crates/{leaf}"), "");
        }

        let found = inputs(root, "tools/app").unwrap();

        let expected: BTreeSet<_> = [
            "tools/app",
            "crates/core",
            "crates/inner",
            "crates/shared",
            "crates/os",
            "crates/gen",
            "crates/patched",
        ]
        .iter()
        .map(|dir| canon(root, dir))
        .collect();
        assert_eq!(found.packages, expected);
        assert_eq!(
            found.files,
            BTreeSet::from([canon(root, "Cargo.toml"), canon(root, "Cargo.lock")])
        );
    }

    #[test]
    fn every_package_in_the_source_counts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "base/Cargo.toml",
            "[workspace]\nmembers = [\"drivers/*\"]\nexclude = [\"bootstrap\"]\n",
        );
        package(
            root,
            "base/drivers/pci",
            "[dependencies]\niommu = { path = \"../../../iommu\" }\n",
        );
        package(
            root,
            "base/bootstrap",
            "[dependencies]\nabi = { path = \"../../abi\" }\n",
        );
        package(
            root,
            "base/target/debug/build/cached",
            "[dependencies]\ngone = { path = \"gone\" }\n",
        );
        write(root, "base/target/CACHEDIR.TAG", "");
        package(root, "iommu", "");
        package(root, "abi", "");

        let found = inputs(root, "base").unwrap();
        let expected: BTreeSet<_> = ["base/drivers/pci", "base/bootstrap", "iommu", "abi"]
            .iter()
            .map(|dir| canon(root, dir))
            .chain([canon(root, "base")])
            .collect();
        assert_eq!(found.packages, expected);
        assert_eq!(
            found.files,
            BTreeSet::from([canon(root, "base/Cargo.toml")])
        );
    }

    #[test]
    fn explicit_workspace_key_and_standalone_packages() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            root,
            "ws/Cargo.toml",
            "[workspace]\n[workspace.dependencies]\nlib = { path = \"../lib\" }\n",
        );
        write(
            root,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nworkspace = \"../ws\"\n[dependencies]\nlib.workspace = true\n",
        );
        package(root, "lib", "");

        let found = inputs(root, "app").unwrap();
        assert_eq!(
            found.packages,
            BTreeSet::from([canon(root, "app"), canon(root, "lib")])
        );
        assert_eq!(found.files, BTreeSet::from([canon(root, "ws/Cargo.toml")]));

        let found = inputs(root, "lib").unwrap();
        assert_eq!(found.packages, BTreeSet::from([canon(root, "lib")]));
        assert!(found.files.is_empty());
    }

    /// The in-tree recipes this exists for: fox and the installer build from
    /// the fox crates and the host workspace's lockfile.
    #[test]
    fn in_tree_fox_recipes_track_the_fox_crates() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for (tool, crates) in [
            ("tools/fox", &["fox-core", "fox-store", "fox-git"][..]),
            ("tools/installer", &["fox-core", "fox-store"][..]),
        ] {
            let found = cargo_inputs(&repo.join(tool)).unwrap();
            for name in crates {
                assert!(
                    found
                        .packages
                        .contains(&canon(&repo, &format!("crates/{name}"))),
                    "{tool} depends on {name}: {found:?}"
                );
            }
            assert!(found.files.contains(&canon(&repo, "Cargo.lock")), "{tool}");
            assert!(found.newest_modified().unwrap().is_some());
        }
        let base = cargo_inputs(&repo.join("userspace/base")).unwrap();
        assert!(
            base.packages.contains(&canon(&repo, "crates/iommu")),
            "{base:?}"
        );
        for source in ["crates/kernel", "crates/bootloader", "userspace/redoxfs"] {
            cargo_inputs(&repo.join(source)).unwrap();
        }
    }

    #[test]
    fn unresolvable_dependencies_are_errors() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        package(
            root,
            "missing",
            "[dependencies]\ngone = { path = \"../gone\" }\n",
        );
        assert!(matches!(
            inputs(root, "missing"),
            Err(CargoInputsError::Read { .. })
        ));

        package(root, "orphan", "[dependencies]\nlib.workspace = true\n");
        assert!(matches!(
            inputs(root, "orphan"),
            Err(CargoInputsError::NoWorkspace { name, .. }) if name == "lib"
        ));

        write(root, "ws/Cargo.toml", "[workspace]\n");
        package(root, "ws/member", "[dependencies]\nlib.workspace = true\n");
        assert!(matches!(
            inputs(root, "ws/member"),
            Err(CargoInputsError::MissingWorkspaceDependency { name, .. }) if name == "lib"
        ));

        write(root, "bad/Cargo.toml", "[package");
        assert!(matches!(
            inputs(root, "bad"),
            Err(CargoInputsError::Parse { .. })
        ));
    }
}
