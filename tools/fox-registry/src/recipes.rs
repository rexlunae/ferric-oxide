//! `fox-registry import-recipes`: cookbook recipes as `kind = "recipe"`
//! package entries.
//!
//! Each recipe becomes a registry package whose source is its recipe
//! directory in the repository given with `--git` (the monorepo): `git` is
//! that repository and `path` the recipe's directory relative to it, as
//! `packaging-design.md` section 2 describes. The summary names the
//! recipe's upstream source (its git URL or tarball), or its `[package]
//! description` when it has one. The package file is delegated to the
//! `--owner` keys like any admitted package; releases are recorded later
//! from signed tags of the repository.
//!
//! Only the fields this needs are read from `recipe.toml` (`[source]` and
//! `[package] description`); everything else in the cookbook's format is
//! ignored. Reading them here, rather than through the cookbook crate,
//! keeps the cookbook's dependency tree (including its git dependencies)
//! out of a tool that registry CI installs.

use std::fs;
use std::path::{Path, PathBuf};

use fox_core::{GitUrl, PackageName};
use fox_index::layout::{Owner, PackageEntry, PackageKind};
use serde::Deserialize;

/// Why recipes could not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RecipeError {
    #[error("cannot read {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid recipe {}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("{} is not inside {}", dir.display(), repo.display())]
    Outside { dir: PathBuf, repo: PathBuf },
}

#[derive(Debug, Default, Deserialize)]
struct RecipeFile {
    #[serde(default)]
    source: Option<RecipeSource>,
    #[serde(default)]
    package: Option<RecipePackage>,
}

#[derive(Debug, Default, Deserialize)]
struct RecipeSource {
    git: Option<String>,
    tar: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RecipePackage {
    description: Option<String>,
}

/// A recipe found under the scanned directory.
#[derive(Debug)]
pub(crate) struct FoundRecipe {
    /// The directory name, which names the package.
    pub(crate) name: String,
    /// The recipe directory relative to the repository root.
    pub(crate) path: String,
    summary: String,
}

/// The recipes under a directory.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Every readable recipe, sorted by path.
    pub(crate) found: Vec<FoundRecipe>,
    /// Recipes that are not valid TOML (work-in-progress recipes can be),
    /// which the cookbook would refuse too.
    pub(crate) invalid: Vec<RecipeError>,
}

/// Every `recipe.toml` under `dir`.
///
/// # Errors
///
/// [`RecipeError::Io`] if a directory or recipe cannot be read, and
/// [`RecipeError::Outside`] if `dir` is not inside `repo`.
pub(crate) fn scan(repo: &Path, dir: &Path) -> Result<Scan, RecipeError> {
    let mut scan = Scan::default();
    walk(repo, dir, &mut scan)?;
    scan.found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(scan)
}

fn walk(repo: &Path, dir: &Path, scan: &mut Scan) -> Result<(), RecipeError> {
    let io_error = |source| RecipeError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let file = dir.join("recipe.toml");
    if file.is_file() {
        match read(repo, dir, &file) {
            Ok(recipe) => scan.found.push(recipe),
            Err(error @ RecipeError::Parse { .. }) => scan.invalid.push(error),
            Err(error) => return Err(error),
        }
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(io_error)? {
        let path = entry.map_err(io_error)?.path();
        if path.is_dir() && !path.is_symlink() {
            walk(repo, &path, scan)?;
        }
    }
    Ok(())
}

fn read(repo: &Path, dir: &Path, file: &Path) -> Result<FoundRecipe, RecipeError> {
    let text = fs::read_to_string(file).map_err(|source| RecipeError::Io {
        path: file.to_path_buf(),
        source,
    })?;
    let recipe: RecipeFile = toml::from_str(&text).map_err(|source| RecipeError::Parse {
        path: file.to_path_buf(),
        source: Box::new(source),
    })?;
    let relative = dir.strip_prefix(repo).map_err(|_| RecipeError::Outside {
        dir: dir.to_path_buf(),
        repo: repo.to_path_buf(),
    })?;
    let source = recipe.source.unwrap_or_default();
    let upstream = source.git.or(source.tar);
    let summary = recipe
        .package
        .and_then(|package| package.description)
        .unwrap_or_else(|| match &upstream {
            Some(url) => format!("Cookbook recipe for {url}"),
            None => "Cookbook recipe".to_owned(),
        });
    Ok(FoundRecipe {
        name: dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: relative.to_string_lossy().replace('\\', "/"),
        summary,
    })
}

impl FoundRecipe {
    /// The package entry, or `None` if the directory name is not a valid
    /// package name.
    pub(crate) fn entry(&self, git: &GitUrl, owners: &[Owner]) -> Option<PackageEntry> {
        let name = PackageName::new(self.name.as_str()).ok()?;
        Some(PackageEntry {
            name,
            kind: PackageKind::Recipe,
            summary: Some(self.summary.clone()),
            git: git.clone(),
            mirrors: Vec::new(),
            path: Some(self.path.clone()),
            owners: owners.to_vec(),
            releases: Vec::new(),
        })
    }
}
