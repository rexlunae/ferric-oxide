//! ferric.toml (the user's choices) and resolution of every option's
//! effective value from choices, defaults, dependencies and selects.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table};

use crate::schema::{Entry, Kind, Level, Schema};
use crate::value::Value;

/// Expression identifier for the architecture the build runs on.
///
/// It is not a catalog option: `features.toml` expressions may read it, and it
/// resolves to the same names `target.arch` uses (`x86_64`, `aarch64`, ...).
pub const HOST_ARCH: &str = "host.arch";

/// The architecture the build runs on, in `target.arch` terms.
///
/// Only the architectures the catalog offers as targets are named, so an
/// expression like `host.arch == target.arch` means "native build" and never
/// accidentally matches a host Ferric cannot build for.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    pub level: Level,
    pub message: String,
}

impl Diag {
    pub fn warn(message: impl Into<String>) -> Diag {
        Diag {
            level: Level::Warn,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Diag {
        Diag {
            level: Level::Error,
            message: message.into(),
        }
    }
}

/// The user's explicit choices, with the document kept so comments and
/// layout survive edits.
pub struct UserConfig {
    pub path: PathBuf,
    doc: DocumentMut,
    pub values: BTreeMap<String, Value>,
}

/// A failure to read or parse the saved configuration.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// Reading an existing configuration failed.
    #[error("cannot read {}", path.display())]
    Read {
        /// Configuration path.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The configuration is not valid TOML.
    #[error("cannot parse {}", path.display())]
    Parse {
        /// Configuration path.
        path: PathBuf,
        /// Underlying TOML error, including its source location.
        #[source]
        source: toml_edit::TomlError,
    },
}

fn flatten(table: &dyn toml_edit::TableLike, prefix: &str, out: &mut Vec<(String, Item)>) {
    for (k, item) in table.iter() {
        let key = if prefix.is_empty() {
            k.to_owned()
        } else {
            format!("{prefix}.{k}")
        };
        match item.as_table_like() {
            Some(t) => flatten(t, &key, out),
            None => out.push((key, item.clone())),
        }
    }
}

impl UserConfig {
    /// Loads ferric.toml. A missing file is an empty configuration. Entries
    /// that do not match the schema are reported and ignored.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::Read`] for filesystem failures other than a missing
    /// file, or [`LoadError::Parse`] for invalid TOML, retaining the source error.
    pub fn load(path: &Path, schema: &Schema) -> Result<(UserConfig, Vec<Diag>), LoadError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(LoadError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let doc: DocumentMut = text.parse().map_err(|source| LoadError::Parse {
            path: path.to_owned(),
            source,
        })?;
        let mut entries = Vec::new();
        flatten(doc.as_table(), "", &mut entries);
        let mut values = BTreeMap::new();
        let mut diags = Vec::new();
        for (key, item) in entries {
            let Some(opt) = schema.get(&key) else {
                let hint = schema
                    .suggest(&key)
                    .map(|s| format!(" (did you mean {s}?)"))
                    .unwrap_or_default();
                diags.push(Diag::error(format!(
                    "{}: unknown option {key}{hint}",
                    path.display()
                )));
                continue;
            };
            let parsed = edit_to_value(&item);
            match parsed {
                Some(v) => match opt.check(&v) {
                    Ok(()) => {
                        values.insert(key, v);
                    }
                    Err(e) => diags.push(Diag::error(format!("{}: {e}", path.display()))),
                },
                None => diags.push(Diag::error(format!(
                    "{}: {key} has an unsupported value",
                    path.display()
                ))),
            }
        }
        Ok((
            UserConfig {
                path: path.to_owned(),
                doc,
                values,
            },
            diags,
        ))
    }

    /// Whether a leaf option is explicitly saved, including unknown or invalid
    /// values omitted from the parsed [`Self::values`] map. Tables group
    /// options and are not leaf options themselves.
    pub fn contains_option(&self, id: &str) -> bool {
        id.split('.')
            .try_fold(self.doc.as_item(), |item, key| item.get(key))
            .is_some_and(|item| !item.is_none() && item.as_table_like().is_none())
    }

    pub fn set(&mut self, id: &str, v: Value) -> Result<(), String> {
        let parts: Vec<&str> = id.split('.').collect();
        let (leaf, parents) = parts.split_last().ok_or("empty option id")?;
        let mut table: &mut Table = self.doc.as_table_mut();
        for part in parents {
            if !table.contains_key(part) {
                let mut t = Table::new();
                t.set_implicit(true);
                table.insert(part, Item::Table(t));
            }
            let item = table.get_mut(part).unwrap();
            if let Some(inline) = item.as_inline_table().cloned() {
                *item = Item::Table(inline.into_table());
            }
            table = item
                .as_table_mut()
                .ok_or_else(|| format!("{part} in {} is not a table", self.path.display()))?;
        }
        table.insert(leaf, Item::Value(v.to_edit()));
        self.values.insert(id.to_owned(), v);
        Ok(())
    }

    pub fn unset(&mut self, id: &str) {
        fn remove(table: &mut Table, parts: &[&str]) {
            match parts {
                [] => {}
                [leaf] => {
                    table.remove(leaf);
                }
                [head, rest @ ..] => {
                    if let Some(item) = table.get_mut(head) {
                        if let Some(inline) = item.as_inline_table().cloned() {
                            *item = Item::Table(inline.into_table());
                        }
                        if let Some(t) = item.as_table_mut() {
                            remove(t, rest);
                            if t.is_empty() {
                                table.remove(head);
                            }
                        }
                    }
                }
            }
        }
        let parts: Vec<&str> = id.split('.').collect();
        remove(self.doc.as_table_mut(), &parts);
        self.values.remove(id);
    }

    pub fn render(&self) -> String {
        let mut s = self.doc.to_string();
        if self.values.is_empty() && s.trim().is_empty() {
            s = String::new();
        }
        s
    }

    pub fn save(&self) -> Result<(), String> {
        let mut text = self.render();
        if text.trim().is_empty() || !self.path.exists() {
            text = format!("{HEADER}{text}");
        }
        std::fs::write(&self.path, text)
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }
}

const HEADER: &str = "\
# Ferric Oxide build configuration (see features.toml for every option).
# Edit with `make menuconfig`, `ferric-config set <option> <value>`, or by hand.
# Options not listed here use their defaults.

";

fn edit_to_value(item: &Item) -> Option<Value> {
    let v = item.as_value()?;
    Some(match v {
        toml_edit::Value::Boolean(b) => Value::Bool(*b.value()),
        toml_edit::Value::Integer(n) => Value::Int(*n.value()),
        toml_edit::Value::String(s) => Value::Str(s.value().clone()),
        toml_edit::Value::Array(a) => Value::List(
            a.iter()
                .map(|i| i.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?,
        ),
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    User,
    Default,
    /// Forced on by another option's `selects`.
    Selected(String),
    /// Dependencies unmet; the option has no effect.
    Inactive,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub values: Vec<Value>,
    pub sources: Vec<Source>,
    pub active: Vec<bool>,
    pub diags: Vec<Diag>,
}

impl Resolved {
    pub fn value(&self, schema: &Schema, id: &str) -> Option<&Value> {
        schema.position(id).map(|i| &self.values[i])
    }

    /// Lookup used by expressions: an inactive bool reads as false.
    pub fn lookup(&self, schema: &Schema, id: &str) -> Option<Value> {
        // `host.arch` is the architecture the build runs on, not an option:
        // expressions compare it with `target.arch` to tell a native build from
        // a cross-compiled one. It has no stored value, so it is answered here
        // rather than in the option table.
        if id == HOST_ARCH {
            return Some(Value::Str(host_arch().to_owned()));
        }
        let i = schema.position(id)?;
        if !self.active[i] && schema.options[i].kind == Kind::Bool {
            return Some(Value::Bool(false));
        }
        Some(self.values[i].clone())
    }

    pub fn menu_active(&self, schema: &Schema, menu: &str) -> bool {
        let mut cur = schema.menu(menu);
        while let Some(m) = cur {
            if let Some((e, _)) = &m.depends
                && !e.eval(&|id| self.lookup(schema, id)).unwrap_or(false)
            {
                return false;
            }
            cur = m.parent.as_deref().and_then(|p| schema.menu(p));
        }
        true
    }

    pub fn entry_active(&self, schema: &Schema, e: &Entry) -> bool {
        match e {
            Entry::Menu(m) => self.menu_active(schema, m),
            Entry::Option(i) => self.active[*i],
        }
    }

    pub fn errors(&self) -> usize {
        self.diags
            .iter()
            .filter(|d| d.level == Level::Error)
            .count()
    }
}

/// Computes every option's value. Defaults and dependencies can refer to
/// other options, so this iterates to a fixed point.
pub fn resolve(schema: &Schema, user: &BTreeMap<String, Value>) -> Resolved {
    let n = schema.options.len();
    let mut r = Resolved {
        values: schema.options.iter().map(|o| o.default.clone()).collect(),
        sources: vec![Source::Default; n],
        active: vec![true; n],
        diags: Vec::new(),
    };
    let mut eval_errors: BTreeMap<String, String> = BTreeMap::new();
    let mut settled = false;
    for _ in 0..64 {
        let prev = r.clone();
        let look = |id: &str| prev.lookup(schema, id);
        for (i, o) in schema.options.iter().enumerate() {
            let mut active = prev.menu_active(schema, &o.menu);
            if let Some((e, src)) = &o.depends {
                active &= e.eval(&look).unwrap_or_else(|err| {
                    eval_errors.insert(o.id.clone(), format!("depends `{src}`: {err}"));
                    false
                });
            }
            let mut default = o.default.clone();
            for (e, v, src) in &o.defaults {
                match e.eval(&look) {
                    Ok(true) => {
                        default = v.clone();
                        break;
                    }
                    Ok(false) => {}
                    Err(err) => {
                        eval_errors.insert(o.id.clone(), format!("default when `{src}`: {err}"));
                    }
                }
            }
            r.active[i] = active;
            match user.get(&o.id) {
                Some(v) if active => {
                    r.values[i] = v.clone();
                    r.sources[i] = Source::User;
                }
                _ => {
                    r.values[i] = default;
                    r.sources[i] = if active {
                        Source::Default
                    } else {
                        Source::Inactive
                    };
                }
            }
        }
        for (i, o) in schema.options.iter().enumerate() {
            if !(r.active[i] && r.values[i] == Value::Bool(true)) {
                continue;
            }
            for t in &o.selects {
                let ti = schema.position(t).unwrap();
                if r.active[ti] {
                    r.values[ti] = Value::Bool(true);
                    r.sources[ti] = Source::Selected(o.id.clone());
                }
            }
        }
        if r.values == prev.values && r.active == prev.active {
            settled = true;
            break;
        }
    }
    if !settled {
        r.diags.push(Diag::error(
            "option defaults or dependencies do not settle (circular conditions in features.toml)",
        ));
    }
    for (id, err) in eval_errors {
        r.diags.push(Diag::error(format!("{id}: {err}")));
    }
    for (i, o) in schema.options.iter().enumerate() {
        if user.contains_key(&o.id) && !r.active[i] {
            r.diags.push(Diag::warn(format!(
                "{} is set but has no effect: needs {}",
                o.id,
                o.depends
                    .as_ref()
                    .map(|(_, s)| s.clone())
                    .unwrap_or_else(|| format!("menu {}", o.menu))
            )));
        }
        if let Source::Selected(by) = &r.sources[i]
            && user.get(&o.id) == Some(&Value::Bool(false))
        {
            r.diags.push(Diag::warn(format!(
                "{} is off in ferric.toml but {by} turns it on",
                o.id
            )));
        }
        if o.experimental && r.active[i] && r.values[i].truthy() && o.kind == Kind::Bool {
            r.diags
                .push(Diag::warn(format!("{} is experimental", o.id)));
        }
        for t in &o.selects {
            let ti = schema.position(t).unwrap();
            if r.active[i] && r.values[i] == Value::Bool(true) && !r.active[ti] {
                r.diags.push(Diag::warn(format!(
                    "{} selects {t}, whose dependencies are unmet",
                    o.id
                )));
            }
        }
    }
    for rule in &schema.rules {
        match rule.when.eval(&|id| r.lookup(schema, id)) {
            Ok(true) => r.diags.push(Diag {
                level: rule.level,
                message: rule.message.clone(),
            }),
            Ok(false) => {}
            Err(e) => r
                .diags
                .push(Diag::error(format!("rule `{}`: {e}", rule.when_src))),
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests::real_schema;

    fn user(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn defaults_follow_cpu_level() {
        let s = real_schema();
        let r = resolve(&s, &BTreeMap::new());
        assert_eq!(
            r.value(&s, "cpu.x86_64_level"),
            Some(&Value::Str("v3".into()))
        );
        assert_eq!(
            r.value(&s, "kernel.cpu.xsave"),
            Some(&Value::Str("always".into()))
        );
        assert_eq!(
            r.value(&s, "kernel.cpu.smap"),
            Some(&Value::Str("auto".into()))
        );

        let r = resolve(&s, &user(&[("cpu.x86_64_level", Value::Str("v1".into()))]));
        assert_eq!(
            r.value(&s, "kernel.cpu.xsave"),
            Some(&Value::Str("auto".into()))
        );
    }

    #[test]
    fn depends_and_selects() {
        let s = real_schema();
        let r = resolve(
            &s,
            &user(&[
                ("kernel.multi_core", Value::Bool(false)),
                ("kernel.numa", Value::Bool(true)),
                ("kernel.acpi", Value::Bool(false)),
            ]),
        );
        let numa = s.position("kernel.numa").unwrap();
        assert!(!r.active[numa]);
        assert!(
            r.diags
                .iter()
                .any(|d| d.message.contains("kernel.numa is set"))
        );
        assert_eq!(r.value(&s, "kernel.acpi"), Some(&Value::Bool(false)));

        let r = resolve(&s, &user(&[("kernel.acpi", Value::Bool(false))]));
        let acpi = s.position("kernel.acpi").unwrap();
        assert_eq!(r.values[acpi], Value::Bool(true));
        assert_eq!(
            r.sources[acpi],
            Source::Selected("kernel.multi_core".into())
        );
    }

    #[test]
    fn arch_hides_x86_execution_options_but_keeps_both_baselines_editable() {
        let s = real_schema();
        let r = resolve(&s, &user(&[("target.arch", Value::Str("aarch64".into()))]));
        assert!(r.active[s.position("cpu.x86_64_level").unwrap()]);
        assert!(!r.active[s.position("kernel.cpu.smap").unwrap()]);
        assert!(r.active[s.position("cpu.aarch64_level").unwrap()]);
    }

    #[test]
    fn rules_fire() {
        let s = real_schema();
        let r = resolve(
            &s,
            &user(&[("kernel.cpu.xsave", Value::Str("never".into()))]),
        );
        assert_eq!(r.errors(), 1, "{:?}", r.diags);
    }

    #[test]
    fn v4_needs_avx512_patched_in() {
        let s = real_schema();
        let r = resolve(
            &s,
            &user(&[
                ("cpu.x86_64_level", Value::Str("v4".into())),
                ("kernel.cpu.avx512", Value::Str("auto".into())),
                ("kernel.self_modifying", Value::Bool(false)),
            ]),
        );
        assert!(
            r.diags
                .iter()
                .any(|d| d.level == Level::Error && d.message.contains("kernel.cpu.avx512")),
            "{:?}",
            r.diags
        );
    }

    #[test]
    fn edit_preserves_comments() {
        let s = real_schema();
        let dir = std::env::temp_dir().join(format!("ferric-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ferric.toml");
        std::fs::write(&path, "# keep me\n[kernel]\nnuma = true # and me\n").unwrap();
        let (mut u, diags) = UserConfig::load(&path, &s).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(u.values.get("kernel.numa"), Some(&Value::Bool(true)));
        u.set("kernel.cpu.smap", Value::Str("always".into()))
            .unwrap();
        u.set("cpu.x86_64_level", Value::Str("v4".into())).unwrap();
        u.unset("kernel.numa");
        let text = u.render();
        assert!(text.contains("# keep me"), "{text}");
        assert!(text.contains("[kernel.cpu]\nsmap = \"always\""), "{text}");
        assert!(text.contains("[cpu]\nx86_64_level = \"v4\""), "{text}");
        assert!(!text.contains("numa"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_keys_are_reported() {
        let s = real_schema();
        let dir = std::env::temp_dir().join(format!("ferric-config-unk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ferric.toml");
        std::fs::write(
            &path,
            "[kernel]\nnumaa = true\n[cpu]\nx86_64_level = \"v9\"\n",
        )
        .unwrap();
        let (u, diags) = UserConfig::load(&path, &s).unwrap();
        assert!(u.values.is_empty());
        assert_eq!(diags.len(), 2, "{diags:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_errors_preserve_paths_and_sources() {
        use std::error::Error;
        let schema = real_schema();
        let directory = tempfile::tempdir().unwrap();
        let Err(error) = UserConfig::load(directory.path(), &schema) else {
            panic!("a directory cannot be read as a configuration file");
        };
        assert!(matches!(&error, LoadError::Read { path, .. } if path == directory.path()));
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .is_some()
        );

        let path = directory.path().join("ferric.toml");
        std::fs::write(&path, "[target\n").unwrap();
        let Err(error) = UserConfig::load(&path, &schema) else {
            panic!("invalid TOML cannot be loaded");
        };
        assert!(matches!(&error, LoadError::Parse { path: got, .. } if got == &path));
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<toml_edit::TomlError>()
                .is_some()
        );
    }
}
