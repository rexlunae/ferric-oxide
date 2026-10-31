use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use fox_core::cookbook::RecipeName;

use crate::PackageError;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{WALK_DEPTH, bail_other_err, cook::package as cook_package, staged_pkg};

/// Specifies how to download the source for a recipe
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SourceRecipe {
    /// Reuse the source directory of another package
    ///
    /// This is useful when a single source repo contains multiple projects which each have their
    /// own recipe to build them.
    SameAs {
        /// Relative path to the package for which to reuse the source dir
        same_as: String,
    },
    /// Path source
    Path {
        /// The path to the source
        path: String,
    },
    /// A git repository source
    Git {
        /// The URL for the git repository, such as <https://gitlab.redox-os.org/redox-os/ion.git>
        git: String,
        /// The URL for an upstream repository
        upstream: Option<String>,
        /// The optional branch of the git repository to track, such as master. Please specify to
        /// make updates to the rev easier
        branch: Option<String>,
        /// The optional revision of the git repository to use for builds. Please specify for
        /// reproducible builds
        rev: Option<String>,
        /// The optional config to clone with treeless clone. Default is true if "rev" added
        shallow_clone: Option<bool>,
        /// A list of patch files to apply to the source
        #[serde(default)]
        patches: Vec<String>,
        /// Optional script to run to prepare the source
        script: Option<String>,
    },
    /// A tar file source
    Tar {
        /// The URL of a tar source
        tar: String,
        /// The optional blake3 sum of the tar file. Please specify this to make reproducible
        /// builds more reliable
        blake3: Option<String>,
        /// A list of patch files to apply to the source
        #[serde(default)]
        patches: Vec<String>,
        /// Optional script to run to prepare the source, such as ./autogen.sh
        script: Option<String>,
    },
}

/// Specifies how to build a recipe
#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
#[serde(tag = "template")]
#[derive(Default)]
pub enum BuildKind {
    /// Will not build (for meta packages)
    #[serde(rename = "none")]
    #[default]
    None,
    /// Will download compiled package from remote
    #[serde(rename = "remote")]
    Remote,
    /// Will build and install using cargo
    #[serde(rename = "cargo")]
    Cargo {
        /// Path relative to source dir, where to run Cargo
        #[serde(default)]
        cargopath: Option<String>,
        /// Additional flags to cargo
        #[serde(default)]
        cargoflags: Vec<String>,
        /// List of packages to build, if Cargo.toml is a workspace.
        /// Binary names will not be prefixed.
        #[serde(default)]
        cargopackages: Vec<String>,
        /// List of examples to build, if there's an "examples" dir.
        /// Binary names will be prefixed.
        #[serde(default)]
        cargoexamples: Vec<String>,
        /// whether to remove --locked, common for libraries.
        #[serde(default)]
        clearlocked: bool,
        /// whether to force prefix packages with recipe name
        #[serde(default)]
        cargopackagesprefixed: bool,
    },
    /// Will build and install using configure and make
    #[serde(rename = "configure")]
    Configure {
        #[serde(default)]
        configureflags: Vec<String>,
    },
    /// Will build and install using cmake
    #[serde(rename = "cmake")]
    Cmake {
        #[serde(default)]
        cmakeflags: Vec<String>,
    },
    /// Will build and install using meson
    #[serde(rename = "meson")]
    Meson {
        #[serde(default)]
        mesonflags: Vec<String>,
    },
    /// Will build and install using python pip
    #[serde(rename = "python")]
    Python {
        #[serde(default)]
        pyprojectpath: Option<String>,
        #[serde(default)]
        pipflags: Vec<String>,
        /// Some recipes need to run setup.py,
        /// other does not allow running setup.py.
        /// This defaults to not run setup.py, but if build result in error,
        /// like "invalid command 'dist_info'", set this on.
        #[serde(default)]
        legacysetup: bool,
    },
    /// Will build and install using custom commands
    #[serde(rename = "custom")]
    Custom { script: String },
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct BuildRecipe {
    #[serde(flatten)]
    pub kind: BuildKind,
    pub dependencies: Vec<RecipeName>,
    pub compiler: Option<BuildCompiler>,
    #[serde(rename = "dev-dependencies")]
    pub dev_dependencies: Vec<RecipeName>,
    /// Ferric Oxide: where this recipe runs when another recipe uses it as a
    /// build tool (`host:`).
    #[serde(rename = "tool-runs-on")]
    pub tool_runs_on: ToolRunsOn,
}

/// Where a recipe runs when it is a build tool (a `host:` dependency).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolRunsOn {
    /// Wherever `build.build_tools` puts build tools: in the guest build
    /// environment, or on the host.
    #[default]
    Configured,
    /// Always built for and run on the host. For a tool that cannot work from
    /// a guest: one whose own cross build needs a runnable copy of itself, as
    /// CPython's does, or one that build scripts drive with environment
    /// variables and module trees that a guest command does not carry.
    Host,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct PackageRecipe {
    pub dependencies: Vec<RecipeName>,
    pub version: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct OptionalPackageRecipe {
    pub name: String,
    pub dependencies: Vec<RecipeName>,
    pub files: Vec<String>,
}

/// Everything required to build a Redox package
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Recipe {
    /// Specifies how to download the source for this recipe
    pub source: Option<SourceRecipe>,
    /// Specifies how to build this recipe
    pub build: BuildRecipe,
    /// Specifies how to package this recipe
    pub package: PackageRecipe,
    /// Specifies optional packages based from this recipe
    #[serde(rename = "optional-packages")]
    pub optional_packages: Vec<OptionalPackageRecipe>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
pub enum BuildCompiler {
    #[serde(rename = "gcc")]
    Gcc,
    #[serde(rename = "clang")]
    Clang,
    #[serde(rename = "gcc-no-lto")]
    GccNoLTO,
    #[serde(rename = "clang-no-lto")]
    ClangNoLTO,
    #[serde(rename = "no-lto")]
    NoLTO,
}

impl BuildRecipe {
    pub fn new(kind: BuildKind) -> Self {
        let mut build = Self::default();
        build.kind = kind;
        build
    }

    pub fn set_as_remote(&mut self) {
        if self.kind == BuildKind::None {
            // BuildKind::Remote won't handle remote meta-packages
            return;
        }
        self.kind = BuildKind::Remote;
        self.dev_dependencies = Vec::new();
    }

    pub fn set_as_none(&mut self) {
        self.kind = BuildKind::None;
        self.dependencies = Vec::new();
        self.dev_dependencies = Vec::new();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CookRecipe {
    pub name: RecipeName,
    pub dir: PathBuf,
    pub recipe: Recipe,
    pub target: &'static str,
    /// If false, it's listed on install config
    pub is_deps: bool,
    pub rule: String,
    /// whether if this recipe is pinned from cookbook.lock
    pub pinned: bool,
}

/// Where recipe `name`, built from `build`, runs as a build tool.
///
/// A recipe that build-depends on itself (`host:<name>`) cross-builds with a
/// host build of itself: CPython, ICU, groff and zig do. A copy built for the
/// target could never satisfy that, so such a recipe runs on the host whatever
/// its `tool-runs-on` says. Python modules also run inside the host build
/// interpreter, including their compiled extensions. Otherwise the field decides.
fn tool_runs_on(name: &str, build: &BuildRecipe) -> ToolRunsOn {
    let builds_with_itself = build
        .dev_dependencies
        .iter()
        .chain(&build.dependencies)
        .any(|dependency| dependency.is_host() && dependency.without_prefix() == name);
    if builds_with_itself || matches!(build.kind, BuildKind::Python { .. }) {
        ToolRunsOn::Host
    } else {
        build.tool_runs_on
    }
}

/// How a `host:` recipe's dependency `dependency` builds, or `None` to drop it.
///
/// `owner` is the recipe's own name without its selector, and
/// `tools_are_target` is [`crate::build_tools_in_guest`]: whether the recipe is
/// itself built for the target.
///
/// * `host:` of itself is dropped: a tool may list itself (`python312` builds
///   with its own interpreter), and keeping that edge makes the dependency walk
///   descend until it hits the depth limit.
/// * `target:` is dropped for a host tool, which cannot link a target package.
///   For a guest-built tool the pin is dropped but the package stays: the
///   dependency keeps following the recipe's own build, which for a guest-built
///   tool is the target build its libraries are usable from. Dropping the
///   package outright would lose the target libraries it needs.
/// * a `host:` dependency of another recipe stays as written: its own
///   placement decides whether it runs on the host or in the guest.
/// * an unprefixed one is a target package, so a guest-built tool keeps it as
///   written and a host tool's moves to the host build. Leaving it unprefixed
///   for a host tool would have it link the target libraries (`libgmp`,
///   `libmpfr` for a host `binutils`) that the host binary cannot use.
fn map_tool_dependency(
    dependency: RecipeName,
    owner: &str,
    tools_are_target: bool,
) -> Option<RecipeName> {
    if dependency.is_host() {
        if dependency.recipe() == owner {
            None
        } else {
            Some(dependency)
        }
    } else if dependency.is_target() {
        if tools_are_target {
            RecipeName::new(dependency.without_prefix()).ok()
        } else {
            None
        }
    } else if tools_are_target {
        Some(dependency)
    } else {
        Some(dependency.with_host())
    }
}

impl Recipe {
    pub fn new(file: &PathBuf) -> Result<Recipe, PackageError> {
        if !file.is_file() {
            return Err(PackageError::FileMissing(file.clone()));
        }
        let toml = fs::read_to_string(file).map_err(|source| PackageError::FileError {
            path: file.clone(),
            source,
        })?;
        let recipe: Recipe = toml::from_str(&toml).map_err(|source| PackageError::Parse {
            path: file.clone(),
            source,
        })?;
        Ok(recipe)
    }

    pub fn get_packages_list(&self) -> Vec<Option<&OptionalPackageRecipe>> {
        let mut packages: Vec<Option<&OptionalPackageRecipe>> =
            self.optional_packages.iter().map(Some).collect();
        // the mandatory package, put last because of cook_build
        packages.push(None);
        packages
    }
}

impl CookRecipe {
    /// Whether this is a build tool (`host:`) built for the target and run in
    /// the guest build environment, rather than built for the host.
    pub fn runs_in_guest(&self) -> bool {
        self.name.is_host() && crate::tool_runs_in_guest(self.recipe.build.tool_runs_on)
    }

    pub fn new(name: RecipeName, dir: PathBuf, mut recipe: Recipe) -> Result<Self, PackageError> {
        recipe.build.tool_runs_on = tool_runs_on(name.without_prefix(), &recipe.build);
        let target = cook_package::package_target(&name, recipe.build.tool_runs_on);
        if name.is_host() {
            let tools_are_target = crate::tool_runs_in_guest(recipe.build.tool_runs_on);
            let fn_map =
                |p: RecipeName| map_tool_dependency(p, name.without_prefix(), tools_are_target);
            recipe.build.dependencies = recipe
                .build
                .dependencies
                .into_iter()
                .filter_map(fn_map)
                .collect();
            recipe.build.dev_dependencies = recipe
                .build
                .dev_dependencies
                .into_iter()
                .filter_map(fn_map)
                .collect();
        }
        Ok(Self {
            name,
            dir,
            recipe,
            target,
            is_deps: false,
            rule: "".into(),
            pinned: false,
        })
    }

    pub fn dummy(name: &RecipeName) -> Self {
        Self {
            dir: PathBuf::new(),
            is_deps: false,
            name: name.clone(),
            recipe: Recipe::default(),
            target: redoxer::target(),
            pinned: false,
            rule: "".into(),
        }
    }

    pub fn from_name(name: RecipeName) -> Result<Self, PackageError> {
        let dir = staged_pkg::find(name.recipe())
            .ok_or_else(|| PackageError::PackageNotFound(name.clone()))?;
        let file = dir.join("recipe.toml");
        let recipe = Recipe::new(&file)?;
        Self::new(name, dir.to_path_buf(), recipe)
    }

    pub fn from_list(names: Vec<RecipeName>) -> Result<Vec<Self>, PackageError> {
        let mut packages = Vec::new();
        for name in names {
            packages.push(Self::from_name(name)?);
        }
        Ok(packages)
    }

    pub fn from_path(dir: &Path, read_recipe: bool, is_host: bool) -> Result<Self, PackageError> {
        let file = dir.join("recipe.toml");
        let mut name = RecipeName::new(dir.file_name().unwrap().to_string_lossy())?;
        if is_host {
            name = name.with_host();
        }
        let recipe = if read_recipe {
            Recipe::new(&file)?
        } else {
            // clean/unfetch don't need to read recipe
            Recipe::default()
        };
        Self::new(name, dir.to_path_buf(), recipe)
    }

    fn new_recursive(
        names: &[RecipeName],
        recurse_build_deps: bool,
        recurse_dev_build_deps: bool,
        recurse_package_deps: bool,
        collect_build_deps: bool,
        collect_package_deps: bool,
        collect_self: bool,
        recursion: usize,
    ) -> Result<Vec<Self>, PackageError> {
        if recursion == 0 {
            return Err(PackageError::Recursion(Default::default()));
        }

        let mut recipes = Vec::new();
        let mut recipes_set = BTreeSet::new();
        for name in names {
            let recipe = Self::from_name(name.clone())?;

            if recurse_build_deps {
                let dependencies = Self::new_recursive(
                    &recipe.recipe.build.dependencies,
                    recurse_build_deps,
                    recurse_dev_build_deps,
                    recurse_package_deps,
                    collect_build_deps,
                    collect_package_deps,
                    collect_build_deps,
                    recursion - 1,
                )
                .map_err(|mut err| {
                    err.append_recursion(name);
                    err
                })?;

                for dependency in dependencies {
                    if !recipes_set.contains(&dependency.name) {
                        recipes_set.insert(dependency.name.clone());
                        recipes.push(dependency);
                    }
                }
            }

            if recurse_dev_build_deps {
                let dependencies = Self::new_recursive(
                    &recipe.recipe.build.dev_dependencies,
                    recurse_build_deps,
                    recurse_dev_build_deps,
                    recurse_package_deps,
                    collect_build_deps,
                    collect_package_deps,
                    collect_build_deps,
                    recursion - 1,
                )
                .map_err(|mut err| {
                    err.append_recursion(name);
                    err
                })?;

                for dependency in dependencies {
                    if !recipes_set.contains(&dependency.name) {
                        recipes_set.insert(dependency.name.clone());
                        recipes.push(dependency);
                    }
                }
            }

            if recurse_package_deps {
                let dependencies = Self::new_recursive(
                    &recipe.recipe.package.dependencies,
                    recurse_build_deps,
                    recurse_dev_build_deps,
                    recurse_package_deps,
                    collect_build_deps,
                    collect_package_deps,
                    collect_package_deps,
                    recursion - 1,
                )
                .map_err(|mut err| {
                    err.append_recursion(name);
                    err
                })?;

                for dependency in dependencies {
                    if !recipes_set.contains(&dependency.name) {
                        recipes_set.insert(dependency.name.clone());
                        recipes.push(dependency);
                    }
                }
            }

            if collect_self && !recipes_set.contains(&recipe.name) {
                recipes_set.insert(recipe.name.clone());
                recipes.push(recipe);
            }
        }

        Ok(recipes)
    }

    pub fn get_build_deps_recursive(
        names: &[RecipeName],
        include_dev: bool,
    ) -> Result<Vec<Self>, PackageError> {
        let packages = Self::new_recursive(
            names,
            true,
            include_dev,
            false,
            true,
            false,
            true,
            WALK_DEPTH,
        )?;

        Ok(packages)
    }

    pub fn get_package_deps_recursive(
        names: &[RecipeName],
        include_names: bool,
    ) -> Result<Vec<RecipeName>, PackageError> {
        // recurse_build_deps == true here as libraries (build deps) can have runtime files (package deps)
        let packages = Self::new_recursive(
            names,
            true,
            false,
            true,
            false,
            true,
            include_names,
            WALK_DEPTH,
        )?;

        Ok(packages.into_iter().map(|p| p.name).collect())
    }

    pub fn get_all_deps_names_recursive(
        names: &[RecipeName],
        include_dev: bool,
    ) -> Result<Vec<RecipeName>, PackageError> {
        let packages =
            Self::new_recursive(names, true, include_dev, true, true, true, true, WALK_DEPTH)?;

        Ok(packages.into_iter().map(|p| p.name).collect())
    }

    /// reload self back to the recipe.toml, with preserving effect from `self.rule`
    pub fn reload_recipe(&mut self) -> Result<(), PackageError> {
        if self.pinned {
            // TODO: print?
            return Ok(());
        }
        self.recipe = Self::from_path(&self.dir, true, self.name.is_host())?.recipe;
        let _ = self.apply_filesystem_config(&self.rule.clone());
        Ok(())
    }

    /// returns stage dir, pkgar file and toml file.
    pub fn stage_paths(&self) -> (PathBuf, PathBuf, PathBuf) {
        let r = self.name.suffix().map(|p| OptionalPackageRecipe {
            name: p.to_string(),
            ..Default::default()
        });
        cook_package::package_stage_paths(r.as_ref(), &self.target_dir())
    }

    /// returns target dir which is depending on the "host:" prefix
    pub fn target_dir(&self) -> PathBuf {
        self.dir.join("target").join(self.target)
    }

    pub fn apply_filesystem_config(&mut self, rule: &str) -> crate::Result<()> {
        match rule {
            // build from source as usual
            "source" => {}
            // keep local changes
            "local" => self.recipe.source = None,
            // A package server only has its own builds of code kept in this
            // repository, never this checkout's, so those build from source.
            "binary" if self.is_in_tree() => return self.apply_filesystem_config("source"),
            // download from remote build
            "binary" => {
                self.recipe.source = None;
                self.recipe.build.set_as_remote();
            }
            // don't build this recipe (unlikely to go here unless some deps need it)
            // TODO: Note that we're assuming this being ignored from e.g. metapackages
            // TODO: Will totally broke build if this recipe needed as some other build dependencies
            "ignore" => {
                self.recipe.source = None;
                self.recipe.build.set_as_none();
            }
            rule => {
                bail_other_err!(
                    "Invalid pkg config {} = {:?}\nExpecting either 'source', 'local', 'binary' or 'ignore'",
                    self.name.as_str(),
                    rule
                );
            }
        }
        self.rule = rule.to_string();

        Ok(())
    }

    /// Whether this recipe builds code kept in this repository: its source
    /// is a `path`, or a `same_as` recipe whose source is.
    ///
    /// Such recipes are always built from source, even under the `binary`
    /// rule, because a package server cannot have this checkout's build.
    pub fn is_in_tree(&self) -> bool {
        match &self.recipe.source {
            Some(SourceRecipe::Path { .. }) => true,
            Some(SourceRecipe::SameAs { same_as }) => {
                Recipe::new(&self.dir.join(same_as).join("recipe.toml"))
                    .is_ok_and(|canon| matches!(canon.source, Some(SourceRecipe::Path { .. })))
            }
            _ => false,
        }
    }

    /// Whether the prebuilt archive and metadata of every package this
    /// recipe makes are already downloaded, as offline mode needs them.
    pub fn has_downloaded_binaries(&self) -> bool {
        let target_dir = self.dir.join("target").join(self.target);
        self.recipe.get_packages_list().into_iter().all(|package| {
            let (_, pkgar, toml) = crate::cook::package::package_source_paths(package, &target_dir);
            pkgar.is_file() && toml.is_file()
        })
    }

    pub fn guess_version(&self) -> Option<String> {
        let recipe = &self.recipe;
        if recipe.build.kind == BuildKind::None {
            return Some("".into()); // signifies a meta package
        } else if let Some(v) = &recipe.package.version {
            return Some(v.to_string());
        }

        let re = VersionExtractor::new();
        let mut dir = self.dir.to_path_buf();
        if let Some(r) = &recipe.source {
            match r {
                SourceRecipe::Tar {
                    tar,
                    blake3: _,
                    patches: _,
                    script: _,
                } => {
                    if let Some(ver) = re.extract_ver(tar) {
                        return Some(ver);
                    }
                }
                SourceRecipe::Git {
                    git: _,
                    upstream: _,
                    branch,
                    rev,
                    shallow_clone: _,
                    patches: _,
                    script: _,
                } => {
                    if let Some(rev) = rev
                        && let Some(ver) = re.extract_ver(rev)
                    {
                        return Some(ver);
                    }
                    if let Some(branch) = branch
                        && let Some(ver) = re.extract_ver(branch)
                    {
                        return Some(ver);
                    }
                }
                SourceRecipe::SameAs { same_as } => {
                    dir = self.dir.join(same_as);
                }
                _ => {}
            }
        };

        let cargo_path = dir.join("source/Cargo.toml");
        if let Some(ver) = VersionExtractor::extract_cargo_ver(&cargo_path) {
            return Some(ver);
        }
        None
    }

    /// Get the name for recipe that is used as the build source
    pub fn canon_recipe_name(&self) -> RecipeName {
        // TODO: I hope nobody is nesting same_as...
        if let Some(SourceRecipe::SameAs { same_as }) = &self.recipe.source {
            if let Some(name) = Path::new(&same_as)
                .file_name()
                .and_then(|p| p.to_str())
                .and_then(|p| RecipeName::new(p).ok())
            {
                return name;
            } else {
                eprintln!(
                    "canon_recipe_name(): the same_as path for recipe {} is not valid",
                    self.name.as_str()
                )
            }
        }

        self.name.clone()
    }
}

// TODO: Wrap these vectors in a struct

pub fn recipes_mark_as_deps(names: &[RecipeName], packages: &mut Vec<CookRecipe>) {
    for package in packages.iter_mut() {
        package.is_deps = !names.contains(&package.name);
    }
}

pub fn recipes_flatten_package_names(packages: Vec<CookRecipe>) -> Vec<CookRecipe> {
    let mut new_packages = Vec::new();
    let mut packages_set = BTreeSet::new();
    for mut package in packages {
        let is_host = package.name.is_host();
        let mut name = package.name.with_suffix(None);
        if is_host {
            name = name.with_host();
        }
        if !packages_set.contains(name.as_str()) {
            packages_set.insert(name.to_string());
            package.name = name;
            new_packages.push(package);
        }
    }
    new_packages
}
/// Fail early if a recipe configured as `ignore` is still required as a
/// build dependency of another recipe that will be built.
pub fn validate_filesystem_rules(recipes: &[CookRecipe]) -> crate::Result<()> {
    let ignored: BTreeSet<String> = recipes
        .iter()
        .filter(|r| r.rule == "ignore")
        .map(|r| r.canon_recipe_name().without_prefix().to_string())
        .collect();
    if ignored.is_empty() {
        return Ok(());
    }
    for recipe in recipes {
        if recipe.rule == "ignore" {
            continue;
        }
        if recipe.recipe.build.kind == BuildKind::None
            || recipe.recipe.build.kind == BuildKind::Remote
        {
            // Not built from source, so build dependencies are irrelevant.
            continue;
        }
        let mut ignored_dependencies: Vec<&str> = Vec::new();
        for dep in recipe
            .recipe
            .build
            .dependencies
            .iter()
            .chain(recipe.recipe.build.dev_dependencies.iter())
        {
            let dep = dep.without_prefix();

            if ignored.contains(dep) {
                ignored_dependencies.push(dep);
            }
        }
        if !ignored_dependencies.is_empty() {
            bail_other_err!(
                "Invalid filesystem config: the following packages are configured as \"ignore\" but are required as build dependencies by {:?}: {:?}",
                ignored_dependencies,
                recipe.name.as_str(),
            );
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct AutoDeps {
    pub packages: BTreeSet<RecipeName>,
}

pub struct VersionExtractor {
    regex: Regex,
}

impl Default for VersionExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl VersionExtractor {
    pub fn new() -> Self {
        Self {
            regex: Regex::new(r"\d+(\.\d+){1,2}").unwrap(),
        }
    }
    pub fn extract_ver(&self, text: &str) -> Option<String> {
        if let Some(arm) = self.regex.captures(text) {
            return Some(arm.get(0)?.as_str().to_string());
        }
        None
    }
    fn extract_cargo_ver(path: &Path) -> Option<String> {
        let content = std::fs::read_to_string(path).ok()?;
        let manifest = content.parse::<toml::Table>().ok()?;

        if let Some(version) = manifest
            .get("package")
            .and_then(|pkg| pkg.get("version"))
            .and_then(|v| v.as_str())
        {
            return Some(version.to_string());
        }

        if let Some(version) = manifest
            .get("workspace")
            .and_then(|ws| ws.get("package"))
            .and_then(|pkg| pkg.get("version"))
            .and_then(|v| v.as_str())
        {
            return Some(version.to_string());
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use fox_core::cookbook::RecipeName;

    /// `python312` lists itself as a build tool because it builds with its own
    /// interpreter. Keeping that edge makes the dependency walk descend until
    /// it hits the depth limit, which is what broke the tool list: it must be
    /// dropped however the tool is built, not only for a host build.
    #[test]
    fn a_tool_that_lists_itself_drops_that_dependency() {
        use crate::recipe::{BuildRecipe, Recipe};

        let name = RecipeName::new("python312").unwrap().with_host();
        let recipe = Recipe {
            build: BuildRecipe {
                dev_dependencies: vec![name.clone()],
                ..Default::default()
            },
            ..Default::default()
        };
        let cooked = super::CookRecipe::new(name, std::path::PathBuf::from("."), recipe).unwrap();
        assert!(
            cooked.recipe.build.dev_dependencies.is_empty(),
            "a tool's self-reference is dropped from its own dependencies"
        );
    }

    /// A guest-built tool is a target binary, so its dependencies have to be
    /// target packages: an unprefixed dependency stays unprefixed (that is what
    /// makes it a target package), and a `target:` pin keeps its package and
    /// only loses the pin, so the library it names is still linked.
    #[test]
    fn a_recipe_that_builds_with_itself_runs_on_the_host() {
        let build = |toml: &str| toml::from_str::<super::Recipe>(toml).unwrap().build;
        let python =
            build("[build]\ntemplate = \"none\"\ndev-dependencies = [\"host:python312\"]\n");
        assert_eq!(
            super::tool_runs_on("python312", &python),
            super::ToolRunsOn::Host
        );
        // Another tool's host build is just a tool, and leaves the choice alone.
        let xcb = build("[build]\ntemplate = \"none\"\ndev-dependencies = [\"host:python312\"]\n");
        assert_eq!(
            super::tool_runs_on("xcb-proto", &xcb),
            super::ToolRunsOn::Configured
        );
        let marked = build("[build]\ntemplate = \"none\"\ntool-runs-on = \"host\"\n");
        assert_eq!(
            super::tool_runs_on("perl", &marked),
            super::ToolRunsOn::Host
        );
    }

    /// Build modules must have the ABI of the interpreter that imports them.
    #[test]
    fn python_build_modules_and_their_libraries_stay_on_the_host() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in [
            "python3-yaml",
            "python3-cython",
            "python3-setuptools",
            "python3-vcs-versioning",
        ] {
            let path = root.join("recipes/wip/dev/python").join(name);
            let recipe = super::Recipe::new(&path.join("recipe.toml")).unwrap();
            let cooked =
                super::CookRecipe::new(RecipeName::new(name).unwrap().with_host(), path, recipe)
                    .unwrap();
            assert_eq!(
                cooked.recipe.build.tool_runs_on,
                super::ToolRunsOn::Host,
                "{name}"
            );
            assert_eq!(cooked.target, redoxer::host_target(), "{name}");
            assert!(
                cooked
                    .recipe
                    .build
                    .dependencies
                    .iter()
                    .chain(&cooked.recipe.build.dev_dependencies)
                    .all(RecipeName::is_host),
                "{name}: host modules must not link target libraries"
            );
        }
        let target = super::CookRecipe::new(
            RecipeName::new("python3-cython").unwrap(),
            std::path::PathBuf::new(),
            super::Recipe {
                build: super::BuildRecipe::new(super::BuildKind::Python {
                    pyprojectpath: None,
                    pipflags: Vec::new(),
                    legacysetup: false,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            target.target,
            redoxer::target(),
            "installed modules still target Redox"
        );
    }

    #[test]
    fn a_guest_built_tool_keeps_its_target_dependencies() {
        let map = |name: &str| {
            let dependency = RecipeName::new(name).unwrap();
            super::map_tool_dependency(dependency, "python312", true)
                .map(|mapped| mapped.as_str().to_string())
        };

        // Unprefixed stays unprefixed: a target package.
        assert_eq!(map("openssl3").as_deref(), Some("openssl3"));
        assert_eq!(
            map("python312.setuptools").as_deref(),
            Some("python312.setuptools")
        );
        // A `target:` pin drops the pin but keeps the package.
        assert_eq!(map("target:zlib").as_deref(), Some("zlib"));
        assert_eq!(map("target:zlib.dev").as_deref(), Some("zlib.dev"));
        // The tool's own `host:` reference is dropped.
        assert_eq!(map("host:python312"), None);
        assert_eq!(map("host:python312.dev"), None);
        // A `host:` tool of another recipe is kept and built with this one.
        assert_eq!(
            map("host:python3-setuptools").as_deref(),
            Some("host:python3-setuptools")
        );
    }

    /// A host tool compiles and links on the host, so a `target:` dependency is
    /// dropped. Turning it into an unprefixed one would make the walk pick up a
    /// Redox-built package for a host build that cannot use it, which would
    /// change the dependency graph of every host-mode build.
    #[test]
    fn a_host_built_tool_drops_target_dependencies() {
        let map = |name: &str| {
            let dependency = RecipeName::new(name).unwrap();
            super::map_tool_dependency(dependency, "python312", false)
                .map(|mapped| mapped.as_str().to_string())
        };

        assert_eq!(map("target:zlib"), None);
        assert_eq!(map("target:zlib.dev"), None);
        // An unprefixed dependency moves to the host build. Leaving it a target
        // package is what let a host tool link the target libraries: `libgmp`
        // and `libmpfr` are what a host `binutils-gdb` declares.
        assert_eq!(map("openssl3").as_deref(), Some("host:openssl3"));
        assert_eq!(map("libgmp").as_deref(), Some("host:libgmp"));
        assert_eq!(map("libmpfr").as_deref(), Some("host:libmpfr"));
        assert_eq!(map("host:python312"), None);
        assert_eq!(
            map("host:python3-setuptools").as_deref(),
            Some("host:python3-setuptools")
        );
    }

    /// A recipe that builds with another tool names it `host:`, and that edge
    /// is kept as written in either mode: it is a build tool, built for the
    /// target alongside this recipe in guest mode, and `cook::cook_build`
    /// installs it where the guest shims find it.
    #[test]
    fn a_tools_host_tool_stays_a_build_tool() {
        for tools_are_target in [false, true] {
            let tool = super::map_tool_dependency(
                RecipeName::new("host:gperf").unwrap(),
                "fontconfig",
                tools_are_target,
            );
            assert_eq!(
                tool.map(|name| name.as_str().to_string()).as_deref(),
                Some("host:gperf")
            );
        }
    }

    #[test]
    fn git_cargo_recipe() {
        use crate::recipe::{BuildKind, BuildRecipe, Recipe, SourceRecipe};

        let recipe: Recipe = toml::from_str(
            r#"
            [source]
            git = "https://gitlab.redox-os.org/redox-os/acid.git"
            branch = "master"
            rev = "06344744d3d55a5ac9a62a6059cb363d40699bbc"

            [build]
            template = "cargo"
        "#,
        )
        .unwrap();

        assert_eq!(
            recipe,
            Recipe {
                source: Some(SourceRecipe::Git {
                    git: "https://gitlab.redox-os.org/redox-os/acid.git".to_string(),
                    upstream: None,
                    branch: Some("master".to_string()),
                    rev: Some("06344744d3d55a5ac9a62a6059cb363d40699bbc".to_string()),
                    patches: Vec::new(),
                    script: None,
                    shallow_clone: None,
                }),
                build: BuildRecipe::new(BuildKind::Cargo {
                    cargopath: None,
                    cargoflags: Vec::new(),
                    cargopackages: Vec::new(),
                    cargoexamples: Vec::new(),
                    clearlocked: false,
                    cargopackagesprefixed: false,
                }),
                ..Default::default()
            }
        );
    }

    #[test]
    fn tar_custom_recipe() {
        use crate::recipe::{BuildKind, BuildRecipe, Recipe, SourceRecipe};

        let recipe: Recipe = toml::from_str(
            r#"
            [source]
            tar = "http://downloads.xiph.org/releases/ogg/libogg-1.3.3.tar.xz"
            blake3 = "8220c0e4082fa26c07b10bfe31f641d2e33ebe1d1bb0b20221b7016bc8b78a3a"

            [build]
            template = "custom"
            script = "make"
        "#,
        )
        .unwrap();

        assert_eq!(
            recipe,
            Recipe {
                source: Some(SourceRecipe::Tar {
                    tar: "http://downloads.xiph.org/releases/ogg/libogg-1.3.3.tar.xz".to_string(),
                    blake3: Some(
                        "8220c0e4082fa26c07b10bfe31f641d2e33ebe1d1bb0b20221b7016bc8b78a3a"
                            .to_string()
                    ),
                    patches: Vec::new(),
                    script: None,
                }),
                build: BuildRecipe::new(BuildKind::Custom {
                    script: "make".to_string()
                }),
                ..Default::default()
            }
        );
    }

    #[test]
    fn meta_recipe() {
        use crate::recipe::{BuildKind, BuildRecipe, PackageRecipe, Recipe};

        let recipe: Recipe = toml::from_str(
            r#"
            [package]
            dependencies = [
                "gcc13",
            ]
        "#,
        )
        .unwrap();

        assert_eq!(
            recipe,
            Recipe {
                source: None,
                build: BuildRecipe::new(BuildKind::None),
                package: PackageRecipe {
                    dependencies: vec![RecipeName::new("gcc13").unwrap()],
                    ..Default::default()
                },
                ..Default::default()
            }
        );
    }

    fn write_recipe(root: &std::path::Path, name: &str, toml: &str) -> crate::recipe::CookRecipe {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("recipe.toml"), toml).unwrap();
        crate::recipe::CookRecipe::from_path(&dir, true, false).unwrap()
    }

    #[test]
    fn binary_rule_builds_in_tree_recipes_from_source() {
        use crate::recipe::{BuildKind, SourceRecipe};

        let root = tempfile::tempdir().unwrap();
        let build = "[build]\ntemplate = \"custom\"\nscript = \"make\"\n";
        let mut kernel = write_recipe(
            root.path(),
            "kernel",
            &format!("[source]\npath = \"crates/kernel\"\n{build}"),
        );
        let mut profiling = write_recipe(
            root.path(),
            "profiling-kernel",
            &format!("[source]\nsame_as = \"../kernel\"\n{build}"),
        );
        let mut curl = write_recipe(
            root.path(),
            "curl",
            &format!("[source]\ntar = \"https://example.org/curl.tar.xz\"\n{build}"),
        );

        assert!(kernel.is_in_tree());
        assert!(profiling.is_in_tree());
        assert!(!curl.is_in_tree());

        for recipe in [&mut kernel, &mut profiling] {
            recipe.apply_filesystem_config("binary").unwrap();
            assert_eq!(recipe.rule, "source");
            assert!(matches!(recipe.recipe.build.kind, BuildKind::Custom { .. }));
            assert!(recipe.recipe.source.is_some());
        }
        assert!(matches!(
            kernel.recipe.source,
            Some(SourceRecipe::Path { .. })
        ));

        curl.apply_filesystem_config("binary").unwrap();
        assert_eq!(curl.rule, "binary");
        assert_eq!(curl.recipe.build.kind, BuildKind::Remote);
        assert!(curl.recipe.source.is_none());
    }

    #[test]
    fn downloaded_binaries_need_every_archive_and_its_metadata() {
        let root = tempfile::tempdir().unwrap();
        let recipe = write_recipe(
            root.path(),
            "curl",
            "[source]\ntar = \"https://example.org/curl.tar.xz\"\n\n[build]\ntemplate = \"custom\"\nscript = \"make\"\n",
        );
        let target_dir = recipe.dir.join("target").join(recipe.target);
        std::fs::create_dir_all(&target_dir).unwrap();
        assert!(!recipe.has_downloaded_binaries());

        std::fs::write(target_dir.join("source.pkgar"), b"").unwrap();
        assert!(!recipe.has_downloaded_binaries());

        std::fs::write(target_dir.join("source.toml"), b"").unwrap();
        assert!(recipe.has_downloaded_binaries());
    }

    #[test]
    fn ignore_required_by_build_dep_is_rejected() {
        use crate::recipe::{BuildKind, BuildRecipe, CookRecipe, validate_filesystem_rules};

        let mut ignored = CookRecipe::dummy(&RecipeName::new("libogg").unwrap());
        ignored.rule = "ignore".into();

        let mut dependent = CookRecipe::dummy(&RecipeName::new("libvorbis").unwrap());
        dependent.recipe.build = BuildRecipe::new(BuildKind::Custom {
            script: "make".to_string(),
        });
        dependent.recipe.build.dependencies = vec![RecipeName::new("libogg").unwrap()];

        let err = validate_filesystem_rules(&[ignored, dependent]).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("libogg") && msg.contains("libvorbis"),
            "unexpected error: {msg}"
        );
    }
    #[test]
    fn ignore_multiple_required_by_build_dep_are_rejected() {
        use crate::recipe::{BuildKind, BuildRecipe, CookRecipe, validate_filesystem_rules};

        let mut ignored_ogg = CookRecipe::dummy(&RecipeName::new("libogg").unwrap());
        ignored_ogg.rule = "ignore".into();

        let mut ignored_foo = CookRecipe::dummy(&RecipeName::new("libfoo").unwrap());
        ignored_foo.rule = "ignore".into();

        let mut dependent = CookRecipe::dummy(&RecipeName::new("libvorbis").unwrap());

        dependent.recipe.build = BuildRecipe::new(BuildKind::Custom {
            script: "make".to_string(),
        });

        dependent.recipe.build.dependencies = vec![
            RecipeName::new("libogg").unwrap(),
            RecipeName::new("libfoo").unwrap(),
        ];

        let err = validate_filesystem_rules(&[ignored_ogg, ignored_foo, dependent]).unwrap_err();

        let msg = format!("{err}");

        assert!(msg.contains("libogg"), "unexpected error: {msg}");
        assert!(msg.contains("libfoo"), "unexpected error: {msg}");
        assert!(msg.contains("libvorbis"), "unexpected error: {msg}");
    }
    #[test]
    fn ignore_required_by_dev_dep_is_rejected() {
        use crate::recipe::{BuildKind, BuildRecipe, CookRecipe, validate_filesystem_rules};

        let mut ignored = CookRecipe::dummy(&RecipeName::new("libogg").unwrap());
        ignored.rule = "ignore".into();

        let mut dependent = CookRecipe::dummy(&RecipeName::new("libvorbis").unwrap());
        dependent.recipe.build = BuildRecipe::new(BuildKind::Custom {
            script: "make".to_string(),
        });
        dependent.recipe.build.dev_dependencies = vec![RecipeName::new("libogg").unwrap()];

        let err = validate_filesystem_rules(&[ignored, dependent]).unwrap_err();
        assert!(format!("{}", err).contains("libogg"));
    }

    #[test]
    fn ignore_leaf_is_allowed() {
        use crate::recipe::{BuildKind, BuildRecipe, CookRecipe, validate_filesystem_rules};

        // e.g. pruning `orbterm` from a metapackage: nothing references it.
        let mut ignored = CookRecipe::dummy(&RecipeName::new("orbterm").unwrap());
        ignored.rule = "ignore".into();

        let mut meta = CookRecipe::dummy(&RecipeName::new("desktop-minimal").unwrap());
        meta.recipe.build = BuildRecipe::new(BuildKind::None);
        meta.recipe.package.dependencies = vec![RecipeName::new("orbital").unwrap()];

        assert!(validate_filesystem_rules(&[ignored, meta]).is_ok());
        assert!(validate_filesystem_rules(&[]).is_ok());
    }
}
