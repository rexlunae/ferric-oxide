use fox_core::cookbook::{PackageInfo, Prefix, RecipeName};
use pkgar::{PackageFile, Transaction};
use pkgar_core::PackageSrc;
use pkgar_keys::PublicKeyFile;

use crate::bail_other_err;
use crate::config::CookConfig;
use crate::cook::fetch_repo;
use crate::cook::package::package_source_paths;
use crate::cook::{fetch, fs, host_tools, pty::PtyOut, script::*};
use crate::recipe::BuildCompiler;
use crate::recipe::{AutoDeps, BuildKind, CookRecipe, OptionalPackageRecipe, Recipe};
use std::io::Read;
use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
    process::Command,
    str,
};

use crate::{Error, Result, is_redox, log_to_pty, wrap_io_err};

fn auto_deps_from_dynamic_linking(
    stage_dirs: &[PathBuf],
    dep_pkgars: &BTreeSet<(RecipeName, PathBuf)>,
    logger: &PtyOut,
) -> BTreeSet<RecipeName> {
    let mut paths = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let verbose = crate::config::try_get_config()
        .map(|s| s.cook.verbose)
        .unwrap_or(true);
    // Base directories may need to be updated for packages that place binaries in odd locations.
    let mut walk = VecDeque::new();

    for stage_dir in stage_dirs {
        walk.push_back((stage_dir, stage_dir.join("usr/bin")));
        walk.push_back((stage_dir, stage_dir.join("usr/games")));
        walk.push_back((stage_dir, stage_dir.join("usr/lib")));
        walk.push_back((stage_dir, stage_dir.join("usr/libexec")));
    }

    // Recursively (DFS) walk each directory to ensure nested libs and bins are checked.
    while let Some((rel_path, dir)) = walk.pop_front() {
        if visited.contains(&dir) {
            #[cfg(debug_assertions)]
            log_to_pty!(
                logger,
                "DEBUG: auto_deps => Skipping `{dir:?}` (already visited)"
            );
            continue;
        }
        assert!(
            visited.insert(dir.clone()),
            "Directory `{:?}` should not be in visited\nVisited: {:#?}",
            dir,
            visited
        );

        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry_res in read_dir {
            let Ok(entry) = entry_res else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file() {
                paths.insert((rel_path, entry.path()));
            } else if file_type.is_dir() {
                walk.push_front((rel_path, entry.path()));
            }
        }
    }

    let mut needed = BTreeSet::new();
    for (rel_path, path) in paths {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let read_cache = object::ReadCache::new(file);
        let Ok(object) = object::build::elf::Builder::read(&read_cache) else {
            continue;
        };
        let Some(dynamic_data) = object.dynamic_data() else {
            continue;
        };
        for dynamic in dynamic_data {
            let object::build::elf::Dynamic::String { tag, val } = dynamic else {
                continue;
            };
            let Ok(name) = str::from_utf8(val) else {
                continue;
            };
            let path_rel = path
                .strip_prefix(rel_path)
                .ok()
                .filter(|_| verbose)
                .map(|s| s.display());
            match *tag {
                object::elf::DT_NEEDED => {
                    if let Some(path_rel) = path_rel {
                        log_to_pty!(logger, "DEBUG: {} needs {}", path_rel, name);
                    }
                    needed.insert(name.to_string());
                }
                object::elf::DT_RPATH | object::elf::DT_RUNPATH => {
                    if let Some(path_rel) = path_rel {
                        log_to_pty!(logger, "DEBUG: {} uses runpath {}", path_rel, name);
                    }
                }
                _ => {} // DT_SONAME
            }
        }
    }

    let mut missing = needed.clone();
    // relibc and friends will always be installed
    for preinstalled in &["libc.so.6", "libgcc_s.so.1", "libstdc++.so.6"] {
        missing.remove(*preinstalled);
    }

    let mut deps = BTreeSet::new();
    if let Ok(key_file) = pkgar_keys::PublicKeyFile::open(fox_core::cookbook::PUBLIC_KEY) {
        for (dep, archive_path) in dep_pkgars.iter() {
            let Ok(mut package) = pkgar::PackageFile::new(archive_path, &key_file.pkey) else {
                continue;
            };
            let Ok(entries) = pkgar_core::PackageSrc::read_entries(&mut package) else {
                continue;
            };
            for entry in entries {
                let Ok(entry_path) = pkgar::ext::EntryExt::check_path(&entry) else {
                    continue;
                };
                for prefix in &["lib", "usr/lib"] {
                    let Ok(child_path) = entry_path.strip_prefix(prefix) else {
                        continue;
                    };
                    let Some(child_name) = child_path.to_str() else {
                        continue;
                    };
                    if needed.contains(child_name) {
                        if verbose {
                            log_to_pty!(logger, "DEBUG: {} provides {}", dep, child_name);
                        }
                        deps.insert(dep.with_prefix(Prefix::Any));
                        missing.remove(child_name);
                    }
                }
            }
        }
    }

    if verbose {
        for name in missing {
            log_to_pty!(logger, "INFO: missing {}", name);
        }
    }

    deps
}

fn auto_deps_from_static_package_deps(
    build_dep_pkgars: &BTreeSet<(RecipeName, PathBuf)>,
    dynamic_dep_pkgars: &BTreeSet<RecipeName>,
) -> std::result::Result<BTreeSet<RecipeName>, crate::PackageError> {
    let static_dep_pkgars: Vec<RecipeName> = build_dep_pkgars
        .iter()
        .map(|x| x.0.clone())
        .filter(|x| !dynamic_dep_pkgars.contains(x))
        .collect();
    let pkgs = CookRecipe::get_package_deps_recursive(&static_dep_pkgars, false)?;

    Ok(pkgs.into_iter().collect())
}

/// Which dependency set a recipe's sysroot is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SysrootDeps {
    /// The host tool packages, for a `host:` recipe built for the host.
    Host,
    /// The target packages, for a target recipe and for a `host:` recipe built
    /// for the target.
    Target,
}

/// The dependency sets a recipe builds against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DependencySets {
    sysroot: SysrootDeps,
    /// Whether the host tool packages are also installed as a toolchain the
    /// build scripts run tools from.
    toolchain: bool,
}

/// Chooses the dependency sets for `name`, ignoring the dependency contents.
///
/// A recipe built for the target compiles against the target packages, laid
/// out as its sysroot. That includes a `host:` recipe when its tools are built
/// for the guest: it is a target binary then, so its own libraries are the
/// target ones, and the host toolchain must stay empty so no host-built
/// library is linked into it.
///
/// Otherwise a `host:` recipe is a host binary. It compiles against its host
/// tool packages: they are host-built too, so they take the sysroot's place,
/// and the target sysroot must stay empty so no target library is linked into
/// it. It also installs them as a toolchain, which its scripts run tools from.
///
/// The packages a dependency walk found are not consulted here, so the choice
/// is the same on a cold build and on the rebuild that clears the cached
/// directories.
fn dependency_sets(name: &RecipeName, tools_are_target: bool) -> DependencySets {
    match (name.is_host(), tools_are_target) {
        (true, true) => DependencySets {
            sysroot: SysrootDeps::Target,
            toolchain: false,
        },
        (true, false) => DependencySets {
            sysroot: SysrootDeps::Host,
            toolchain: true,
        },
        (false, _) => DependencySets {
            sysroot: SysrootDeps::Target,
            toolchain: true,
        },
    }
}

pub struct BuildResult {
    pub stage_dirs: Vec<PathBuf>,
    pub auto_deps: BTreeSet<RecipeName>,
    pub cached: bool,
}

impl BuildResult {
    pub fn new(stage_dirs: Vec<PathBuf>, auto_deps: BTreeSet<RecipeName>) -> Self {
        BuildResult {
            stage_dirs,
            auto_deps,
            cached: false,
        }
    }

    pub fn cached(stage_dirs: Vec<PathBuf>, auto_deps: BTreeSet<RecipeName>) -> Self {
        BuildResult {
            stage_dirs,
            auto_deps,
            cached: true,
        }
    }
}

pub fn build(
    recipe_dir: &Path,
    source_dir: &Path,
    target_dir: &Path,
    cook_recipe: &CookRecipe,
    cook_config: &CookConfig,
    logger: &PtyOut,
) -> Result<BuildResult> {
    let recipe = &cook_recipe.recipe;
    let name = &cook_recipe.name;
    let check_source = !cook_recipe.is_deps;
    let sysroot_dir = get_sub_target_dir(target_dir, "sysroot");
    let toolchain_dir = get_sub_target_dir(target_dir, "toolchain");
    // The tools built for the target, which run in the guest through shims.
    let guest_tools_dir = get_sub_target_dir(target_dir, "guest-tools");
    let auto_deps_file = get_sub_target_dir(target_dir, "auto_deps.toml");
    let stage_dirs = get_stage_dirs(&recipe.optional_packages, target_dir);
    let stage_pkgars: Vec<PathBuf> = stage_dirs
        .iter()
        .map(|p| p.with_added_extension("pkgar"))
        .collect();
    let native_sysroot_time = native_sysroot_modified(recipe_dir);
    let native_sysroot_changed = native_sysroot_is_newer(native_sysroot_time, &stage_pkgars);
    let cli_jobs = cook_config.jobs;
    if recipe.build.kind == BuildKind::None {
        // metapackages don't need to do anything here
        return Ok(BuildResult::new(stage_dirs, BTreeSet::new()));
    }

    let mut dep_pkgars = BTreeSet::new();
    let mut dep_host_pkgars = BTreeSet::new();
    let mut dep_guest_tool_pkgars = BTreeSet::new();
    let build_deps = CookRecipe::get_build_deps_recursive(
        &[
            &recipe.build.dependencies[..],
            &recipe.build.dev_dependencies[..],
        ]
        .concat(),
        false,
    )?;
    for dependency in build_deps.iter() {
        let (_, pkgar, _) = dependency.stage_paths();
        if dependency.runs_in_guest() {
            // A tool built for the target is a target package, so it joins the
            // sysroot like any other dependency, and the guest tools the shims
            // stand in for.
            dep_pkgars.insert((dependency.name.clone(), pkgar.clone()));
            dep_guest_tool_pkgars.insert((dependency.name.clone(), pkgar));
        } else if dependency.name.is_host() {
            // A host-built tool joins the toolchain the recipe runs tools from.
            dep_host_pkgars.insert((dependency.name.clone(), pkgar));
        } else {
            dep_pkgars.insert((dependency.name.clone(), pkgar));
        }
    }

    macro_rules! make_auto_deps {
        ($cached:expr) => {
            build_auto_deps(
                recipe,
                &auto_deps_file,
                &stage_dirs,
                $cached,
                cook_config,
                dep_pkgars,
                logger,
            )
        };
    }

    if !check_source && !native_sysroot_changed {
        // TODO: when stage_dirs does not exist due to clean_target was true, extract from stage.pkgar?
        let stage_present = stage_pkgars.iter().all(|file| file.is_file());
        if stage_present && auto_deps_file.is_file() {
            log_to_pty!(logger, "DEBUG: using cached build, not checking source");
            let auto_deps = make_auto_deps!(true)?;
            return Ok(BuildResult::cached(stage_dirs, auto_deps));
        }
    }

    if recipe.build.kind == BuildKind::Remote {
        return build_remote(stage_dirs, stage_pkgars, recipe, target_dir, logger);
    }

    let sets = dependency_sets(name, cook_recipe.runs_in_guest());
    let deps_sysroot = match sets.sysroot {
        SysrootDeps::Host => &dep_host_pkgars,
        SysrootDeps::Target => &dep_pkgars,
    };
    let have_toolchain = sets.toolchain && !dep_host_pkgars.is_empty();
    let have_guest_tools = sets.toolchain && !dep_guest_tool_pkgars.is_empty();
    let (sysroot_cached, toolchain_cached, guest_tools_cached) = (
        build_deps_dir(logger, &sysroot_dir, deps_sysroot)?,
        if have_toolchain {
            build_deps_dir(logger, &toolchain_dir, &dep_host_pkgars)?
        } else {
            drop_stale_deps_dir(&toolchain_dir)?
        },
        if have_guest_tools {
            build_deps_dir(logger, &guest_tools_dir, &dep_guest_tool_pkgars)?
        } else {
            drop_stale_deps_dir(&guest_tools_dir)?
        },
    );

    // Rebuild stage if deps or source is newer
    if !sysroot_cached || !toolchain_cached || !guest_tools_cached || {
        build_is_source_newer(
            logger,
            recipe_dir,
            source_dir,
            &auto_deps_file,
            stage_pkgars,
            native_sysroot_time,
        )
    } {
        if auto_deps_file.is_file() {
            fs::remove_all(&auto_deps_file)?;
        }
        for stage_dir in &stage_dirs {
            remove_stage_dir(stage_dir)?;
        }
        if cook_config.clean_target {
            // no matter what, these two caches are invalid
            if sysroot_cached {
                fs::remove_all(&sysroot_dir)?;
                build_deps_dir(logger, &sysroot_dir, deps_sysroot)?;
            }
            if toolchain_cached && have_toolchain {
                fs::remove_all(&toolchain_dir)?;
                build_deps_dir(logger, &toolchain_dir, &dep_host_pkgars)?;
            }
            if guest_tools_cached && have_guest_tools {
                fs::remove_all(&guest_tools_dir)?;
                build_deps_dir(logger, &guest_tools_dir, &dep_guest_tool_pkgars)?;
            }
        }
    } else {
        log_to_pty!(logger, "DEBUG: using cached build");
        // stop early otherwise we'll end up rebuilding
        let auto_deps = make_auto_deps!(true)?;
        return Ok(BuildResult::cached(stage_dirs, auto_deps));
    }

    let stage_dir = stage_dirs
        .last()
        .expect("Should have atleast one stage dir");

    let build_dir = get_sub_target_dir(target_dir, "build");
    if !stage_dir.is_dir() {
        // Create stage.tmp
        let stage_dir_tmp = target_dir.join("stage.tmp");
        fs::create_dir_clean(&stage_dir_tmp)?;

        // Create build dir, if it does not exist
        // Cargo does not fingerprint native libraries supplied through a sysroot.
        if cook_config.clean_build || native_sysroot_changed || !build_dir.is_dir() {
            fs::create_dir_clean(&build_dir)?;
        }

        let flags_fn = |name, flags: &Vec<String>| {
            format!(
                "{name}+=(\n{}\n)\n",
                flags
                    .iter()
                    .map(|s| format!("  \"{s}\""))
                    .collect::<Vec<String>>()
                    .join("\n")
            )
        };

        let bool_fn = |name, flag: &bool| {
            if *flag { name } else { "" }
        };

        let mut allow_cargo_offline = false;
        //TODO: better integration with redoxer (library instead of binary)
        //TODO: configurable target
        //TODO: Add more configurability, convert scripts to Rust?
        host_tools::require(cook_recipe)?;
        let script = match &recipe.build.kind {
            BuildKind::Cargo {
                cargopath,
                cargoflags,
                cargopackages,
                cargoexamples,
                clearlocked,
                cargopackagesprefixed,
            } => {
                allow_cargo_offline = true;
                let mut script = format!(
                    "DYNAMIC_INIT\n{}{}\nCOOKBOOK_CARGO_PATH={} ",
                    bool_fn("COOKBOOK_CARGO_FLAGS=()\n", clearlocked),
                    flags_fn("COOKBOOK_CARGO_FLAGS", cargoflags),
                    cargopath.as_deref().unwrap_or(".")
                );
                if cargopackages.is_empty() && cargoexamples.is_empty() {
                    script += "cookbook_cargo\n"
                } else {
                    if !cargopackages.is_empty() {
                        script += if *cargopackagesprefixed {
                            "cookbook_cargo_packages_prefixed"
                        } else {
                            "cookbook_cargo_packages"
                        };
                        for package in cargopackages {
                            script += " ";
                            script += package;
                        }
                        script += "\n";
                    }
                    if !cargoexamples.is_empty() {
                        script += "cookbook_cargo_examples";
                        for example in cargoexamples {
                            script += " ";
                            script += example;
                        }
                        script += "\n";
                    }
                }

                script
            }
            BuildKind::Configure { configureflags } => format!(
                "DYNAMIC_INIT\n{}cookbook_configure",
                flags_fn("COOKBOOK_CONFIGURE_FLAGS", configureflags),
            ),
            BuildKind::Cmake { cmakeflags } => format!(
                "DYNAMIC_INIT\n{}cookbook_cmake",
                flags_fn("COOKBOOK_CMAKE_FLAGS", cmakeflags),
            ),
            BuildKind::Meson { mesonflags } => format!(
                "DYNAMIC_INIT\n{}cookbook_meson",
                flags_fn("COOKBOOK_MESON_FLAGS", mesonflags),
            ),
            BuildKind::Python {
                pyprojectpath,
                legacysetup,
                pipflags,
            } => format!(
                "DYNAMIC_INIT\n{}{}{}cookbook_python",
                bool_fn("COOKBOOK_PYTHON_LEGACY_SETUP=1\n", legacysetup),
                pyprojectpath
                    .as_ref()
                    .map(|s| format!("COOKBOOK_PYTHON_PYPROJECT_PATH={s}\n"))
                    .unwrap_or("".into()),
                flags_fn("COOKBOOK_PYTHON_PIP_FLAGS", pipflags),
            ),
            BuildKind::Custom { script } => script.clone(),
            BuildKind::Remote => unreachable!(),
            BuildKind::None => "".to_owned(),
        };

        let command = {
            //TODO: remove unwraps
            let cookbook_build = build_dir.canonicalize().unwrap();
            let cookbook_recipe = recipe_dir.canonicalize().unwrap();
            let cookbook_root = Path::new(".").canonicalize().unwrap();
            let cookbook_stage = stage_dir_tmp.canonicalize().unwrap();
            let cookbook_source = source_dir.canonicalize().unwrap();
            let cookbook_sysroot = sysroot_dir.canonicalize().unwrap();
            let cookbook_toolchain = toolchain_dir.canonicalize().ok();
            let bash_args = if cook_config.verbose_cmd { "-ex" } else { "-e" };
            let local_redoxer = Path::new("target/release/cookbook_redoxer");
            let mut command = if is_redox() && !local_redoxer.is_file() {
                let mut command = Command::new("cookbook_redoxer");
                command.env("COOKBOOK_REDOXER", "cookbook_redoxer");
                command
            } else {
                let cookbook_redoxer = local_redoxer
                    .canonicalize()
                    .unwrap_or(PathBuf::from("/bin/false"));
                let mut command = Command::new(&cookbook_redoxer);
                command.env("COOKBOOK_REDOXER", &cookbook_redoxer);
                command
            };
            command.arg("env").arg("bash").arg(bash_args);
            command.current_dir(&cookbook_build);
            command.env("TARGET", cook_recipe.target);
            command.env("COOKBOOK_BUILD", &cookbook_build);
            command.env("COOKBOOK_NAME", name.recipe());
            command.env("COOKBOOK_HOST_TARGET", redoxer::host_target());
            command.env("COOKBOOK_RECIPE", &cookbook_recipe);
            command.env("COOKBOOK_ROOT", &cookbook_root);
            command.env("COOKBOOK_STAGE", &cookbook_stage);
            command.env("COOKBOOK_SOURCE", &cookbook_source);
            command.env("COOKBOOK_SYSROOT", &cookbook_sysroot);
            if let Some(cookbook_toolchain) = &cookbook_toolchain {
                command.env("COOKBOOK_TOOLCHAIN", cookbook_toolchain);
            } else if name.is_host() && !cook_recipe.runs_in_guest() {
                command.env("COOKBOOK_TOOLCHAIN", &cookbook_sysroot);
            }
            // Tools built for the guest are Redox binaries. Scripts reach them
            // through the guest runner instead of running them directly, so it
            // needs to know where it is, and the shims that stand in for the
            // tools' binaries on PATH.
            if let Some(guest_tools) = crate::guest_tools_dir() {
                command.env("COOKBOOK_GUEST_TOOLS", &guest_tools);
                // The shims run `repo` themselves, and the cookbook's working
                // directory is the recipe's build dir, so it is named absolutely.
                let repo = cookbook_root
                    .join("target/release/repo")
                    .canonicalize()
                    .map_err(wrap_io_err!(
                        "locating the repo binary for guest build tools"
                    ))?;
                command.env("COOKBOOK_REPO_BIN", &repo);
                if let Ok(tools) = guest_tools_dir.canonicalize() {
                    command.env(
                        "COOKBOOK_GUEST_TOOLS_SHIMS",
                        write_guest_tool_shims(&guest_tools, &tools, &repo)?,
                    );
                }
            }
            if let Some(compiler) = recipe.build.compiler.as_ref() {
                match compiler {
                    BuildCompiler::Gcc | BuildCompiler::GccNoLTO => {
                        command.env("REDOXER_USE_CLANG", "0");
                    }
                    BuildCompiler::Clang | BuildCompiler::ClangNoLTO => {
                        command.env("REDOXER_USE_CLANG", "1");
                    }
                    _ => {}
                }
                match compiler {
                    BuildCompiler::ClangNoLTO | BuildCompiler::GccNoLTO | BuildCompiler::NoLTO => {
                        command.env("REDOXER_USE_LTO", "0");
                    }
                    _ => {}
                }
            };
            command.env("COOKBOOK_MAKE_JOBS", cli_jobs.to_string());
            if cook_config.verbose_cmd {
                command.env("COOKBOOK_VERBOSE", "1");
            }
            if cook_config.offline && allow_cargo_offline {
                command.env("COOKBOOK_OFFLINE", "1");
            } else {
                command.env_remove("COOKBOOK_OFFLINE");
            }
            if let Ok(ident_source) = fetch::fetch_get_source_info(cook_recipe) {
                command.env("COOKBOOK_SOURCE_IDENT", ident_source.source_identifier);
                command.env("COOKBOOK_COMMIT_IDENT", ident_source.commit_identifier);
            }
            command
        };

        let full_script = format!(
            "{}\n{}\n{}\n{}",
            BUILD_PRESCRIPT, SHARED_PRESCRIPT, script, BUILD_POSTSCRIPT
        );
        fs::run_command_stdin(command, full_script.as_bytes(), logger)?;

        // Move to each features dir
        let mut globs = Vec::new();
        for (i, feat) in recipe.optional_packages.iter().enumerate() {
            let stage_dir = &stage_dirs[i];
            fs::create_dir_clean(stage_dir)?;
            for path in &feat.files {
                let glob = globset::Glob::new(path).map_err(|e| format!("{}", e))?;
                globs.push((glob.compile_matcher(), stage_dir.clone()));
            }
        }
        fs::move_dir_all_fn(
            &stage_dir_tmp,
            &Box::new(|path: PathBuf| {
                for (glob, dst) in &globs {
                    if glob.is_match(&path) {
                        return Some(dst.as_path());
                    }
                }
                None
            }),
        )
        .map_err(wrap_io_err!("Moving to stages dir"))?;

        // Move stage.tmp to stage atomically
        fs::rename(&stage_dir_tmp, stage_dir)?;
    }

    if cook_config.clean_target {
        fs::remove_all(&build_dir)?;
        clean_deps_dir(&sysroot_dir)?;
        if toolchain_dir.is_dir() {
            clean_deps_dir(&toolchain_dir)?;
        }
        if guest_tools_dir.is_dir() {
            clean_deps_dir(&guest_tools_dir)?;
        }
        // don't remove stage dir yet
    }

    let auto_deps = make_auto_deps!(false)?;
    Ok(BuildResult::new(stage_dirs, auto_deps))
}

fn native_sysroot_is_newer(stamp: Option<std::time::SystemTime>, packages: &[PathBuf]) -> bool {
    stamp.is_some_and(|stamp| {
        packages
            .iter()
            .any(|package| fs::modified(package).map_or(true, |stage| stamp > stage))
    })
}

// A native sysroot contains static relibc objects outside normal recipe deps.
// Kernel/UEFI do not link them, and relibc itself creates the sysroot stamp.
fn native_sysroot_modified(recipe_dir: &Path) -> Option<std::time::SystemTime> {
    if matches!(
        recipe_dir.file_name()?.to_str()?,
        "relibc" | "kernel" | "bootloader"
    ) {
        return None;
    }
    let stamp = std::env::var_os("FERRIC_NATIVE_SYSROOT_STAMP")?;
    fs::modified(Path::new(&stamp)).ok()
}

fn build_is_source_newer(
    logger: &PtyOut,
    recipe_dir: &Path,
    source_dir: &Path,
    auto_deps_file: &Path,
    stage_pkgars: Vec<PathBuf>,
    native_sysroot_modified: Option<std::time::SystemTime>,
) -> bool {
    if !auto_deps_file.is_file() || native_sysroot_is_newer(native_sysroot_modified, &stage_pkgars)
    {
        return true;
    }
    let Ok(stage_modified) = fs::modified_all(&stage_pkgars, fs::modified) else {
        return true;
    };
    let Ok(mut source_modified) = fs::modified_dir_ignore_git(source_dir) else {
        return false;
    };
    let mut recipe_is_newest = false;
    if let Ok(recipe_modified) = fs::modified(&recipe_dir.join("recipe.toml"))
        && recipe_modified > source_modified
    {
        source_modified = recipe_modified;
        recipe_is_newest = true;
    }
    let newer = source_modified > stage_modified;
    if newer {
        log_to_pty!(
            logger,
            "DEBUG: updating build: {} is newer",
            if recipe_is_newest { "recipe" } else { "source" }
        );
    }
    newer
}

pub fn remove_stage_dir(stage_dir: &PathBuf) -> crate::Result<()> {
    if stage_dir.is_dir() {
        fs::remove_all(stage_dir)?;
    }
    let stage_file = stage_dir.with_added_extension("pkgar");
    if stage_file.is_file() {
        fs::remove_all(&stage_file)?;
    }
    let stage_meta = stage_dir.with_added_extension("toml");
    if stage_meta.is_file() {
        fs::remove_all(&stage_meta)?;
    }
    let stage_files = stage_dir.with_added_extension("files");
    if stage_files.is_file() {
        fs::remove_all(&stage_files)?;
    }
    Ok(())
}

pub fn get_stage_dirs(features: &Vec<OptionalPackageRecipe>, target_dir: &Path) -> Vec<PathBuf> {
    let mut target_dir = target_dir.to_path_buf();
    if let Some(cross_target) = crate::cross_target() {
        // TODO: automatically pass COOKBOOK_CROSS_GNU_TARGET?
        target_dir = target_dir.join(cross_target)
    }
    let mut v = Vec::new();
    for f in features {
        v.push(target_dir.join(format!("stage.{}", f.name)));
    }
    // intentionally added last as it contains leftover files from package features
    v.push(target_dir.join("stage"));
    v
}

pub fn get_sub_target_dir(target_dir: &Path, sub_path: &str) -> PathBuf {
    let mut target_dir = target_dir.to_path_buf();
    if let Some(cross_target) = crate::cross_target() {
        // TODO: automatically pass COOKBOOK_CROSS_GNU_TARGET?
        target_dir = target_dir.join(cross_target)
    }
    target_dir.join(sub_path)
}

/// Writes a shim per tool in `toolchain/bin` and returns the shim directory.
///
/// `toolchain` is the recipe's `guest-tools` directory: the tools built for the
/// target. Tools that run on the host are in its `toolchain` directory instead
/// and are run directly.
///
/// With `build.build_tools = "guest"` those binaries are built for the target,
/// so the host cannot run them. A shim of the same name runs the real one in the
/// guest environment (`repo guest-run`), which keeps a recipe script that calls
/// a tool by name working unchanged. The directory is per build and shared by
/// the recipes cooked from it.
///
/// The shim runs the tool by its **name**, not by its host path. The toolchain
/// holds the same tools the guest environment installed under the same
/// basenames, and the guest resolves the name on its `PATH`, whereas the
/// toolchain path is a host directory the guest does not have. The guest
/// command a shim passes is therefore the file name alone.
///
/// `Error` is large, like every `Result` in this module; the error paths here
/// are startup failures, not per-file ones, so boxing buys nothing.
#[allow(clippy::result_large_err)]
fn write_guest_tool_shims(guest_tools: &str, toolchain: &Path, repo: &Path) -> Result<PathBuf> {
    let bin = toolchain.join("bin");
    let shims = toolchain.join("guest-shims");
    if shims.is_dir() {
        fs::remove_all(&shims)?;
    }
    fs::create_dir(&shims)?;
    let entries = match std::fs::read_dir(&bin) {
        Ok(entries) => entries,
        // A recipe with no tools has no toolchain binaries to stand in for.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(shims),
        Err(source) => {
            return Err(wrap_io_err!(bin.clone(), "reading the toolchain binaries")(
                source,
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(wrap_io_err!(bin.clone(), "reading the toolchain binaries"))?;
        let name = entry.file_name();
        let shim = shims.join(&name);
        // The guest has the tool installed under this same name on PATH, so
        // the command is the name and never the toolchain's host path.
        let command = name.to_str().ok_or_else(|| Error::Io {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a toolchain binary's name is not valid UTF-8",
            ),
            path: Some(entry.path()),
            context: "naming the guest tool a shim runs",
        })?;
        // `repo` sits beside the cookbook_redoxer shim the recipe runs under,
        // so the shim finds it by the same path rather than guessing one.
        let script = format!(
            "#!/bin/sh\n# Generated: runs the tool built for the target in the guest.\nexec \"{}\" guest-run --guest \"{}\" -- \"{}\" \"$@\"\n",
            shell_quote(&repo.display().to_string()),
            shell_quote(guest_tools),
            shell_quote(command),
        );
        std::fs::write(&shim, script)
            .map_err(wrap_io_err!(shim.clone(), "writing a guest tool shim"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).map_err(
                wrap_io_err!(shim.clone(), "setting a guest tool shim executable"),
            )?;
        }
    }
    Ok(shims)
}

/// Quotes a path for the `sh` shim, which sees it inside double quotes.
fn shell_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Removes `deps_dir` when the recipe no longer has packages for it, and says
/// whether it was already absent.
///
/// A directory left by an earlier build would still be used: a leftover
/// `toolchain` puts host-built libraries in front of the target ones, and a
/// leftover `guest-tools` keeps generating shims for a tool that now runs on the
/// host, ahead of its host binary. Finding one means the recipe's dependencies
/// changed, so it reports the build as not cached.
///
/// # Errors
/// Returns an error when the directory exists and cannot be removed.
#[allow(clippy::result_large_err)]
fn drop_stale_deps_dir(deps_dir: &Path) -> Result<bool> {
    if deps_dir.exists() {
        fs::remove_all(deps_dir)?;
        Ok(false)
    } else {
        Ok(true)
    }
}

fn build_deps_dir(
    logger: &PtyOut,
    deps_dir: &PathBuf,
    dep_pkgars: &BTreeSet<(RecipeName, PathBuf)>,
) -> Result<bool> {
    let pkey_path = fox_core::cookbook::PUBLIC_KEY;
    let pkey_file = match PublicKeyFile::open(pkey_path) {
        Ok(k) => k.pkey,
        Err(e) => {
            if !dep_pkgars.is_empty() {
                return Err(Error::from(e));
            } else {
                // should never be accessed
                Default::default()
            }
        }
    };
    let tags_dir = deps_dir.join(".tags");
    if tags_dir.is_dir() {
        // check all files present and exact
        let mut cached = fs::check_files_present(
            &tags_dir,
            &dep_pkgars
                .iter()
                .map(|(name, _)| name.without_prefix())
                .collect(),
        )?;
        if cached {
            for (name, pkgar_path) in dep_pkgars {
                let tag_file = tags_dir.join(name.without_prefix());
                let Ok(tag_hash) = blake3::Hash::from_hex(fs::read_to_string(&tag_file)?) else {
                    log_to_pty!(
                        logger,
                        "DEBUG: updating {:?}: {:?} is absent",
                        deps_dir.file_name().unwrap().display(),
                        name.as_str(),
                    );
                    cached = false;
                    continue;
                };
                let pkgar_hash = PackageFile::new(pkgar_path, &pkey_file)?.header().blake3;
                if *tag_hash.as_bytes() != pkgar_hash {
                    log_to_pty!(
                        logger,
                        "DEBUG: updating {:?}: {:?} is updated",
                        deps_dir.file_name().unwrap().display(),
                        name.as_str(),
                    );
                    cached = false;
                }
            }
        }
        if cached {
            return Ok(true);
        }
        fs::remove_all(deps_dir)?;
    }

    // Create sysroot.tmp
    let deps_dir_tmp = deps_dir.with_added_extension("tmp");
    fs::create_dir_clean(&deps_dir_tmp)?;
    let tags_dir = deps_dir_tmp.join(".tags");
    let usr_dir = deps_dir_tmp.join("usr");
    fs::create_dir(&tags_dir)?;
    fs::create_dir(&usr_dir)?;

    for folder in &["bin", "include", "lib", "share"] {
        // Make sure sysroot/usr/$folder exists
        fs::create_dir(&usr_dir.join(folder))?;

        // Link sysroot/$folder sysroot/usr/$folder
        fs::symlink(Path::new("usr").join(folder), deps_dir_tmp.join(folder))?;
    }

    for (name, archive_path) in dep_pkgars {
        let tag_file = tags_dir.join(name.without_prefix());
        let mut package = PackageFile::new(archive_path, &pkey_file)?;
        Transaction::install(&mut package, &deps_dir_tmp)?.commit()?;
        let hash = blake3::Hash::from_bytes(package.header().blake3).to_hex();
        std::fs::write(&tag_file, hash.as_bytes()).map_err(wrap_io_err!("Writing tag"))?;
    }

    // Move sysroot.tmp to sysroot atomically
    fs::rename(&deps_dir_tmp, deps_dir)?;

    Ok(false)
}

fn clean_deps_dir(deps_dir: &PathBuf) -> Result<bool> {
    // this retain tags for future check
    let tags_dir = deps_dir.join(".tags");
    let tags_dir_tmp = deps_dir.with_added_extension("tags");
    fs::rename(&tags_dir, &tags_dir_tmp)?;
    fs::remove_all(deps_dir)?;
    fs::create_dir(deps_dir)?;
    fs::rename(&tags_dir_tmp, &tags_dir)?;
    Ok(false)
}

/// Calculate automatic dependencies
fn build_auto_deps(
    recipe: &Recipe,
    auto_deps_path: &Path,
    stage_dirs: &Vec<PathBuf>,
    cached: bool,
    cook_config: &CookConfig,
    mut dep_pkgars: BTreeSet<(RecipeName, PathBuf)>,
    logger: &PtyOut,
) -> Result<BTreeSet<RecipeName>> {
    if auto_deps_path.is_file() && !cached {
        if cook_config.verbose {
            log_to_pty!(logger, "DEBUG: updating {}", auto_deps_path.display());
        }
        fs::remove_all(auto_deps_path)?;
    }

    let auto_deps = if auto_deps_path.exists() {
        let wrapper: AutoDeps = fs::read_toml(auto_deps_path)?;
        wrapper.packages
    } else {
        let mut dynamic_deps = auto_deps_from_dynamic_linking(stage_dirs, &dep_pkgars, logger);
        dep_pkgars.retain(|x| recipe.build.dependencies.contains(&x.0));
        let package_deps =
            auto_deps_from_static_package_deps(&dep_pkgars, &dynamic_deps).unwrap_or_default();
        dynamic_deps.extend(package_deps);

        let wrapper = AutoDeps {
            packages: dynamic_deps,
        };
        fs::serialize_and_write(auto_deps_path, &wrapper)?;
        wrapper.packages
    };
    Ok(auto_deps)
}

pub fn build_remote(
    stage_dirs: Vec<PathBuf>,
    stage_pkgars: Vec<PathBuf>,
    recipe: &Recipe,
    target_dir: &Path,
    logger: &PtyOut,
) -> Result<BuildResult> {
    let source_toml = target_dir.join("source.toml");
    let auto_deps_path = target_dir.join("auto_deps.toml");
    let Some(remote) = fetch_repo::binary_repo() else {
        bail_other_err!("There is no prebuilt package remote to check packages against")
    };
    let source_pubkey = remote.key();

    let packages = recipe.get_packages_list();
    let mut cached = auto_deps_path.is_file();
    for (i, package) in packages.iter().enumerate() {
        let stage_pkgar = &stage_pkgars[i];

        if !stage_pkgar.is_file() {
            cached = false;
            break;
        }

        let (_, source_pkgar, _) = package_source_paths(*package, target_dir);
        if fs::modified(&source_pkgar)? > fs::modified(stage_pkgar)? {
            cached = false;
            break;
        }
    }

    if cached {
        log_to_pty!(logger, "DEBUG: using cached build");
        let wrapper: AutoDeps = fs::read_toml(&auto_deps_path)?;
        return Ok(BuildResult::cached(stage_dirs, wrapper.packages));
    }

    for (i, package) in packages.into_iter().enumerate() {
        let stage_dir = &stage_dirs[i];
        let (_, source_pkgar, _) = package_source_paths(package, target_dir);
        fs::create_dir_clean(stage_dir)?;
        pkgar::extract(source_pubkey, &source_pkgar, stage_dir).map_err(|e| {
            if matches!(e, pkgar::Error::Core(pkgar_core::Error::Dryoc(_))) {
                hint_incorrect_pkey(logger, source_pubkey, &source_pkgar);
            }
            e
        })?
    }

    if auto_deps_path.is_file() {
        fs::remove_all(&auto_deps_path)?
    }

    let auto_deps = {
        let toml_content = fs::read_to_string(&source_toml)?;
        let pkg_toml = PackageInfo::from_toml(&toml_content)?;
        let wrapper = AutoDeps {
            packages: pkg_toml.depends.into_iter().collect(),
        };
        fs::serialize_and_write(&auto_deps_path, &wrapper)?;
        wrapper.packages
    };
    Ok(BuildResult::new(stage_dirs, auto_deps))
}

fn hint_incorrect_pkey(logger: &PtyOut, source_pubkey: &Path, source_pkgar: &Path) -> Option<()> {
    // TODO: Move to pkgar
    let mut file = std::fs::File::open(source_pkgar).ok()?;
    let mut buffer = vec![0u8; pkgar_core::HEADER_SIZE];
    file.read_exact(&mut buffer).ok()?;
    log_to_pty!(
        logger,
        "DEBUG: Pkgar key mismatch, hint:\n - expected: {:?}\n - current:  {:?}",
        blake3::Hash::from_bytes(unsafe {
            pkgar_core::Header::new_unchecked(&buffer).ok()?.public_key
        })
        .to_hex()
        .to_string(),
        blake3::Hash::from_bytes(PublicKeyFile::open(source_pubkey).ok()?.pkey)
            .to_hex()
            .to_string(),
    );

    Some(())
}

#[cfg(test)]
mod tests {
    /// A tool that moved from the guest to the host leaves its `guest-tools`
    /// directory behind. It must go, and its going counts as a dependency
    /// change, or its shims would shadow the host binary.
    #[test]
    fn a_leftover_deps_dir_is_removed_and_invalidates_the_build() {
        let temp = tempfile::tempdir().unwrap();
        let guest_tools = temp.path().join("guest-tools");
        std::fs::create_dir_all(guest_tools.join("bin")).unwrap();
        std::fs::write(guest_tools.join("bin/python3.12"), "").unwrap();
        assert!(!super::drop_stale_deps_dir(&guest_tools).unwrap());
        assert!(!guest_tools.exists());
        assert!(super::drop_stale_deps_dir(&guest_tools).unwrap());
    }

    use fox_core::cookbook::RecipeName;
    use std::os::unix;
    use std::os::unix::fs::PermissionsExt;

    /// Builds the name of a recipe with `prefix` as its build selector, the
    /// way `recipes/…/recipe.toml` names one.
    fn recipe(name: &str, prefix: &str) -> RecipeName {
        let name = format!("{prefix}{name}");
        RecipeName::new(name).unwrap()
    }

    /// A tool built for the guest is a target binary: it must link the target
    /// packages (its own libraries and the target libraries it uses) and must
    /// not be handed the host toolchain, which would put host-built libraries
    /// in front of the target ones.
    #[test]
    fn a_guest_built_tool_builds_against_the_target_sysroot_only() {
        assert_eq!(
            super::dependency_sets(&recipe("mdbook", "host:"), true),
            super::DependencySets {
                sysroot: super::SysrootDeps::Target,
                toolchain: false,
            }
        );
    }

    /// A tool built for the host compiles and links on the host, so its own
    /// host packages are its sysroot; a target package must not be, or the
    /// host compiler would link target binaries. It runs its tools from that
    /// same toolchain.
    #[test]
    fn a_host_built_tool_builds_against_its_host_tools() {
        assert_eq!(
            super::dependency_sets(&recipe("mdbook", "host:"), false),
            super::DependencySets {
                sysroot: super::SysrootDeps::Host,
                toolchain: true,
            }
        );
    }

    /// An ordinary recipe is a target package, so its dependencies are target
    /// packages; the host tools in its dependency walk are the ones its build
    /// scripts run.
    #[test]
    fn a_target_recipe_builds_against_the_target_sysroot_and_host_tools() {
        assert_eq!(
            super::dependency_sets(&recipe("ncdu", "target:"), false),
            super::DependencySets {
                sysroot: super::SysrootDeps::Target,
                toolchain: true,
            }
        );
        // An unprefixed name is a target package too.
        assert_eq!(
            super::dependency_sets(&recipe("ncdu", ""), true),
            super::DependencySets {
                sysroot: super::SysrootDeps::Target,
                toolchain: true,
            }
        );
    }

    /// Metapackages, remote builds and a toolchain-less build all have to pick
    /// the ordinary target sysroot: an unprefixed recipe name is a target
    /// package, and a `host:` one only becomes a host binary when its tools
    /// are not built for the guest. The choice reads the name and the mode,
    /// never the packages found, so a rebuild that clears the cached
    /// directories selects the same sets as the cold build did.
    #[test]
    fn the_choice_follows_the_name_prefix_and_the_mode() {
        for tools_are_target in [false, true] {
            let target = super::dependency_sets(&recipe("base", ""), tools_are_target);
            assert_eq!(target.sysroot, super::SysrootDeps::Target);

            let tool = super::dependency_sets(&recipe("gperf", "host:"), tools_are_target);
            assert_eq!(tool.sysroot == super::SysrootDeps::Target, tools_are_target);
            assert_eq!(tool.toolchain, !tools_are_target);
        }
    }

    /// A tool built for the guest cannot run on the host, so each toolchain
    /// binary gets a shim that dispatches it into the guest under the name the
    /// recipe already calls.
    #[test]
    fn guest_tool_shims_are_written_for_every_toolchain_binary() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("toolchain/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("mdbook"), "").unwrap();
        std::fs::write(bin.join("gperf"), "").unwrap();

        let repo = temp.path().join("target/release/repo");
        std::fs::create_dir_all(repo.parent().unwrap()).unwrap();
        std::fs::write(&repo, "").unwrap();
        let shims = super::write_guest_tool_shims(
            "/guest/guest.img",
            &temp.path().join("toolchain"),
            &repo,
        )
        .unwrap();

        for tool in ["mdbook", "gperf"] {
            let shim = shims.join(tool);
            let script = std::fs::read_to_string(&shim).unwrap();
            assert!(
                script.contains("guest-run") && script.contains("/guest/guest.img"),
                "the shim dispatches {tool} into the guest: {script}"
            );
            assert!(
                script.contains(repo.to_str().unwrap()),
                "the shim runs the repo beside this checkout: {script}"
            );
            // The guest has the tool on PATH under the same name, so the shim
            // names it. The host toolchain path does not exist in the guest.
            let guest_command = script
                .rsplit("\" -- \"")
                .next()
                .and_then(|rest| rest.split('\"').next())
                .unwrap_or_default();
            assert_eq!(guest_command, tool, "the shim names the tool: {script}");
            assert!(
                !script.contains(&bin.join(tool).display().to_string()),
                "the shim does not pass the host toolchain path: {script}"
            );
            // The shim stands in for the tool on PATH, so it must be executable.
            assert!(
                std::fs::metadata(&shim).unwrap().permissions().mode() & 0o111 != 0,
                "{tool}'s shim is executable"
            );
        }
    }

    /// The shim's guest command is the tool's file name, never a path: the
    /// guest has the tool on its PATH, and a path would be treated as a host
    /// file to copy rather than a program to run.
    #[test]
    fn guest_tool_shims_pass_a_bare_tool_name_not_a_path() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("toolchain/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("pkg-config"), "").unwrap();

        let repo = temp.path().join("repo");
        std::fs::write(&repo, "").unwrap();
        let shims = super::write_guest_tool_shims(
            "/guest/guest.img",
            &temp.path().join("toolchain"),
            &repo,
        )
        .unwrap();

        let script = std::fs::read_to_string(shims.join("pkg-config")).unwrap();
        assert!(
            script.ends_with("-- \"pkg-config\" \"$@\"\n"),
            "the shim ends with the bare tool name: {script}"
        );
        // Neither the host toolchain path nor a guest path may appear as the
        // command: the guest resolves the bare name on its PATH.
        for fragment in [bin.display().to_string(), "/guest/toolchain".to_string()] {
            assert!(
                !script.contains(&format!("-- \"{fragment}")),
                "the shim passes no path for {fragment}: {script}"
            );
        }
    }

    #[test]
    fn native_sysroot_checks_every_required_package_variant() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let auto_deps = temp.path().join("auto-deps.toml");
        std::fs::write(&auto_deps, "").unwrap();
        let stamp = std::time::SystemTime::now();
        let old = stamp - std::time::Duration::from_secs(60);
        let new = stamp + std::time::Duration::from_secs(60);
        let package = temp.path().join("stage.pkgar");
        let dev = temp.path().join("stage.dev.pkgar");
        for (path, modified) in [(&source, old), (&package, old), (&dev, new)] {
            if path != &source {
                std::fs::write(path, "package").unwrap();
            }
            std::fs::File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }
        let variants = vec![package.clone(), dev.clone()];
        assert!(super::native_sysroot_is_newer(Some(stamp), &variants));
        assert!(super::build_is_source_newer(
            &None,
            temp.path(),
            &source,
            &auto_deps,
            variants.clone(),
            Some(stamp)
        ));
        std::fs::remove_file(&package).unwrap();
        assert!(super::native_sysroot_is_newer(Some(stamp), &variants));
        assert!(super::build_is_source_newer(
            &None,
            temp.path(),
            &source,
            &auto_deps,
            variants,
            Some(stamp)
        ));
        assert!(!super::native_sysroot_is_newer(Some(stamp), &[dev]));
    }

    #[test]
    fn refreshed_native_sysroot_invalidates_statically_linked_packages() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let auto_deps = temp.path().join("auto-deps.toml");
        std::fs::write(&auto_deps, "").unwrap();
        let package = temp.path().join("stage.pkgar");
        std::fs::write(&package, "package").unwrap();
        let stage_time = super::fs::modified(&package).unwrap();
        assert!(!super::native_sysroot_is_newer(None, &[package.clone()]));
        assert!(!super::native_sysroot_is_newer(
            Some(stage_time),
            &[package.clone()]
        ));
        assert!(super::native_sysroot_is_newer(
            Some(stage_time + std::time::Duration::from_secs(2)),
            &[package.clone()]
        ));
        std::fs::File::open(&source)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(stage_time - std::time::Duration::from_secs(2)),
            )
            .unwrap();
        assert!(!super::build_is_source_newer(
            &None,
            temp.path(),
            &source,
            &auto_deps,
            vec![package.clone()],
            None
        ));
        assert!(super::build_is_source_newer(
            &None,
            temp.path(),
            &source,
            &auto_deps,
            vec![package],
            Some(stage_time + std::time::Duration::from_secs(2))
        ));
    }

    #[test]
    fn file_system_loop_no_infinite_loop() {
        let mut root = std::env::temp_dir();
        root.push("temp_test_dir_file_system_loop_no_infinite_loop");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("Failed to create temporary root directory");

        // Hierarchy with an infinite loop
        let dir = root.join("loop");
        unix::fs::symlink(&root, &dir).expect("Linking {dir:?} to {root:?}");

        // Sanity check that we have a loop
        assert_eq!(
            root.canonicalize().unwrap(),
            dir.canonicalize().unwrap(),
            "Expected a loop where {dir:?} points to {root:?}"
        );

        let entries =
            super::auto_deps_from_dynamic_linking(&vec![root.clone()], &Default::default(), &None);
        assert!(
            entries.is_empty(),
            "auto_deps shouldn't have yielded any libraries"
        );
    }
}
