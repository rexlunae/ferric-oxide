//! features.toml: the catalog of menus, options, effects and rules.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use crate::expr::{self, Expr};
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Bool,
    Choice,
    Int,
    String,
    List,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Bool => "bool",
            Kind::Choice => "choice",
            Kind::Int => "int",
            Kind::String => "string",
            Kind::List => "list",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ChoiceDef {
    Plain(String),
    Full {
        value: String,
        title: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub value: String,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondDefault {
    pub when: String,
    pub value: toml::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    pub when: Option<String>,
    #[serde(default)]
    pub make: BTreeMap<String, String>,
    #[serde(default)]
    pub make_append: BTreeMap<String, String>,
    #[serde(default)]
    pub kernel_features: Vec<String>,
    #[serde(default)]
    pub kernel_config: BTreeMap<String, String>,
    /// Keys of the initfs `trace.toml` (`enable`, `disable`), each a list of selections;
    /// `"{value}"` takes a list option's items.
    #[serde(default)]
    pub trace_config: BTreeMap<String, String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub ignore_packages: Vec<String>,
    /// Packages to add built from source even under REPO_BINARY, for
    /// recipes that exist only in this repository.
    #[serde(default)]
    pub source_packages: Vec<String>,
    #[serde(default)]
    pub packages_from_value: bool,
    #[serde(default)]
    pub ignore_from_value: bool,
    #[serde(default)]
    pub general: BTreeMap<String, toml::Value>,
    /// Lets this effect replace a make variable another option assigned.
    #[serde(default, rename = "override")]
    pub override_: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOption {
    id: String,
    menu: String,
    #[serde(rename = "type")]
    kind: Kind,
    title: String,
    #[serde(default)]
    help: String,
    default: toml::Value,
    #[serde(default)]
    defaults: Vec<CondDefault>,
    depends: Option<String>,
    #[serde(default)]
    selects: Vec<String>,
    #[serde(default)]
    choices: Vec<ChoiceDef>,
    min: Option<i64>,
    max: Option<i64>,
    #[serde(default)]
    experimental: bool,
    make_var: Option<String>,
    make_true: Option<String>,
    make_false: Option<String>,
    kernel_feature: Option<String>,
    kernel_config: Option<String>,
    trace_config: Option<String>,
    #[serde(default)]
    rebuild: Vec<String>,
    #[serde(default)]
    effect: Vec<Effect>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMenu {
    id: String,
    title: String,
    parent: Option<String>,
    #[serde(default)]
    help: String,
    depends: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warn,
    Error,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    level: Level,
    when: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    #[serde(default)]
    menu: Vec<RawMenu>,
    #[serde(default)]
    option: Vec<RawOption>,
    #[serde(default)]
    rule: Vec<RawRule>,
}

/// A compiled effect: the `when` expression is parsed.
#[derive(Debug, Clone)]
pub struct CEffect {
    pub when: Option<Expr>,
    pub def: Effect,
}

#[derive(Debug, Clone)]
pub struct OptDef {
    pub id: String,
    pub menu: String,
    pub kind: Kind,
    pub title: String,
    pub help: String,
    pub default: Value,
    pub defaults: Vec<(Expr, Value, String)>,
    pub depends: Option<(Expr, String)>,
    pub selects: Vec<String>,
    pub choices: Vec<Choice>,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub experimental: bool,
    pub rebuild: Vec<String>,
    pub effects: Vec<CEffect>,
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub id: String,
    pub title: String,
    pub parent: Option<String>,
    pub help: String,
    pub depends: Option<(Expr, String)>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub level: Level,
    pub when: Expr,
    pub when_src: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub menus: Vec<Menu>,
    pub options: Vec<OptDef>,
    pub rules: Vec<Rule>,
    index: HashMap<String, usize>,
}

/// Menu entries in display order: submenus and options interleaved as
/// they appear in features.toml.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Menu(String),
    Option(usize),
}

impl OptDef {
    /// Checks a value against this option's type and constraints.
    pub fn check(&self, v: &Value) -> Result<(), String> {
        match (self.kind, v) {
            (Kind::Bool, Value::Bool(_)) => Ok(()),
            (Kind::String, Value::Str(_)) => Ok(()),
            (Kind::List, Value::List(_)) => Ok(()),
            (Kind::Choice, Value::Str(s)) => {
                if self.choices.iter().any(|c| &c.value == s) {
                    Ok(())
                } else {
                    Err(format!(
                        "`{s}` is not a choice for {}; expected one of {}",
                        self.id,
                        self.choices
                            .iter()
                            .map(|c| c.value.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
            (Kind::Int, Value::Int(n)) => {
                if self.min.is_some_and(|m| *n < m) || self.max.is_some_and(|m| *n > m) {
                    Err(format!(
                        "{} must be between {} and {}",
                        self.id,
                        self.min.map_or("-inf".into(), |m| m.to_string()),
                        self.max.map_or("inf".into(), |m| m.to_string())
                    ))
                } else {
                    Ok(())
                }
            }
            _ => Err(format!(
                "{} expects a {} value, got {v}",
                self.id,
                self.kind.name()
            )),
        }
    }

    /// Parses a command-line string into a value of this option's type.
    pub fn parse_cli(&self, s: &str) -> Result<Value, String> {
        let v = match self.kind {
            Kind::Bool => match s {
                "y" | "yes" | "true" | "on" | "1" => Value::Bool(true),
                "n" | "no" | "false" | "off" | "0" => Value::Bool(false),
                _ => {
                    return Err(format!(
                        "{}: `{s}` is not a boolean (use true/false)",
                        self.id
                    ));
                }
            },
            Kind::Int => Value::Int(
                s.trim()
                    .parse()
                    .map_err(|_| format!("{}: `{s}` is not an integer", self.id))?,
            ),
            Kind::Choice | Kind::String => Value::Str(s.to_owned()),
            Kind::List => Value::List(
                s.split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect(),
            ),
        };
        self.check(&v)?;
        Ok(v)
    }
}

fn compile(src: &str, ctx: &str) -> Result<Expr, String> {
    expr::parse(src).map_err(|e| format!("{ctx}: {e}"))
}

impl Schema {
    pub fn load(path: &Path) -> Result<Schema, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Schema::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Schema, String> {
        let raw: RawSchema = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut menus = Vec::new();
        for m in raw.menu {
            menus.push(Menu {
                depends: m
                    .depends
                    .as_ref()
                    .map(|d| Ok::<_, String>((compile(d, &m.id)?, d.clone())))
                    .transpose()?,
                id: m.id,
                title: m.title,
                parent: m.parent,
                help: m.help,
            });
        }

        let mut options = Vec::new();
        for o in raw.option {
            let ctx = o.id.clone();
            let default = Value::from_toml(&o.default)
                .ok_or_else(|| format!("{ctx}: unsupported default"))?;
            let mut effects = Vec::new();
            if let Some(var) = &o.make_var {
                match o.kind {
                    Kind::Bool => {
                        effects.push(CEffect {
                            when: Some(Expr::Ident("value".into())),
                            def: Effect {
                                make: [(var.clone(), o.make_true.clone().unwrap_or("1".into()))]
                                    .into(),
                                ..Default::default()
                            },
                        });
                        effects.push(CEffect {
                            when: Some(Expr::Not(Box::new(Expr::Ident("value".into())))),
                            def: Effect {
                                make: [(var.clone(), o.make_false.clone().unwrap_or("0".into()))]
                                    .into(),
                                ..Default::default()
                            },
                        });
                    }
                    _ => effects.push(CEffect {
                        when: None,
                        def: Effect {
                            make: [(var.clone(), "{value}".into())].into(),
                            ..Default::default()
                        },
                    }),
                }
            }
            if let Some(feat) = &o.kernel_feature {
                if o.kind != Kind::Bool {
                    return Err(format!("{ctx}: kernel_feature needs a bool option"));
                }
                effects.push(CEffect {
                    when: Some(Expr::Ident("value".into())),
                    def: Effect {
                        kernel_features: vec![feat.clone()],
                        ..Default::default()
                    },
                });
            }
            if let Some(key) = &o.kernel_config {
                effects.push(CEffect {
                    when: None,
                    def: Effect {
                        kernel_config: [(key.clone(), "{value}".into())].into(),
                        ..Default::default()
                    },
                });
            }
            if let Some(key) = &o.trace_config {
                if o.kind != Kind::List {
                    return Err(format!("{ctx}: trace_config needs a list option"));
                }
                effects.push(CEffect {
                    when: None,
                    def: Effect {
                        trace_config: [(key.clone(), "{value}".into())].into(),
                        ..Default::default()
                    },
                });
            }
            for e in o.effect {
                let when = match (&e.when, o.kind) {
                    (Some(w), _) => Some(compile(w, &ctx)?),
                    // A bool option's effects apply when it is on, unless told otherwise.
                    (None, Kind::Bool) => Some(Expr::Ident("value".into())),
                    (None, _) => None,
                };
                effects.push(CEffect { when, def: e });
            }
            let mut defaults = Vec::new();
            for d in o.defaults {
                let v = Value::from_toml(&d.value)
                    .ok_or_else(|| format!("{ctx}: unsupported conditional default"))?;
                defaults.push((compile(&d.when, &ctx)?, v, d.when));
            }
            let choices = o
                .choices
                .into_iter()
                .map(|c| match c {
                    ChoiceDef::Plain(v) => Choice {
                        title: v.clone(),
                        value: v,
                    },
                    ChoiceDef::Full { value, title } => Choice {
                        title: title.unwrap_or_else(|| value.clone()),
                        value,
                    },
                })
                .collect();
            options.push(OptDef {
                depends: o
                    .depends
                    .as_ref()
                    .map(|d| Ok::<_, String>((compile(d, &ctx)?, d.clone())))
                    .transpose()?,
                id: o.id,
                menu: o.menu,
                kind: o.kind,
                title: o.title,
                help: o.help,
                default,
                defaults,
                selects: o.selects,
                choices,
                min: o.min,
                max: o.max,
                experimental: o.experimental,
                rebuild: o.rebuild,
                effects,
            });
        }

        let mut rules = Vec::new();
        for r in raw.rule {
            rules.push(Rule {
                level: r.level,
                when: compile(&r.when, "rule")?,
                when_src: r.when,
                message: r.message,
            });
        }

        let index = options
            .iter()
            .enumerate()
            .map(|(i, o)| (o.id.clone(), i))
            .collect();
        let schema = Schema {
            menus,
            options,
            rules,
            index,
        };
        schema.validate()?;
        Ok(schema)
    }

    fn validate(&self) -> Result<(), String> {
        let menu_ids: HashSet<&str> = self.menus.iter().map(|m| m.id.as_str()).collect();
        if menu_ids.len() != self.menus.len() {
            return Err("duplicate menu id".into());
        }
        if self.index.len() != self.options.len() {
            return Err("duplicate option id".into());
        }
        for m in &self.menus {
            if let Some(p) = &m.parent
                && !menu_ids.contains(p.as_str())
            {
                return Err(format!("menu {}: unknown parent {p}", m.id));
            }
            if let Some((e, _)) = &m.depends {
                self.check_idents(e, &m.id, false)?;
            }
        }
        for o in &self.options {
            if !menu_ids.contains(o.menu.as_str()) {
                return Err(format!("{}: unknown menu {}", o.id, o.menu));
            }
            if o.kind == Kind::Choice && o.choices.is_empty() {
                return Err(format!("{}: choice option without choices", o.id));
            }
            o.check(&o.default).map_err(|e| format!("default: {e}"))?;
            for (e, v, _) in &o.defaults {
                self.check_idents(e, &o.id, false)?;
                o.check(v)
                    .map_err(|e| format!("conditional default: {e}"))?;
            }
            if let Some((e, _)) = &o.depends {
                self.check_idents(e, &o.id, false)?;
            }
            for e in &o.effects {
                if let Some(w) = &e.when {
                    self.check_idents(w, &o.id, true)?;
                }
            }
            for s in &o.selects {
                match self.get(s) {
                    Some(t) if t.kind == Kind::Bool && o.kind == Kind::Bool => {}
                    Some(_) => {
                        return Err(format!("{}: selects {s}, but both must be bools", o.id));
                    }
                    None => return Err(format!("{}: selects unknown option {s}", o.id)),
                }
            }
            // An option id must not also be a table holding other options.
            let prefix = format!("{}.", o.id);
            if let Some(other) = self.options.iter().find(|x| x.id.starts_with(&prefix)) {
                return Err(format!(
                    "{} is both an option and a prefix of {}",
                    o.id, other.id
                ));
            }
        }
        for r in &self.rules {
            self.check_idents(&r.when, "rule", false)?;
        }
        Ok(())
    }

    fn check_idents(&self, e: &Expr, ctx: &str, allow_value: bool) -> Result<(), String> {
        let mut ids = Vec::new();
        e.idents(&mut ids);
        for id in ids {
            // `host.arch` is supplied by the resolver, not declared as an option.
            if (allow_value && id == "value")
                || id == crate::model::HOST_ARCH
                || self.index.contains_key(&id)
            {
                continue;
            }
            return Err(format!("{ctx}: expression refers to unknown option `{id}`"));
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&OptDef> {
        self.index.get(id).map(|&i| &self.options[i])
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }

    pub fn menu(&self, id: &str) -> Option<&Menu> {
        self.menus.iter().find(|m| m.id == id)
    }

    /// Top-level menus.
    pub fn roots(&self) -> Vec<Entry> {
        self.menus
            .iter()
            .filter(|m| m.parent.is_none())
            .map(|m| Entry::Menu(m.id.clone()))
            .collect()
    }

    /// Options of a menu followed by its submenus.
    pub fn entries(&self, menu: &str) -> Vec<Entry> {
        let mut out: Vec<Entry> = self
            .options
            .iter()
            .enumerate()
            .filter(|(_, o)| o.menu == menu)
            .map(|(i, _)| Entry::Option(i))
            .collect();
        out.extend(
            self.menus
                .iter()
                .filter(|m| m.parent.as_deref() == Some(menu))
                .map(|m| Entry::Menu(m.id.clone())),
        );
        out
    }

    /// Closest option ids, for "did you mean" hints.
    pub fn suggest(&self, id: &str) -> Option<&str> {
        let tail = id.rsplit('.').next().unwrap_or(id);
        self.options
            .iter()
            .map(|o| o.id.as_str())
            .find(|o| o.ends_with(&format!(".{tail}")) || o.contains(tail))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn real_schema() -> Schema {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../features.toml");
        Schema::load(&path).unwrap()
    }

    #[test]
    fn repository_schema_is_valid() {
        let s = real_schema();
        assert!(s.get("cpu.x86_64_level").is_some());
        assert!(s.get("kernel.multi_core").is_some());
        assert!(!s.roots().is_empty());
    }

    #[test]
    fn rejects_unknown_reference() {
        let err = Schema::parse(
            r#"
            [[menu]]
            id = "m"
            title = "M"
            [[option]]
            id = "m.a"
            menu = "m"
            type = "bool"
            title = "A"
            default = false
            depends = "m.missing"
            "#,
        )
        .unwrap_err();
        assert!(err.contains("m.missing"), "{err}");
    }

    #[test]
    fn rejects_bad_default() {
        let err = Schema::parse(
            r#"
            [[menu]]
            id = "m"
            title = "M"
            [[option]]
            id = "m.c"
            menu = "m"
            type = "choice"
            title = "C"
            default = "z"
            choices = ["x", "y"]
            "#,
        )
        .unwrap_err();
        assert!(err.contains("not a choice"), "{err}");
    }
}
