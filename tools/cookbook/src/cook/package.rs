use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use fox_core::cookbook::{PackageInfo, Prefix, RecipeName};
use pkgar::{PackageFile, Transaction, ext::PackageSrcExt};
use pkgar_core::HeaderFlags;
use pkgar_keys::PublicKeyFile;

use crate::{
    Error, Result,
    config::CookConfig,
    cook::{cook_build::BuildResult, fetch, fox_manifest, fs::*, pty::PtyOut},
    log_to_pty,
    recipe::{BuildKind, CookRecipe, OptionalPackageRecipe},
    wrap_io_err,
};

pub fn package(
    recipe: &CookRecipe,
    build_result: &BuildResult,
    cook_config: &CookConfig,
    logger: &PtyOut,
) -> Result<()> {
    let name = &recipe.name;
    let target_dir = &recipe.target_dir();
    let auto_deps = &build_result.auto_deps;
    if recipe.recipe.build.kind == BuildKind::None {
        // metapackages don't have stage dir and optional packages
        package_toml(
            target_dir.join("stage.toml"),
            recipe,
            None,
            None,
            recipe.recipe.package.dependencies.clone(),
            auto_deps,
        )?;
        return Ok(());
    }

    let secret_path = "build/id_ed25519.toml";
    let public_path = fox_core::cookbook::PUBLIC_KEY;
    if !Path::new(secret_path).is_file() || !Path::new(public_path).is_file() {
        if !Path::new("build").is_dir() {
            create_dir(Path::new("build"))?;
        }
        let (public_key, secret_key) = pkgar_keys::SecretKeyFile::new();
        public_key.save(public_path)?;
        secret_key.save(secret_path)?;
    }

    let packages = recipe.recipe.get_packages_list();
    // Ferric Oxide: signed fox manifests for target packages (fox_manifest.rs).
    let fox_manifests = fox_manifest::enabled() && !name.is_host();
    // The commit is optional in a manifest: prebuilt (remote) packages and
    // path sources may have no source info.
    let source_ident = if fox_manifests {
        fetch::fetch_get_source_info(recipe).ok()
    } else {
        None
    };

    for package in packages {
        let (stage_dir, package_file, package_meta) = package_stage_paths(package, target_dir);
        // Rebuild package if stage is newer
        if package_file.is_file() && !build_result.cached {
            log_to_pty!(logger, "DEBUG: updating '{}'", package_file.display());
            remove_all(&package_file)?;
            if package_meta.is_file() {
                remove_all(&package_meta)?;
            }
        }

        let deps = if package.is_some() {
            BTreeSet::from([name.with_prefix(Prefix::Any)])
        } else {
            auto_deps.clone()
        };
        let package_name = match package {
            Some(p) => RecipeName::new(format!("{}.{}", name.recipe(), p.name))?,
            None => name.clone(),
        };
        let package_deps: Vec<RecipeName> = match package {
            Some(p) => p
                .dependencies
                .iter()
                .map(|dep| {
                    if dep.recipe().is_empty() {
                        package_name.with_suffix(dep.suffix())
                    } else {
                        dep.clone()
                    }
                })
                .collect(),
            None => recipe.recipe.package.dependencies.clone(),
        };

        if !package_file.is_file() {
            if !name.is_host() {
                let version = recipe.guess_version();
                let merged = merge_deps(package_deps.clone(), &deps);
                let full_name = get_package_name(recipe.name.without_prefix(), package);
                let git = match &recipe.recipe.source {
                    Some(crate::recipe::SourceRecipe::Git { git, .. }) => Some(git.as_str()),
                    _ => None,
                };
                let info = fox_manifest::PackageInfo {
                    name: &full_name,
                    version: version.as_deref(),
                    target: recipe.target,
                    depends: merged.iter().map(|d| d.without_prefix()).collect(),
                    source: git.map(|git| {
                        let commit = source_ident.as_ref().map(|i| i.source_identifier.as_str());
                        (git, commit)
                    }),
                };
                if let Some(path) = fox_manifest::update_stage(&stage_dir, &info, fox_manifests)? {
                    log_to_pty!(logger, "DEBUG: wrote fox manifest '{}'", path.display());
                }
            }
            pkgar::create_with_flags(
                secret_path,
                package_file.to_str().unwrap(),
                stage_dir.to_str().unwrap(),
                HeaderFlags::latest(
                    pkgar_core::Architecture::Independent,
                    match cook_config.compressed {
                        true => pkgar_core::Packaging::LZMA2,
                        false => pkgar_core::Packaging::Uncompressed,
                    },
                ),
            )?;
        }

        if !package_meta.is_file() {
            package_toml(
                package_meta,
                recipe,
                Some((Path::new(public_path), &package_file)),
                package,
                package_deps,
                &deps,
            )?;
        }
    }

    Ok(())
}

/// The recipe's declared dependencies followed by the automatic ones it
/// does not already list.
fn merge_deps(
    mut package_deps: Vec<RecipeName>,
    auto_deps: &BTreeSet<RecipeName>,
) -> Vec<RecipeName> {
    for dep in auto_deps {
        if !package_deps.contains(dep) {
            package_deps.push(dep.clone());
        }
    }
    package_deps
}

pub fn package_toml(
    toml_path: PathBuf,
    recipe: &CookRecipe,
    package_file: Option<(&Path, &PathBuf)>,
    package_suffix: Option<&OptionalPackageRecipe>,
    package_deps: Vec<RecipeName>,
    auto_deps: &BTreeSet<RecipeName>,
) -> Result<()> {
    let package_deps = merge_deps(package_deps, auto_deps);

    let (hash, network_size, storage_size) = if let Some((pkey_path, archive_path)) = package_file {
        use pkgar_core::PackageSrc;
        let pkey = pkgar_keys::PublicKeyFile::open(pkey_path)?.pkey;
        let mut package = pkgar::PackageFile::new(archive_path, &pkey)?;
        let mt = std::fs::metadata(archive_path)
            .map_err(wrap_io_err!(archive_path, "Reading metadata"))?;
        let package_size = mt.len();
        let header = package.header();
        let storage_size = match header.flags.packaging() {
            pkgar_core::Packaging::LZMA2 => {
                let mut size = header
                    .total_size()
                    .map_err(|e| Error::Pkgar(pkgar::Error::Core(e)))?
                    as u64;
                let entries = package.read_entries()?;
                for entry in entries {
                    let data_reader = package.data_reader(&entry)?;
                    size += data_reader.unpacked_size;
                    package.restore_reader(data_reader.into_inner())?;
                }
                size
            }
            _ => package_size,
        };

        (
            blake3::Hash::from_bytes(package.header().blake3)
                .to_hex()
                .to_string(),
            package_size,
            storage_size,
        )
    } else {
        ("".into(), 0, 0)
    };

    let ident_source = fetch::fetch_get_source_info(recipe)?;

    let package = PackageInfo {
        name: RecipeName::new(get_package_name(
            recipe.name.without_prefix(),
            package_suffix,
        ))
        .unwrap(),
        version: recipe.guess_version().unwrap_or("TODO".into()),
        target: recipe.target.to_string(),
        blake3: hash,
        network_size,
        storage_size,
        depends: package_deps,
        commit_identifier: ident_source.commit_identifier,
        source_identifier: ident_source.source_identifier,
        time_identifier: ident_source.time_identifier,
        ..Default::default()
    };

    serialize_and_write(&toml_path, &package)?;
    Ok(())
}

pub fn package_target(name: &RecipeName) -> &'static str {
    if name.is_host() {
        redoxer::host_target()
    } else {
        redoxer::target()
    }
}

pub fn package_stage_paths(
    package: Option<&OptionalPackageRecipe>,
    target_dir: &Path,
) -> (PathBuf, PathBuf, PathBuf) {
    let mut target_dir = target_dir.to_path_buf();
    if let Some(cross_target) = crate::cross_target() {
        // TODO: automatically pass COOKBOOK_CROSS_GNU_TARGET?
        target_dir = target_dir.join(cross_target)
    }
    package_name_paths(package, &target_dir, "stage")
}

pub fn package_source_paths(
    package: Option<&OptionalPackageRecipe>,
    target_dir: &Path,
) -> (PathBuf, PathBuf, PathBuf) {
    package_name_paths(package, target_dir, "source")
}

fn package_name_paths(
    package: Option<&OptionalPackageRecipe>,
    target_dir: &Path,
    name: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    let prefix_name = get_package_name(name, package);
    let package_stage = target_dir.join(&prefix_name);
    let package_file = package_stage.with_added_extension("pkgar");
    let package_meta = package_stage.with_added_extension("toml");
    (package_stage, package_file, package_meta)
}

pub fn get_package_name(name: &str, package: Option<&OptionalPackageRecipe>) -> String {
    get_package_name_inner(name, package.map(|p| p.name.as_str()))
}

fn get_package_name_inner(name: &str, package: Option<&str>) -> String {
    let mut prefix_name = name.to_string();
    if let Some(package) = package {
        prefix_name.push('.');
        prefix_name.push_str(package);
    }
    prefix_name
}

/// Installs a built package, signed by the key in `pkey_path`, into a
/// sysroot. With `store`, fox installs it and records it, and the key
/// becomes one the sysroot trusts so fox there can check it later;
/// without, its files are only extracted.
///
/// Returns whether the package was already installed as built.
pub fn package_handle_push(
    store: Option<&fox_store::Store>,
    pkey_path: &Path,
    archive_path: &Path,
    sysroot_dir: &Path,
) -> crate::Result<bool> {
    if !archive_path.is_file() {
        // A meta-package: nothing to place.
        return Ok(true);
    }
    let Some(store) = store else {
        // Bare install without metadata, cache mechanism or anything
        let pkey = PublicKeyFile::open(pkey_path)?.pkey;
        let mut package = PackageFile::new(archive_path, &pkey)?;
        Transaction::install(&mut package, sysroot_dir)?.commit()?;
        return Ok(false);
    };
    let key =
        fox_core::keys::install_key_file(pkey_path, sysroot_dir, fox_core::keys::COOKBOOK_LABEL)?;
    let mut keys = fox_core::TrustedKeys::new();
    keys.insert(key.label, key.key);
    let archive = fox_core::archive::Archive::open(archive_path, &keys)?;
    let installed = store.get(&archive.manifest().name)?;
    if installed.is_some_and(|p| p.archive_blake3 == Some(archive.blake3())) {
        return Ok(true);
    }
    // Without features.toml packages.signed_manifests, cookbook archives
    // carry only the unsigned metadata file beside them.
    let options = fox_store::InstallOptions {
        force: false,
        allow_unsigned_metadata: true,
    };
    store.install(&archive, options)?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
    use fox_core::{Manifest, PackageName};

    #[test]
    fn push_installs_with_fox_once() {
        let dir = tempfile::tempdir().unwrap();
        let key = FixtureKey::generate(dir.path(), "id_ed25519").unwrap();
        let manifest = Manifest::new(PackageName::new("app").unwrap(), "1.0");
        let file = [FixtureEntry::file("usr/bin/app", b"app", 0o755)];
        let archive = build_package(dir.path(), &key, Some(&manifest), &file, false).unwrap();
        let sysroot = dir.path().join("sysroot");
        std::fs::create_dir(&sysroot).unwrap();
        let store = fox_store::Store::open(&sysroot).unwrap();

        let push = || package_handle_push(Some(&store), &key.public_path, &archive, &sysroot);
        assert!(!push().unwrap());
        assert!(sysroot.join("usr/bin/app").is_file());
        assert!(sysroot.join("etc/fox/keys/cookbook.toml").is_file());
        let installed = store.get(&manifest.name).unwrap().unwrap();
        assert_eq!(installed.signer_label, fox_core::keys::COOKBOOK_LABEL);
        assert!(push().unwrap(), "the same archive again is cached");
    }

    #[test]
    fn push_without_store_only_extracts() {
        let dir = tempfile::tempdir().unwrap();
        let key = FixtureKey::generate(dir.path(), "id_ed25519").unwrap();
        let file = [FixtureEntry::file("usr/bin/app", b"app", 0o755)];
        let archive = build_package(dir.path(), &key, None, &file, false).unwrap();
        let sysroot = dir.path().join("sysroot");

        assert!(!package_handle_push(None, &key.public_path, &archive, &sysroot).unwrap());
        assert!(sysroot.join("usr/bin/app").is_file());
        assert!(!sysroot.join("var/lib/fox").exists());
    }

    #[test]
    fn push_of_a_meta_package_places_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("group.pkgar");
        let key = dir.path().join("none.pub.toml");
        assert!(package_handle_push(None, &key, &missing, dir.path()).unwrap());
    }
}
