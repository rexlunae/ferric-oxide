//! Turns resolved options into files the build reads:
//!
//! - `config.mk`: make variables, included by mk/config.mk
//! - `filesystem.toml`: the base filesystem config plus package and size changes
//! - `kernel-config.toml`: the kernel's config.toml (CPU feature policy)
//! - `trace.toml`: the initfs trace configuration bootstrap applies (`ferric_trace::config`)
//! - `state.toml`: per-recipe fingerprints, used to mark recipes stale

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::model::{Diag, Resolved};
use crate::schema::Schema;
use crate::value::Value;

#[derive(Debug, Default)]
pub struct Output {
    pub make: Vec<(String, String)>,
    pub make_append: BTreeMap<String, Vec<String>>,
    pub kernel_features: BTreeSet<String>,
    pub kernel_config: BTreeMap<String, String>,
    /// The initfs `trace.toml`'s keys and their selections.
    pub trace_config: BTreeMap<String, Vec<String>>,
    pub packages: BTreeSet<String>,
    /// Packages written as `"source"`, so they build from their recipe
    /// even when REPO_BINARY fetches the rest.
    pub source_packages: BTreeSet<String>,
    pub ignore: BTreeSet<String>,
    pub general: BTreeMap<String, toml::Value>,
    pub fingerprints: BTreeMap<String, String>,
}

fn substitute(template: &str, v: &Value) -> String {
    template.replace("{value}", &v.render())
}

fn fnv(data: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Collects every active option's effects.
pub fn collect(schema: &Schema, r: &Resolved) -> Result<Output, Vec<Diag>> {
    let mut out = Output::default();
    let mut errors = Vec::new();
    let mut make_owner: BTreeMap<String, (String, bool)> = BTreeMap::new();
    let mut prints: BTreeMap<String, String> = BTreeMap::new();

    for (i, o) in schema.options.iter().enumerate() {
        let v = &r.values[i];
        let state = if r.active[i] {
            v.to_toml().to_string()
        } else {
            "<inactive>".into()
        };
        for recipe in &o.rebuild {
            prints
                .entry(recipe.clone())
                .or_default()
                .push_str(&format!("{}={state}\n", o.id));
        }
        if !r.active[i] {
            continue;
        }
        let look = |id: &str| {
            if id == "value" {
                Some(v.clone())
            } else {
                r.lookup(schema, id)
            }
        };
        for e in &o.effects {
            let applies = match &e.when {
                None => true,
                Some(w) => match w.eval(&look) {
                    Ok(b) => b,
                    Err(err) => {
                        errors.push(Diag::error(format!("{}: effect condition: {err}", o.id)));
                        false
                    }
                },
            };
            if !applies {
                continue;
            }
            let d = &e.def;
            for (var, tmpl) in &d.make {
                let val = substitute(tmpl, v);
                if let Some((owner, owner_override)) = make_owner.get(var) {
                    if *owner_override {
                        // An explicit override already won.
                        continue;
                    }
                    let existing = out.make.iter().find(|(k, _)| k == var).map(|(_, x)| x);
                    if !d.override_ && existing != Some(&val) {
                        errors.push(Diag::error(format!(
                            "{} and {owner} both set make variable {var}",
                            o.id
                        )));
                        continue;
                    }
                }
                out.make.retain(|(k, _)| k != var);
                out.make.push((var.clone(), val));
                make_owner.insert(var.clone(), (o.id.clone(), d.override_));
            }
            for (var, tmpl) in &d.make_append {
                let val = substitute(tmpl, v);
                if !val.is_empty() {
                    out.make_append.entry(var.clone()).or_default().push(val);
                }
            }
            out.kernel_features
                .extend(d.kernel_features.iter().cloned());
            for (k, tmpl) in &d.kernel_config {
                out.kernel_config.insert(k.clone(), substitute(tmpl, v));
            }
            for (k, tmpl) in &d.trace_config {
                let items = match v {
                    Value::List(items) if tmpl == "{value}" => items.clone(),
                    _ => vec![substitute(tmpl, v)],
                };
                out.trace_config
                    .entry(k.clone())
                    .or_default()
                    .extend(items.into_iter().filter(|item| !item.is_empty()));
            }
            out.packages.extend(d.packages.iter().cloned());
            out.ignore.extend(d.ignore_packages.iter().cloned());
            out.source_packages
                .extend(d.source_packages.iter().cloned());
            if let Value::List(items) = v {
                if d.packages_from_value {
                    out.packages.extend(items.iter().cloned());
                }
                if d.ignore_from_value {
                    out.ignore.extend(items.iter().cloned());
                }
            }
            for (k, tv) in &d.general {
                let tv = match tv {
                    toml::Value::String(s) if s == "{value}" => v.to_toml(),
                    toml::Value::String(s) => toml::Value::String(substitute(s, v)),
                    other => other.clone(),
                };
                out.general.insert(k.clone(), tv);
            }
        }
    }
    for both in out.source_packages.intersection(&out.ignore) {
        errors.push(Diag::error(format!(
            "package {both} is both added and removed"
        )));
    }
    out.packages.retain(|p| !out.source_packages.contains(p));
    for both in out.packages.intersection(&out.ignore) {
        errors.push(Diag::error(format!(
            "package {both} is both added and removed"
        )));
    }
    out.fingerprints = prints.into_iter().map(|(k, v)| (k, fnv(&v))).collect();
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// Mirrors the FILESYSTEM_CONFIG selection in mk/config.mk.
pub fn base_filesystem_config(root: &Path, arch: &str, board: &str, name: &str) -> PathBuf {
    if board.is_empty() {
        let per_arch = Path::new("config").join(arch).join(format!("{name}.toml"));
        if root.join(&per_arch).is_file() {
            per_arch
        } else {
            Path::new("config").join(format!("{name}.toml"))
        }
    } else {
        Path::new("config")
            .join(arch)
            .join(board)
            .join(format!("{name}.toml"))
    }
}

/// `to` relative to directory `from`; both relative to the same root.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from.components().collect();
    let to_c: Vec<Component> = to.components().collect();
    let common = from.iter().zip(&to_c).take_while(|(a, b)| a == b).count();
    let mut p = PathBuf::new();
    for _ in common..from.len() {
        p.push("..");
    }
    for c in &to_c[common..] {
        p.push(c);
    }
    p
}

/// The `include` of the generated `filesystem.toml` in `out_dir` for the base configuration
/// `base` (relative to `root`). The installer resolves it from the file's directory, so it is
/// relative for an `out_dir` under the build root, and the base configuration's absolute path
/// for an absolute `out_dir` (`--out` given as one), which shares no prefix with `base`.
fn filesystem_include(root: &Path, out_dir: &Path, base: &Path) -> Result<PathBuf, Vec<Diag>> {
    if !out_dir.is_absolute() {
        return Ok(relative(out_dir, base));
    }
    let path = root.join(base);
    std::fs::canonicalize(&path).map_err(|err| {
        vec![Diag::error(format!(
            "cannot resolve {} for an absolute output directory: {err}",
            path.display()
        ))]
    })
}

/// `path` for a make variable that recipes read from another directory: under `$(CURDIR)`
/// (the build root) when relative, as it is when absolute (`--out` given as one).
fn make_path(path: &Path) -> String {
    let text = make_escape(&path.to_string_lossy());
    if path.is_absolute() {
        text
    } else {
        format!("$(CURDIR)/{text}")
    }
}

fn make_escape(s: &str) -> String {
    s.replace('$', "$$").replace('#', "\\#")
}

pub struct Plan {
    pub files: Vec<(PathBuf, String)>,
    pub remove: Vec<PathBuf>,
    pub fingerprints: BTreeMap<String, String>,
}

/// Renders the generated files. `out_dir` is relative to the build root.
pub fn plan(schema: &Schema, r: &Resolved, root: &Path, out_dir: &Path) -> Result<Plan, Vec<Diag>> {
    let out = collect(schema, r)?;
    let get = |id: &str| r.value(schema, id).map(Value::render).unwrap_or_default();
    let arch = get("target.arch");
    let board = if r.active[schema.position("target.board").unwrap()] {
        get("target.board")
    } else {
        String::new()
    };
    let config_name = get("image.config");

    let base = base_filesystem_config(root, &arch, &board, &config_name);
    if !root.join(&base).is_file() {
        return Err(vec![Diag::error(format!(
            "image.config = \"{config_name}\" needs {}, which does not exist for {arch}",
            base.display()
        ))]);
    }

    let all_values: String = schema
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("{}={}\n", o.id, r.values[i].to_toml()))
        .collect();
    let hash = fnv(&all_values);

    let mut files = Vec::new();
    let mut remove = Vec::new();

    // filesystem.toml
    let fs_path = out_dir.join("filesystem.toml");
    let mut fs = String::new();
    fs.push_str("# Generated by ferric-config from ferric.toml. Do not edit.\n");
    fs.push_str(&format!("# config-hash: {hash}\n\n"));
    let include = filesystem_include(root, out_dir, &base)?;
    fs.push_str(&format!(
        "include = [{}]\n",
        toml::Value::String(include.to_string_lossy().into_owned())
    ));
    if !out.general.is_empty() {
        fs.push_str("\n[general]\n");
        for (k, v) in &out.general {
            fs.push_str(&format!("{k} = {v}\n"));
        }
    }
    if !out.packages.is_empty() || !out.source_packages.is_empty() || !out.ignore.is_empty() {
        fs.push_str("\n[packages]\n");
        for p in &out.packages {
            fs.push_str(&format!("{} = {{}}\n", toml_key(p)));
        }
        for p in &out.source_packages {
            fs.push_str(&format!("{} = \"source\"\n", toml_key(p)));
        }
        for p in &out.ignore {
            fs.push_str(&format!("{} = \"ignore\"\n", toml_key(p)));
        }
    }
    files.push((fs_path.clone(), fs));

    // kernel-config.toml
    let kc_path = out_dir.join("kernel-config.toml");
    let have_kernel_config = !out.kernel_config.is_empty();
    if have_kernel_config {
        let mut doc = toml_edit::DocumentMut::new();
        for (k, v) in &out.kernel_config {
            let parts: Vec<&str> = k.split('.').collect();
            let (leaf, parents) = parts.split_last().unwrap();
            let mut t = doc.as_table_mut();
            for p in parents {
                if !t.contains_key(p) {
                    let mut nt = toml_edit::Table::new();
                    nt.set_implicit(true);
                    t.insert(p, toml_edit::Item::Table(nt));
                }
                t = t[p].as_table_mut().unwrap();
            }
            t.insert(leaf, toml_edit::value(v.as_str()));
        }
        files.push((
            kc_path.clone(),
            format!(
                "# Generated by ferric-config from ferric.toml. Do not edit.\n# Copied into the kernel source as config.toml by recipes/core/kernel.\n\n{doc}"
            ),
        ));
    } else {
        remove.push(kc_path.clone());
    }

    // trace.toml
    let trace_path = out_dir.join("trace.toml");
    let trace = trace_file(&out.trace_config, &out.kernel_features)?;
    if let Some(text) = &trace {
        files.push((trace_path.clone(), text.clone()));
    } else {
        remove.push(trace_path.clone());
    }

    // config.mk
    let mut mk = String::new();
    mk.push_str(
        "# Generated by ferric-config from ferric.toml. Do not edit: run `make menuconfig`\n\
         # or edit ferric.toml. Values here use ?=, so .config and the command line win.\n\n",
    );
    mk.push_str("FERRIC_CONFIG_ACTIVE:=1\n");
    mk.push_str(&format!("FERRIC_CONFIG_HASH:={hash}\n"));
    mk.push_str(&format!("FERRIC_ARCH:={arch}\n"));
    mk.push_str(&format!("FERRIC_CONFIG_NAME:={config_name}\n"));
    mk.push_str(&format!("FERRIC_BOARD:={board}\n\n"));

    // Target selection first, then settings that do not depend on it. CPU,
    // kernel and image settings only make sense for the configured target,
    // so they sit behind a guard in case ARCH or CONFIG_NAME is overridden.
    let order = |k: &str| match k {
        "ARCH" | "BOARD" | "CONFIG_NAME" => 0,
        k if k.starts_with("FERRIC_") || k == "QEMU_CPU" => 2,
        _ => 1,
    };
    let mut plain_vars: Vec<_> = out.make.iter().filter(|(k, _)| order(k) < 2).collect();
    plain_vars.sort_by_key(|(k, _)| order(k));
    let target_vars: Vec<_> = out.make.iter().filter(|(k, _)| order(k) == 2).collect();
    for (k, v) in &plain_vars {
        mk.push_str(&format!("{k}?={}\n", make_escape(v)));
    }
    mk.push_str("\n# The rest only applies to the configured target.\n");
    mk.push_str("ifeq ($(ARCH)|$(BOARD)|$(CONFIG_NAME),$(FERRIC_ARCH)|$(FERRIC_BOARD)|$(FERRIC_CONFIG_NAME))\n");
    mk.push_str(&format!(
        "FILESYSTEM_CONFIG?={}\n",
        fs_path.to_string_lossy()
    ));
    for (k, v) in &target_vars {
        if k.starts_with("FERRIC_") {
            mk.push_str(&format!("export {k}:={}\n", make_escape(v)));
        } else {
            mk.push_str(&format!("{k}?={}\n", make_escape(v)));
        }
    }
    for (k, parts) in &out.make_append {
        mk.push_str(&format!("export {k}:={}\n", make_escape(&parts.join(" "))));
    }
    mk.push_str(&format!(
        "export FERRIC_KERNEL_FEATURES:={}\n",
        out.kernel_features
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",")
    ));
    if have_kernel_config {
        mk.push_str(&format!(
            "export FERRIC_KERNEL_CONFIG:={}\n",
            make_path(&kc_path)
        ));
    }
    if trace.is_some() {
        mk.push_str(&format!(
            "export FERRIC_INITFS_TRACE:={}\n",
            make_path(&trace_path)
        ));
    }
    mk.push_str("else\n");
    mk.push_str("$(info ferric-config: ARCH, BOARD or CONFIG_NAME differs from ferric.toml; ignoring its image, CPU and kernel settings)\n");
    mk.push_str("endif\n");
    files.push((out_dir.join("config.mk"), mk));

    Ok(Plan {
        files,
        remove,
        fingerprints: out.fingerprints,
    })
}

/// The initfs `trace.toml` for `keys`, or `None` when it would change nothing. The text is
/// parsed back with the format bootstrap uses, and its selections are checked against the
/// events a kernel with `kernel_features` has and against the kernel's and bootstrap's size
/// limits, so a mistake fails here rather than being logged and ignored at boot.
fn trace_file(
    keys: &BTreeMap<String, Vec<String>>,
    kernel_features: &BTreeSet<String>,
) -> Result<Option<String>, Vec<Diag>> {
    let mut config = ferric_trace::config::BootConfig::default();
    for (key, items) in keys {
        match key.as_str() {
            "enable" => config.enable.extend(items.iter().cloned()),
            "disable" => config.disable.extend(items.iter().cloned()),
            other => {
                return Err(vec![Diag::error(format!(
                    "trace_config key {other:?} is not enable or disable"
                ))]);
            }
        }
    }
    if config.is_empty() {
        return Ok(None);
    }
    let check = |err: &dyn std::error::Error| Diag::error(format!("initfs trace.toml: {err}"));
    let catalog = &ferric_trace::kernel::CATALOG;
    let compiled = catalog.compiled_for(|feature| kernel_features.contains(feature));
    config
        .check_compiled(catalog, &compiled)
        .map_err(|err| vec![check(&err)])?;
    let body = toml::to_string(&config).map_err(|err| vec![check(&err)])?;
    let text = format!(
        "# Generated by ferric-config from ferric.toml (kernel.trace_initfs_*). Do not edit.\n\
         # The base package puts it in the initfs at {}; bootstrap applies it before init.\n\n{body}",
        ferric_trace::config::INITFS_PATH
    );
    // What bootstrap reads must be what was meant.
    let parsed = ferric_trace::config::BootConfig::parse(text.as_bytes())
        .map_err(|err| vec![check(&err)])?;
    if parsed != config {
        return Err(vec![Diag::error(
            "initfs trace.toml does not read back as written".to_owned(),
        )]);
    }
    Ok(Some(text))
}

fn toml_key(k: &str) -> String {
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        k.to_owned()
    } else {
        toml::Value::String(k.to_owned()).to_string()
    }
}

pub struct Applied {
    pub written: Vec<PathBuf>,
    pub stale: Vec<String>,
    pub touched: usize,
}

/// A filesystem failure while applying generated configuration.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The output directory could not be created.
    #[error("cannot create configuration directory {}", path.display())]
    CreateOutput {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A generated configuration file could not be written.
    #[error("cannot write generated configuration {}", path.display())]
    WriteOutput {
        /// File that could not be written.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A recipe selected for rebuilding could not be opened for a timestamp update.
    #[error("cannot open recipe {} to mark it stale", path.display())]
    OpenRecipe {
        /// Recipe that could not be opened.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A recipe selected for rebuilding could not be marked stale.
    #[error("cannot update recipe timestamp {}", path.display())]
    TouchRecipe {
        /// Recipe whose modification time could not be updated.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// Applied rebuild fingerprints could not be recorded.
    #[error("cannot write rebuild state {}", path.display())]
    WriteState {
        /// State file that could not be written.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
}

fn read_state(path: &Path) -> Option<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: toml::Table = toml::from_str(&text).ok()?;
    Some(
        v.get("recipes")?
            .as_table()?
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect(),
    )
}

/// Every recipe directory name -> its recipe.toml.
fn recipe_files(root: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut stack = vec![root.join("recipes")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == "source" || name == "target" || name.starts_with('.') {
                    continue;
                }
                stack.push(p);
            } else if e.file_name() == "recipe.toml"
                && let Some(name) = dir.file_name()
            {
                out.entry(name.to_string_lossy().into_owned())
                    .or_default()
                    .push(p);
            }
        }
    }
    out
}

/// Writes the plan, marks recipes whose options changed as stale by
/// touching their recipe.toml (the cookbook rebuilds a recipe whose
/// recipe.toml is newer than its stage), and records applied fingerprints.
/// With `touch` false, generated files are still written, but recipe timestamps
/// and rebuild state remain unchanged so a later application can mark them stale.
///
/// # Errors
///
/// Returns [`ApplyError`] if output creation, a generated write, a recipe open or
/// timestamp update, or a rebuild-state write fails, preserving the path and
/// underlying filesystem error. Recipe failures leave the applied fingerprints
/// unchanged so the next application retries every pending rebuild.
pub fn apply(root: &Path, out_dir: &Path, plan: &Plan, touch: bool) -> Result<Applied, ApplyError> {
    let abs_out = root.join(out_dir);
    std::fs::create_dir_all(&abs_out).map_err(|source| ApplyError::CreateOutput {
        path: abs_out.clone(),
        source,
    })?;
    let mut written = Vec::new();
    for (p, content) in &plan.files {
        let abs = root.join(p);
        if std::fs::read_to_string(&abs).ok().as_deref() != Some(content.as_str()) {
            std::fs::write(&abs, content).map_err(|source| ApplyError::WriteOutput {
                path: abs.clone(),
                source,
            })?;
            written.push(p.clone());
        }
    }
    for p in &plan.remove {
        let _ = std::fs::remove_file(root.join(p));
    }

    let state_path = abs_out.join("state.toml");
    let old = read_state(&state_path).unwrap_or_default();
    let stale: Vec<String> = plan
        .fingerprints
        .iter()
        .filter(|(k, v)| old.get(*k) != Some(v))
        .map(|(k, _)| k.clone())
        .collect();

    let mut touched = 0;
    if touch && !stale.is_empty() {
        let recipes = recipe_files(root);
        let now = SystemTime::now();
        let all = stale.iter().any(|s| s == "*");
        for (name, paths) in &recipes {
            if all || stale.contains(name) {
                for p in paths {
                    let f = std::fs::File::options()
                        .write(true)
                        .open(p)
                        .map_err(|source| ApplyError::OpenRecipe {
                            path: p.clone(),
                            source,
                        })?;
                    f.set_modified(now)
                        .map_err(|source| ApplyError::TouchRecipe {
                            path: p.clone(),
                            source,
                        })?;
                    touched += 1;
                }
            }
        }
    }

    // Generated output is not proof that recipes were marked for rebuild. Keep
    // the applied baseline intact when a caller deliberately skips that step.
    if touch {
        let mut state = String::from(
            "# ferric-config bookkeeping: option fingerprints per recipe.\n[recipes]\n",
        );
        for (k, v) in &plan.fingerprints {
            state.push_str(&format!("{} = \"{v}\"\n", toml_key(k)));
        }
        std::fs::write(&state_path, state).map_err(|source| ApplyError::WriteState {
            path: state_path,
            source,
        })?;
    }

    Ok(Applied {
        written,
        stale,
        touched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::resolve;
    use crate::schema::tests::real_schema;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn file<'a>(plan: &'a Plan, name: &str) -> &'a str {
        &plan
            .files
            .iter()
            .find(|(p, _)| p.ends_with(name))
            .unwrap()
            .1
    }

    #[test]
    fn experimental_rtl8125_is_opt_in() {
        let schema = real_schema();
        for enabled in [false, true] {
            let user = BTreeMap::from([("drivers.rtl8125".to_owned(), Value::Bool(enabled))]);
            let resolved = resolve(&schema, &user);
            let output = plan(&schema, &resolved, &root(), Path::new("build/ferric")).unwrap();
            let mk = file(&output, "config.mk");
            assert_eq!(
                mk.contains("export FERRIC_BASE_DRIVERS:=rtl8125d\n"),
                enabled,
                "{mk}"
            );
        }
    }

    #[test]
    fn default_plan() {
        let s = real_schema();
        let r = resolve(&s, &BTreeMap::new());
        let plan = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let mk = file(&plan, "config.mk");
        assert!(mk.contains("ARCH?=x86_64\n"), "{mk}");
        assert!(mk.contains("CONFIG_NAME?=desktop\n"), "{mk}");
        assert!(mk.contains("PODMAN_BUILD?=0\n"), "{mk}");
        assert!(mk.contains("PREFIX_TOOLCHAIN?=llvm\n"), "{mk}");
        assert!(mk.contains("FSTOOLS_NO_MOUNT?=1\n"), "{mk}");
        assert!(mk.contains("REPO_BINARY?=\n"), "{mk}");
        assert!(
            mk.contains("export FERRIC_TARGET_RUSTFLAGS:=-C target-cpu=x86-64-v3\n"),
            "{mk}"
        );
        assert!(
            mk.contains("export FERRIC_TARGET_CPPFLAGS:=-march=x86-64-v3\n"),
            "{mk}"
        );
        assert!(mk.contains("QEMU_CPU?=max\n"), "{mk}");
        // The level also picks fox's package variants.
        assert!(mk.contains("export FERRIC_FOX_LEVEL:=v3\n"), "{mk}");
        assert!(
            mk.contains("export FERRIC_TARGET_FLAGS_EXCLUDE:=kernel bootloader\n"),
            "{mk}"
        );
        assert!(
            mk.contains("export FERRIC_KERNEL_FEATURES:=acpi,aslr,kaslr,multi_core,prot_none,self_modifying,serial_debug,x86_kvm_pv\n"),
            "{mk}"
        );
        let fs = file(&plan, "filesystem.toml");
        assert!(
            fs.contains("include = [\"../../config/desktop.toml\"]"),
            "{fs}"
        );
        let kc = file(&plan, "kernel-config.toml");
        assert!(kc.contains("[arch.x86_64.features]"), "{kc}");
        assert!(kc.contains("xsave = \"always\""), "{kc}");
        assert!(kc.contains("smap = \"auto\""), "{kc}");
    }

    #[test]
    fn package_manager_choice() {
        let s = real_schema();
        let r = resolve(&s, &BTreeMap::new());
        let plan_default = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let fs = file(&plan_default, "filesystem.toml");
        // fox exists only here, so it builds from source even with REPO_BINARY.
        assert!(fs.contains("fox = \"source\"\n"), "{fs}");
        assert!(fs.contains("pkgutils = \"ignore\"\n"), "{fs}");
        let mk = file(&plan_default, "config.mk");
        assert!(mk.contains("export FERRIC_FOX_MANIFEST:=1\n"), "{mk}");
        // fox finishes the image root; no package set is synced by default.
        assert!(mk.contains("FERRIC_FOX_IMAGE:=1\n"), "{mk}");
        assert!(!mk.contains("FERRIC_FOX_LOCKED"), "{mk}");

        let user: BTreeMap<String, Value> = [
            ("packages.set", Value::Str("config/packages.toml".into())),
            ("packages.locked", Value::Bool(true)),
            ("cpu.x86_64_level", Value::Str("v4".into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let r = resolve(&s, &user);
        let plan_set = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let mk = file(&plan_set, "config.mk");
        assert!(
            mk.contains("FERRIC_FOX_PACKAGES:=config/packages.toml\n"),
            "{mk}"
        );
        assert!(mk.contains("FERRIC_FOX_LOCKED:=1\n"), "{mk}");
        assert!(mk.contains("FERRIC_FOX_LEVEL:=v4\n"), "{mk}");

        let user: BTreeMap<String, Value> = [
            ("packages.manager", Value::Str("pkgutils".into())),
            ("packages.signed_manifests", Value::Bool(false)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let r = resolve(&s, &user);
        let plan_pkg = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let fs = file(&plan_pkg, "filesystem.toml");
        assert!(!fs.contains("fox"), "{fs}");
        assert!(!fs.contains("pkgutils"), "{fs}");
        let mk = file(&plan_pkg, "config.mk");
        assert!(mk.contains("export FERRIC_FOX_MANIFEST:=0\n"), "{mk}");
        // pkgutils images are not finished with fox.
        assert!(!mk.contains("FERRIC_FOX_IMAGE"), "{mk}");

        // Asking for fox by hand as well still writes it once, from source.
        let user: BTreeMap<String, Value> = [(
            "image.packages_add".to_owned(),
            Value::List(vec!["fox".into()]),
        )]
        .into_iter()
        .collect();
        let r = resolve(&s, &user);
        let plan_twice = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let fs = file(&plan_twice, "filesystem.toml");
        assert_eq!(fs.matches("fox =").count(), 1, "{fs}");
        assert!(fs.contains("fox = \"source\"\n"), "{fs}");
    }

    #[test]
    fn image_changes_and_overrides() {
        let s = real_schema();
        let user: BTreeMap<String, Value> = [
            ("image.config", Value::Str("demo".into())),
            ("image.filesystem_size", Value::Int(4096)),
            ("image.packages_add", Value::List(vec!["vim".into()])),
            ("image.packages_remove", Value::List(vec!["servo".into()])),
            ("qemu.cpu_model", Value::Str("Haswell".into())),
            (
                "cpu.extra_rustflags",
                Value::Str("-C target-feature=+aes".into()),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let r = resolve(&s, &user);
        let plan = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let fs = file(&plan, "filesystem.toml");
        assert!(
            fs.contains("include = [\"../../config/x86_64/demo.toml\"]"),
            "{fs}"
        );
        assert!(fs.contains("[general]\nfilesystem_size = 4096\n"), "{fs}");
        assert!(fs.contains("vim = {}\n"), "{fs}");
        assert!(fs.contains("servo = \"ignore\"\n"), "{fs}");
        let mk = file(&plan, "config.mk");
        assert!(mk.contains("QEMU_CPU?=Haswell\n"), "{mk}");
        assert!(!mk.contains("QEMU_CPU?=max"), "{mk}");
        assert!(
            mk.contains(
                "FERRIC_TARGET_RUSTFLAGS:=-C target-cpu=x86-64-v3 -C target-feature=+aes\n"
            ),
            "{mk}"
        );
    }

    #[test]
    fn frame_pointers_reach_every_target_package() {
        let s = real_schema();
        let user: BTreeMap<String, Value> = [("cpu.frame_pointers".to_owned(), Value::Bool(true))]
            .into_iter()
            .collect();
        let r = resolve(&s, &user);
        let plan = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let mk = file(&plan, "config.mk");
        assert!(
            mk.contains(
                "FERRIC_TARGET_RUSTFLAGS:=-C target-cpu=x86-64-v3 -C force-frame-pointers=yes\n"
            ),
            "{mk}"
        );
        assert!(
            mk.contains("FERRIC_TARGET_CPPFLAGS:=-march=x86-64-v3 -fno-omit-frame-pointer\n"),
            "{mk}"
        );
        // Off by default, config.mk adds neither flag. That is all this checks: Rust packages
        // still get frame pointers on 64-bit targets from redoxer, so off only means C and C++
        // code omits them.
        let r = resolve(&s, &BTreeMap::new());
        let off = super::plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        assert!(!file(&off, "config.mk").contains("frame-pointer"));
    }

    #[test]
    fn kernel_features_without_defaults() {
        let s = real_schema();
        let user: BTreeMap<String, Value> = [
            ("kernel.multi_core", Value::Bool(false)),
            ("kernel.acpi", Value::Bool(false)),
            ("kernel.debugger", Value::Bool(true)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let r = resolve(&s, &user);
        let plan = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let mk = file(&plan, "config.mk");
        assert!(
            mk.contains("export FERRIC_KERNEL_FEATURES:=aslr,debugger,kaslr,prot_none,self_modifying,serial_debug,syscall_debug,x86_kvm_pv\n"),
            "{mk}"
        );
    }

    #[test]
    fn aarch64_has_no_kernel_config() {
        let s = real_schema();
        let user: BTreeMap<String, Value> = [
            ("target.arch", Value::Str("aarch64".into())),
            ("cpu.aarch64_level", Value::Str("armv8.2-a".into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let r = resolve(&s, &user);
        let plan = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        assert!(
            plan.remove
                .iter()
                .any(|p| p.ends_with("kernel-config.toml"))
        );
        let mk = file(&plan, "config.mk");
        assert!(
            mk.contains("FERRIC_TARGET_CPPFLAGS:=-march=armv8.2-a"),
            "{mk}"
        );
        assert!(!mk.contains("QEMU_CPU"), "{mk}");
        assert!(!mk.contains("x86_kvm_pv"), "{mk}");
    }

    fn trace_user(enable: &[&str], disable: &[&str], trace: bool) -> BTreeMap<String, Value> {
        let list = |items: &[&str]| Value::List(items.iter().map(|s| (*s).to_owned()).collect());
        [
            ("kernel.trace", Value::Bool(trace)),
            ("kernel.trace_initfs_enable", list(enable)),
            ("kernel.trace_initfs_disable", list(disable)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }

    #[test]
    fn initfs_trace_toml() {
        let s = real_schema();
        let user = trace_user(&["syscall", "scheme.request"], &["syscall.exit"], true);
        let r = resolve(&s, &user);
        let plan = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        let text = file(&plan, "trace.toml");
        assert!(text.starts_with("# Generated by ferric-config"), "{text}");
        let config = ferric_trace::config::BootConfig::parse(text.as_bytes()).unwrap();
        assert_eq!(config.enable, ["syscall", "scheme.request"]);
        assert_eq!(config.disable, ["syscall.exit"]);
        let mk = file(&plan, "config.mk");
        assert!(
            mk.contains("export FERRIC_INITFS_TRACE:=$(CURDIR)/build/ferric/trace.toml\n"),
            "{mk}"
        );
        // Only for the configured target, like the kernel settings.
        let guard = mk.find("ifeq (").unwrap();
        assert!(mk.find("FERRIC_INITFS_TRACE").unwrap() > guard, "{mk}");
        assert!(!plan.remove.iter().any(|p| p.ends_with("trace.toml")));
    }

    #[test]
    fn absolute_output_paths() {
        let s = real_schema();
        let r = resolve(&s, &trace_user(&["sched"], &[], true));
        let plan = plan(&s, &r, &root(), Path::new("/tmp/ferric-out")).unwrap();
        let mk = file(&plan, "config.mk");
        assert!(
            mk.contains("export FERRIC_INITFS_TRACE:=/tmp/ferric-out/trace.toml\n"),
            "{mk}"
        );
        assert!(
            mk.contains("export FERRIC_KERNEL_CONFIG:=/tmp/ferric-out/kernel-config.toml\n"),
            "{mk}"
        );
    }

    #[test]
    fn no_initfs_trace_toml_by_default() {
        let s = real_schema();
        for user in [
            BTreeMap::new(),
            // The options need kernel.trace.
            trace_user(&["syscall"], &[], false),
            trace_user(&[], &[], true),
        ] {
            let r = resolve(&s, &user);
            let plan = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
            assert!(!plan.files.iter().any(|(p, _)| p.ends_with("trace.toml")));
            assert!(plan.remove.iter().any(|p| p.ends_with("trace.toml")));
            assert!(!file(&plan, "config.mk").contains("FERRIC_INITFS_TRACE"));
        }
    }

    #[test]
    fn initfs_trace_names_are_checked() {
        let s = real_schema();
        let r = resolve(&s, &trace_user(&["sched.swich"], &[], true));
        let errors = match plan(&s, &r, &root(), Path::new("build/ferric")) {
            Err(errors) => errors,
            Ok(_) => panic!("a misspelt event must not generate"),
        };
        let text = format!("{errors:?}");
        assert!(text.contains("sched.swich"), "{text}");
    }

    /// The errors of a plan that must not generate.
    fn plan_errors(user: &BTreeMap<String, Value>) -> String {
        let s = real_schema();
        let r = resolve(&s, user);
        match plan(&s, &r, &root(), Path::new("build/ferric")) {
            Err(errors) => format!("{errors:?}"),
            Ok(_) => panic!("{user:?} must not generate"),
        }
    }

    #[test]
    fn initfs_trace_needs_compiled_events() {
        // scheme.cancel is not in a kernel without the scheme tracepoints.
        let mut user = trace_user(&["scheme.cancel"], &[], true);
        user.insert("kernel.trace_scheme".into(), Value::Bool(false));
        let text = plan_errors(&user);
        assert!(
            text.contains("scheme.cancel") && text.contains("compiled"),
            "{text}"
        );
        // Nor are info events with only errors compiled in.
        let mut user = trace_user(&[], &["sched.switch"], true);
        user.insert("kernel.trace_max_level".into(), Value::Str("error".into()));
        assert!(plan_errors(&user).contains("sched.switch"));
        // A selection with a compiled event is kept whole.
        let mut user = trace_user(&["scheme.cancel sched.switch"], &[], true);
        user.insert("kernel.trace_scheme".into(), Value::Bool(false));
        let s = real_schema();
        let r = resolve(&s, &user);
        let generated = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        assert!(file(&generated, "trace.toml").contains("\"scheme.cancel sched.switch\""));
    }

    #[test]
    fn initfs_trace_sizes_are_checked() {
        // One command over the kernel's limit.
        let long = "syscall ".repeat(ferric_trace::abi::MAX_COMMAND_BYTES / 8);
        let text = plan_errors(&trace_user(&[&long], &[], true));
        assert!(text.contains("bytes"), "{text}");
        // Commands that each fit, in a file over bootstrap's limit.
        let fits = "syscall ".repeat(400);
        let many = vec![fits.as_str(); ferric_trace::config::MAX_FILE_BYTES / fits.len() + 1];
        let text = plan_errors(&trace_user(&many, &[], true));
        assert!(text.contains("bootstrap"), "{text}");
    }

    #[test]
    fn trace_kernel_features_match_the_catalog() {
        // ferric_trace's names for the kernel's trace features, used to check selections
        // against the configured kernel, are the features the options turn on.
        let s = real_schema();
        let r = resolve(&s, &trace_user(&[], &[], true));
        let out = collect(&s, &r).unwrap();
        for subsystem in ferric_trace::Subsystem::ALL {
            assert!(
                out.kernel_features.contains(subsystem.kernel_feature()),
                "{subsystem}: {:?}",
                out.kernel_features
            );
        }
        for level in ferric_trace::Level::ALL {
            let Some(feature) = level.kernel_feature() else {
                continue;
            };
            let mut user = trace_user(&[], &[], true);
            user.insert(
                "kernel.trace_max_level".into(),
                Value::Str(level.name().into()),
            );
            let out = collect(&s, &resolve(&s, &user)).unwrap();
            assert!(
                out.kernel_features.contains(feature),
                "{level}: {:?}",
                out.kernel_features
            );
        }
    }

    #[test]
    fn trace_crash_options() {
        let s = real_schema();
        let mut user = trace_user(&[], &[], true);
        user.insert("kernel.trace_crash".into(), Value::Bool(true));
        user.insert("kernel.trace_crash_test".into(), Value::Bool(true));
        let out = collect(&s, &resolve(&s, &user)).unwrap();
        assert!(out.kernel_features.contains("trace_crash"));
        assert!(out.kernel_features.contains("trace_crash_test"));
        assert_eq!(out.kernel_config["trace.crash_base"], "0x4000000");
        // Both need kernel.trace.
        let mut user = trace_user(&[], &[], false);
        user.insert("kernel.trace_crash".into(), Value::Bool(true));
        let out = collect(&s, &resolve(&s, &user)).unwrap();
        assert!(!out.kernel_features.iter().any(|f| f.starts_with("trace")));
    }

    #[test]
    fn log_persist_options() {
        let s = real_schema();
        // Off by default, with no region address.
        let out = collect(&s, &resolve(&s, &BTreeMap::new())).unwrap();
        assert!(!out.kernel_features.contains("log_persist"));
        assert!(!out.kernel_config.contains_key("log.persist_base"));
        // On without tracing, its region right after the crash region's default.
        let mut user = trace_user(&[], &[], false);
        user.insert("kernel.log_persist".into(), Value::Bool(true));
        let out = collect(&s, &resolve(&s, &user)).unwrap();
        assert!(out.kernel_features.contains("log_persist"));
        assert_eq!(out.kernel_config["log.persist_base"], "0x4010000");
        user.insert(
            "kernel.log_persist_base".into(),
            Value::Str("0x8000000".into()),
        );
        let out = collect(&s, &resolve(&s, &user)).unwrap();
        assert_eq!(out.kernel_config["log.persist_base"], "0x8000000");
    }

    #[test]
    fn absolute_output_finds_the_base_config() {
        let s = real_schema();
        let r = resolve(&s, &BTreeMap::new());
        let out_dir = Path::new("/tmp/ferric-out");
        let generated = plan(&s, &r, &root(), out_dir).unwrap();
        let fs: toml::Table = toml::from_str(file(&generated, "filesystem.toml")).unwrap();
        let include = fs["include"].as_array().unwrap()[0].as_str().unwrap();
        // As the installer resolves it, from the generated file's directory.
        let resolved = out_dir.join(include);
        assert!(resolved.is_file(), "{}", resolved.display());
        let base = base_filesystem_config(&root(), "x86_64", "", "desktop");
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(root().join(base)).unwrap()
        );
        // Relative output directories keep their relative include.
        let generated = plan(&s, &r, &root(), Path::new("build/ferric")).unwrap();
        assert!(file(&generated, "filesystem.toml").contains("include = [\"../../config/"));
    }

    #[test]
    fn missing_base_config_is_an_error() {
        let s = real_schema();
        let user: BTreeMap<String, Value> = [
            ("target.arch", Value::Str("riscv64gc".into())),
            ("image.config", Value::Str("full".into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let r = resolve(&s, &user);
        assert!(plan(&s, &r, &root(), Path::new("build/ferric")).is_err());
    }

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative(
                Path::new("build/ferric"),
                Path::new("config/x86_64/demo.toml")
            ),
            PathBuf::from("../../config/x86_64/demo.toml")
        );
        assert_eq!(
            relative(Path::new("config"), Path::new("config/desktop.toml")),
            PathBuf::from("desktop.toml")
        );
    }

    fn rebuild_fixture(scope: &str, fingerprint: &str) -> Plan {
        Plan {
            files: vec![(PathBuf::from("build/ferric/config.mk"), fingerprint.into())],
            remove: Vec::new(),
            fingerprints: BTreeMap::from([(scope.into(), fingerprint.into())]),
        }
    }

    fn fixture_recipes(root: &Path) -> Vec<PathBuf> {
        ["kernel", "app"]
            .into_iter()
            .map(|name| {
                let path = root.join("recipes/core").join(name).join("recipe.toml");
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "[build]\n").unwrap();
                path
            })
            .collect()
    }

    #[test]
    fn no_touch_preserves_pending_recipe_and_global_rebuilds() {
        for scope in ["kernel", "*"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let recipes = fixture_recipes(root);
            let out = Path::new("build/ferric");
            apply(root, out, &rebuild_fixture(scope, "old"), true).unwrap();
            let state_path = root.join(out).join("state.toml");
            let old_state = std::fs::read(&state_path).unwrap();
            let timestamp = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(60);
            for recipe in &recipes {
                std::fs::File::options()
                    .write(true)
                    .open(recipe)
                    .unwrap()
                    .set_modified(timestamp)
                    .unwrap();
            }

            let changed = rebuild_fixture(scope, "new");
            for _ in 0..2 {
                let preview = apply(root, out, &changed, false).unwrap();
                assert_eq!(preview.touched, 0);
                assert_eq!(preview.stale, [scope]);
                assert_eq!(std::fs::read(&state_path).unwrap(), old_state);
                assert_eq!(
                    std::fs::read_to_string(root.join(out).join("config.mk")).unwrap(),
                    "new"
                );
                for recipe in &recipes {
                    assert_eq!(recipe.metadata().unwrap().modified().unwrap(), timestamp);
                }
            }

            let generated = apply(root, out, &changed, true).unwrap();
            assert_eq!(generated.stale, [scope]);
            assert_eq!(generated.touched, if scope == "*" { 2 } else { 1 });
            assert!(recipes[0].metadata().unwrap().modified().unwrap() > timestamp);
            if scope == "kernel" {
                assert_eq!(
                    recipes[1].metadata().unwrap().modified().unwrap(),
                    timestamp
                );
            }
            assert_ne!(std::fs::read(&state_path).unwrap(), old_state);
            let repeated = apply(root, out, &changed, true).unwrap();
            assert_eq!(repeated.touched, 0);
            assert!(repeated.stale.is_empty());
        }
    }

    #[test]
    fn no_touch_first_generation_leaves_rebuild_state_uncommitted() {
        let dir = tempfile::tempdir().unwrap();
        fixture_recipes(dir.path());
        let out = Path::new("build/ferric");
        let plan = rebuild_fixture("kernel", "first");
        let preview = apply(dir.path(), out, &plan, false).unwrap();
        assert_eq!(preview.touched, 0);
        assert!(!dir.path().join(out).join("state.toml").exists());
        assert!(dir.path().join(out).join("config.mk").is_file());

        let generated = apply(dir.path(), out, &plan, true).unwrap();
        assert_eq!(generated.stale, ["kernel"]);
        assert_eq!(generated.touched, 1);
        assert!(dir.path().join(out).join("state.toml").is_file());
    }

    #[test]
    fn output_failure_preserves_rebuild_state_and_error_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fixture_recipes(root);
        let out = Path::new("build/ferric");
        apply(root, out, &rebuild_fixture("kernel", "old"), true).unwrap();
        let state = root.join(out).join("state.toml");
        let before = std::fs::read(&state).unwrap();
        let output = root.join(out).join("config.mk");
        std::fs::remove_file(&output).unwrap();
        std::fs::create_dir(&output).unwrap();

        let error = apply(root, out, &rebuild_fixture("kernel", "new"), true)
            .err()
            .unwrap();
        assert!(matches!(&error, ApplyError::WriteOutput { path, .. } if path == &output));
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .is_some()
        );
        assert_eq!(std::fs::read(&state).unwrap(), before);
    }

    #[test]
    #[cfg(unix)]
    fn failed_recipe_touch_preserves_pending_rebuild_for_retry() {
        for scope in ["kernel", "*"] {
            for first_generation in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();
                let recipes = fixture_recipes(root);
                let out = Path::new("build/ferric");
                if !first_generation {
                    apply(root, out, &rebuild_fixture(scope, "old"), true).unwrap();
                }
                let state = root.join(out).join("state.toml");
                let before = std::fs::read(&state).ok();
                let timestamp = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(60);
                std::fs::File::options()
                    .write(true)
                    .open(&recipes[1])
                    .unwrap()
                    .set_modified(timestamp)
                    .unwrap();

                // A dangling link fails even when tests run with elevated privileges.
                std::fs::remove_file(&recipes[0]).unwrap();
                std::os::unix::fs::symlink("missing.toml", &recipes[0]).unwrap();
                let plan = rebuild_fixture(scope, "new");
                let error = apply(root, out, &plan, true).err().unwrap();
                assert!(
                    matches!(&error, ApplyError::OpenRecipe { path, .. } if path == &recipes[0])
                );
                assert!(
                    std::error::Error::source(&error)
                        .unwrap()
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|source| source.kind() == std::io::ErrorKind::NotFound)
                );
                assert!(
                    error
                        .to_string()
                        .contains(&recipes[0].display().to_string())
                );
                assert_eq!(std::fs::read(&state).ok(), before);
                if scope == "*" {
                    // A partial global rebuild must not consume the baseline either.
                    assert!(recipes[1].metadata().unwrap().modified().unwrap() > timestamp);
                }

                let preview = apply(root, out, &plan, false).unwrap();
                assert_eq!(preview.touched, 0);
                assert_eq!(preview.stale, [scope]);
                assert_eq!(std::fs::read(&state).ok(), before);

                std::fs::remove_file(&recipes[0]).unwrap();
                std::fs::write(&recipes[0], "[build]\n").unwrap();
                let retried = apply(root, out, &plan, true).unwrap();
                assert_eq!(retried.stale, [scope]);
                assert_eq!(retried.touched, if scope == "*" { 2 } else { 1 });
                assert_ne!(std::fs::read(&state).ok(), before);
                assert!(apply(root, out, &plan, true).unwrap().stale.is_empty());
            }
        }
    }
}
