//! Ferric Oxide: signed `fox` manifests inside package archives.
//!
//! With `FERRIC_FOX_MANIFEST=1` in the environment (the feature catalog's
//! `packages.signed_manifests`, on by default when `ferric.toml` is used),
//! each target package gets `usr/share/fox/manifest/<name>.toml` in its
//! stage before it is packed. The file is an ordinary archive entry, so the
//! pkgar signature covers the package's name, version, target, dependencies
//! and source. `fox` then reads its identity from the archive itself rather
//! than from the unsigned `<name>.toml` written next to it. The format is
//! `fox_core::Manifest`.
//!
//! Without the variable nothing is added, as upstream, and a manifest left
//! in a stage by an earlier build is removed so it cannot go stale.

use std::path::{Path, PathBuf};

use fox_core::{Manifest, SourceInfo, manifest_path};

use crate::{Error, Result, wrap_io_err};

/// The environment variable that turns manifests on, with the value `1`.
pub const ENV_VAR: &str = "FERRIC_FOX_MANIFEST";

/// Whether manifests are written in this build.
pub fn enabled() -> bool {
    std::env::var_os(ENV_VAR).is_some_and(|value| value == "1")
}

/// What a manifest says about one package.
#[derive(Debug, Clone, Default)]
pub struct PackageInfo<'a> {
    /// The package name, `recipe` or `recipe.optional`.
    pub name: &'a str,
    /// The recipe's version, if the cookbook could tell it.
    pub version: Option<&'a str>,
    /// The target triple.
    pub target: &'a str,
    /// Runtime dependencies.
    pub depends: Vec<&'a str>,
    /// The git source and the commit it was built from, when there is one.
    pub source: Option<(&'a str, Option<&'a str>)>,
}

/// Builds the manifest for `info`.
///
/// Versions the cookbook could not tell (missing, empty or its `TODO`
/// placeholder) become `unknown`, since a manifest needs one.
///
/// # Errors
///
/// [`Error::FoxName`] if the package or a dependency name is not a valid
/// `fox` package name.
pub fn manifest(info: &PackageInfo<'_>) -> Result<Manifest> {
    let name = fox_name(info.name)?;
    let version = info
        .version
        .filter(|version| !version.is_empty() && *version != "TODO")
        .unwrap_or("unknown");
    let mut manifest = Manifest::new(name, version);
    manifest.target = Some(info.target.to_owned());
    manifest.depends = info
        .depends
        .iter()
        .copied()
        .map(fox_name)
        .collect::<Result<_>>()?;
    manifest.depends.sort();
    manifest.depends.dedup();
    manifest.source = info.source.map(|(git, commit)| SourceInfo {
        git: git.to_owned(),
        commit: commit.filter(|c| !c.is_empty()).map(str::to_owned),
    });
    Ok(manifest)
}

/// Writes the manifest into `stage_dir` when `enabled` (normally
/// [`enabled()`]), or removes a stale one when not, and returns the path
/// when one was written.
///
/// # Errors
///
/// [`Error::FoxName`] or [`Error::FoxManifest`] if the manifest cannot be
/// built, [`Error::Io`] if the file cannot be written or removed.
pub fn update_stage(
    stage_dir: &Path,
    info: &PackageInfo<'_>,
    enabled: bool,
) -> Result<Option<PathBuf>> {
    let name = fox_name(info.name)?;
    let path = stage_dir.join(manifest_path(&name));
    if !enabled {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(wrap_io_err!(path, "Removing fox manifest")(error)),
        }
        return Ok(None);
    }
    let text = manifest(info)?.to_toml().map_err(Error::FoxManifest)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(wrap_io_err!(dir, "Creating fox manifest dir"))?;
    }
    std::fs::write(&path, text).map_err(wrap_io_err!(path, "Writing fox manifest"))?;
    Ok(Some(path))
}

fn fox_name(name: &str) -> Result<fox_core::PackageName> {
    fox_core::PackageName::new(name).map_err(|source| Error::FoxName {
        name: name.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> PackageInfo<'static> {
        PackageInfo {
            name: "llvm21.dev",
            version: Some("21.1.0"),
            target: "x86_64-unknown-redox",
            depends: vec!["llvm21.runtime", "relibc", "relibc"],
            source: Some(("https://example.com/llvm.git", Some("abc123"))),
        }
    }

    #[test]
    fn builds_the_manifest() {
        let built = manifest(&info()).unwrap();
        assert_eq!(built.name.as_str(), "llvm21.dev");
        assert_eq!(built.version, "21.1.0");
        assert_eq!(built.target.as_deref(), Some("x86_64-unknown-redox"));
        let deps: Vec<_> = built.depends.iter().map(|d| d.as_str()).collect();
        assert_eq!(deps, ["llvm21.runtime", "relibc"]);
        assert_eq!(built.source.unwrap().commit.as_deref(), Some("abc123"));
        // It round-trips through the format fox reads.
        let text = manifest(&info()).unwrap().to_toml().unwrap();
        assert_eq!(Manifest::from_toml(&text).unwrap().version, "21.1.0");
    }

    #[test]
    fn placeholder_versions_become_unknown() {
        for version in [None, Some(""), Some("TODO")] {
            let info = PackageInfo { version, ..info() };
            assert_eq!(manifest(&info).unwrap().version, "unknown");
        }
    }

    #[test]
    fn rejects_names_fox_cannot_use() {
        let info = PackageInfo {
            depends: vec!["host:gcc13"],
            ..info()
        };
        assert!(matches!(manifest(&info), Err(Error::FoxName { .. })));
    }

    #[test]
    fn stage_follows_the_switch() {
        let stage = tempfile::tempdir().unwrap();
        let expected = stage.path().join("usr/share/fox/manifest/llvm21.dev.toml");
        let written = update_stage(stage.path(), &info(), true).unwrap();
        assert_eq!(written.as_deref(), Some(expected.as_path()));
        let text = std::fs::read_to_string(&expected).unwrap();
        assert!(text.contains("name = \"llvm21.dev\""), "{text}");
        assert_eq!(update_stage(stage.path(), &info(), false).unwrap(), None);
        assert!(!expected.exists());
    }
}
