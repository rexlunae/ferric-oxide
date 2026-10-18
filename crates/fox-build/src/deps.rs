//! The crate graph of a build, for its SBOM.
//!
//! `cargo metadata`, run for the build's target and features, gives the
//! resolved graph; `Cargo.lock` adds what metadata leaves out, the registry
//! checksum of each crate. The graph is walked from the package along normal
//! and build dependencies (dev-dependencies never reach the output). A crate
//! is *build-only* when every path to it goes through a build dependency or
//! a procedural macro, which run on the build host and are not linked in.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use cargo_metadata::{DependencyKind, Metadata, Package, PackageId, TargetKind};
use fox_sbom::{CrateDependency, CrateSource};
use serde::Deserialize;

use crate::BuildError;

/// The parts of `Cargo.lock` the SBOM needs.
#[derive(Debug, Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

#[derive(Debug, Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

/// The package's direct dependencies and its whole crate graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrateGraph {
    pub roots: Vec<String>,
    pub crates: Vec<CrateDependency>,
}

/// Whether a crate is reached as part of the output or only at build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Context {
    Runtime,
    Build,
}

/// Builds the graph of `root` from resolved `metadata` and `lock` (the
/// contents of `Cargo.lock`).
///
/// # Errors
///
/// [`BuildError::Lockfile`] if `Cargo.lock` does not parse, and
/// [`BuildError::NoResolve`] if the metadata has no resolved graph.
pub(crate) fn crate_graph(
    metadata: &Metadata,
    root: &PackageId,
    lock: &str,
    src: &Path,
) -> Result<CrateGraph, BuildError> {
    let lock: Lockfile = toml::from_str(lock).map_err(BuildError::Lockfile)?;
    let checksums: BTreeMap<(&str, &str, &str), &str> = lock
        .package
        .iter()
        .filter_map(|p| {
            let source = p.source.as_deref()?;
            let checksum = p.checksum.as_deref()?;
            Some(((p.name.as_str(), p.version.as_str(), source), checksum))
        })
        .collect();
    let resolve = metadata.resolve.as_ref().ok_or(BuildError::NoResolve)?;
    let nodes: BTreeMap<&PackageId, _> = resolve.nodes.iter().map(|n| (&n.id, n)).collect();
    let packages: BTreeMap<&PackageId, &Package> =
        metadata.packages.iter().map(|p| (&p.id, p)).collect();

    // Breadth-first from the root; a crate seen first at build time is
    // visited again if it is later reached at run time.
    let mut context: BTreeMap<&PackageId, Context> = BTreeMap::new();
    let mut edges: BTreeMap<&PackageId, BTreeSet<&PackageId>> = BTreeMap::new();
    let mut queue = VecDeque::from([(root, Context::Runtime)]);
    while let Some((id, ctx)) = queue.pop_front() {
        let Some(node) = nodes.get(id) else { continue };
        for dep in &node.deps {
            let kinds = dep.dep_kinds.iter().map(|k| k.kind);
            let dep_ctx = kinds
                .filter_map(|kind| match kind {
                    DependencyKind::Normal => Some(ctx),
                    DependencyKind::Build => Some(Context::Build),
                    _ => None,
                })
                .min();
            let Some(mut dep_ctx) = dep_ctx else { continue };
            // A procedural macro runs in the compiler; neither it nor what it
            // pulls in is linked into the output.
            if packages.get(&dep.pkg).is_some_and(|p| is_proc_macro(p)) {
                dep_ctx = Context::Build;
            }
            edges.entry(id).or_default().insert(&dep.pkg);
            let better = context.get(&dep.pkg).is_none_or(|seen| dep_ctx < *seen);
            if better {
                context.insert(&dep.pkg, dep_ctx);
                queue.push_back((&dep.pkg, dep_ctx));
            }
        }
    }

    let id_string = |id: &PackageId| stable_id(id, packages.get(id).copied(), src);
    let roots = edges
        .get(root)
        .map(|deps| deps.iter().map(|id| id_string(id)).collect())
        .unwrap_or_default();
    let crates = context
        .iter()
        .filter(|(id, _)| **id != root)
        .filter_map(|(id, ctx)| {
            let package = packages.get(id)?;
            Some(CrateDependency {
                id: id_string(id),
                name: package.name.to_string(),
                version: package.version.to_string(),
                source: crate_source(package, &checksums),
                license: package.license.clone(),
                build_only: *ctx == Context::Build,
                depends: edges
                    .get(id)
                    .map(|deps| {
                        deps.iter()
                            .filter(|dep| **dep != root)
                            .map(|dep| id_string(dep))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect();
    Ok(CrateGraph { roots, crates })
}

/// The SBOM id of a crate. Cargo's package id of a path crate holds the
/// absolute path of the build directory, which would make the SBOM differ
/// between builds of the same source; those are named by their directory in
/// the source tree instead.
fn stable_id(id: &PackageId, package: Option<&Package>, src: &Path) -> String {
    match package {
        Some(package) if package.source.is_none() => {
            let dir = package
                .manifest_path
                .as_std_path()
                .parent()
                .and_then(|dir| dir.strip_prefix(src).ok())
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!(
                "path+fox-source:/{dir}#{}@{}",
                package.name, package.version
            )
        }
        _ => id.repr.clone(),
    }
}

fn is_proc_macro(package: &Package) -> bool {
    package
        .targets
        .iter()
        .any(|t| t.kind.contains(&TargetKind::ProcMacro))
}

fn crate_source(package: &Package, checksums: &BTreeMap<(&str, &str, &str), &str>) -> CrateSource {
    let Some(source) = &package.source else {
        return CrateSource::Path;
    };
    let repr = source.repr.as_str();
    if let Some(rest) = repr.strip_prefix("git+") {
        // `git+<url>?<query>#<commit>`; the commit is what pins it.
        let (url, commit) = rest.split_once('#').unwrap_or((rest, ""));
        let url = url.split_once('?').map_or(url, |(url, _)| url);
        return CrateSource::Git {
            url: url.to_owned(),
            commit: commit.to_owned(),
        };
    }
    let url = repr
        .strip_prefix("registry+")
        .or_else(|| repr.strip_prefix("sparse+"))
        .unwrap_or(repr);
    let version = package.version.to_string();
    CrateSource::Registry {
        url: url.to_owned(),
        checksum: checksums
            .get(&(package.name.as_str(), version.as_str(), repr))
            .map(|sum| (*sum).to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed `cargo metadata` document: app -> lib (normal),
    /// app -> derive (proc macro) -> syn, app -> cc (build), lib -> syn, and
    /// a dev-dependency that must not appear.
    fn metadata() -> Metadata {
        let pkg = |id: &str, name: &str, source: Option<&str>, kind: &str| {
            serde_json::json!({
                "name": name, "version": "1.0.0", "id": id, "license": "MIT",
                "license_file": null, "description": null, "source": source,
                "dependencies": [], "targets": [{
                    "kind": [kind], "crate_types": [kind], "name": name,
                    "src_path": "/x/src/lib.rs", "edition": "2021",
                    "doctest": false, "test": false, "doc": false
                }],
                "features": {}, "manifest_path": "/x/Cargo.toml", "metadata": null,
                "publish": null, "authors": [], "categories": [], "keywords": [],
                "readme": null, "repository": null, "homepage": null,
                "documentation": null, "edition": "2021", "links": null,
                "default_run": null, "rust_version": null
            })
        };
        let reg = "registry+https://github.com/rust-lang/crates.io-index";
        let dep = |pkg: &str, kind: Option<&str>| serde_json::json!({ "name": pkg, "pkg": pkg, "dep_kinds": [{ "kind": kind, "target": null }] });
        let node = |id: &str, deps: Vec<serde_json::Value>| serde_json::json!({ "id": id, "deps": deps, "dependencies": [], "features": [] });
        let doc = serde_json::json!({
            "packages": [
                pkg("app", "app", None, "bin"),
                pkg("lib", "lib", Some(reg), "lib"),
                pkg("derive", "derive", Some(reg), "proc-macro"),
                pkg("syn", "syn", Some(reg), "lib"),
                pkg("cc", "cc", Some("git+https://example.com/cc?branch=main#abc123"), "lib"),
                pkg("tester", "tester", Some(reg), "lib"),
            ],
            "workspace_members": ["app"],
            "workspace_default_members": ["app"],
            "resolve": {
                "root": "app",
                "nodes": [
                    node("app", vec![dep("lib", None), dep("derive", None), dep("cc", Some("build")), dep("tester", Some("dev"))]),
                    node("lib", vec![dep("syn", None)]),
                    node("derive", vec![dep("syn", None)]),
                    node("syn", vec![]),
                    node("cc", vec![]),
                    node("tester", vec![]),
                ]
            },
            "target_directory": "/x/target",
            "build_directory": "/x/target",
            "version": 1,
            "workspace_root": "/x",
            "metadata": null
        });
        serde_json::from_value(doc).unwrap()
    }

    const LOCK: &str = r#"
version = 4

[[package]]
name = "lib"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"

[[package]]
name = "app"
version = "1.0.0"
"#;

    #[test]
    fn walks_runtime_and_build_dependencies() {
        let metadata = metadata();
        let root = metadata.packages[0].id.clone();
        let graph = crate_graph(&metadata, &root, LOCK, Path::new("/x")).unwrap();
        assert_eq!(graph.roots, ["cc", "derive", "lib"]);
        let by_name: BTreeMap<_, _> = graph.crates.iter().map(|c| (c.name.as_str(), c)).collect();
        assert_eq!(
            by_name.keys().copied().collect::<Vec<_>>(),
            ["cc", "derive", "lib", "syn"]
        );
        assert!(!by_name["lib"].build_only);
        assert!(by_name["derive"].build_only);
        assert!(by_name["cc"].build_only);
        // syn is reached through the proc macro at build time, but also
        // through lib at run time.
        assert!(!by_name["syn"].build_only);
        assert_eq!(by_name["lib"].depends, ["syn"]);
        assert_eq!(
            by_name["lib"].source,
            CrateSource::Registry {
                url: "https://github.com/rust-lang/crates.io-index".into(),
                checksum: Some("aaaa".into())
            }
        );
        assert_eq!(
            by_name["cc"].source,
            CrateSource::Git {
                url: "https://example.com/cc".into(),
                commit: "abc123".into()
            }
        );
        assert!(matches!(
            by_name["syn"].source,
            CrateSource::Registry { checksum: None, .. }
        ));
    }

    #[test]
    fn rejects_a_bad_lockfile() {
        let metadata = metadata();
        let root = metadata.packages[0].id.clone();
        assert!(matches!(
            crate_graph(&metadata, &root, "package = 3", Path::new("/x")),
            Err(BuildError::Lockfile(_))
        ));
    }
}
