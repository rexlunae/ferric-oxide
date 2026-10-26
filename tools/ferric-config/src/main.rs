//! ferric-config: menuconfig-style feature selection for the Ferric Oxide
//! build, stored in a plain TOML file so people and agents can edit it.

mod expr;
mod generate;
mod model;
mod schema;
mod tui;
mod value;
mod variants;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use model::{Diag, Resolved, Source, UserConfig};
use schema::{Kind, Level, Schema};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Build system root (default: nearest directory with features.toml)
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// Feature catalog (default: `<root>/features.toml`)
    #[arg(long, global = true)]
    schema: Option<PathBuf>,
    /// Your configuration (default: `<root>/ferric.toml`)
    #[arg(long, global = true, env = "FERRIC_CONFIG")]
    config: Option<PathBuf>,
    /// Where generated files go, relative to the root
    #[arg(long, global = true, default_value = "build/ferric")]
    out: PathBuf,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive editor (the default on a terminal)
    Menu,
    /// Show every option with its value and where the value came from
    List {
        #[arg(long)]
        json: bool,
        /// Only options set in the configuration file
        #[arg(long)]
        changed: bool,
    },
    /// Print one option's value
    Get {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Set options: `set kernel.numa true` or `set kernel.numa=true cpu.x86_64_level=v4`
    Set {
        #[arg(required = true)]
        args: Vec<String>,
    },
    /// Remove options from the configuration file so they use their defaults
    Unset {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Describe an option: help, type, choices, dependencies
    Explain { id: String },
    /// Validate the configuration; exits non-zero on errors
    Check {
        #[command(flatten)]
        overrides: Overrides,
    },
    /// Write build/ferric/*: config.mk, filesystem.toml, kernel-config.toml
    Generate {
        #[command(flatten)]
        overrides: Overrides,
        /// Write outputs without marking recipes stale or committing rebuild fingerprints
        #[arg(long)]
        no_touch: bool,
        #[arg(long, short)]
        quiet: bool,
    },
    /// Print the whole catalog as JSON
    Schema,
    /// List independently enabled CPU build variants (does not build images).
    Variants {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Default)]
struct Overrides {
    /// Resolve an option=value for this invocation without editing ferric.toml.
    #[arg(long = "override", value_name = "OPTION=VALUE")]
    values: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("cannot find features.toml; pass --root")]
    RootNotFound,
    #[error("cannot load catalog {}: {message}", path.display())]
    Catalog { path: PathBuf, message: String },
    #[error(transparent)]
    Load(#[from] model::LoadError),
    #[error("unknown option {id}{hint}")]
    UnknownOption { id: String, hint: String },
    #[error("expected option=value, got `{0}`")]
    Assignment(String),
    #[error("invalid value for {id}: {message}")]
    Value { id: String, message: String },
    #[error("cannot edit configuration: {0}")]
    Edit(String),
    #[error("cannot save configuration: {0}")]
    Save(String),
    #[error("interactive editor: {0}")]
    Editor(String),
    #[error("fix ferric.toml before editing it interactively")]
    InvalidEditorInput,
    #[error(transparent)]
    Variants(#[from] variants::VariantError),
}

fn assignment(raw: &str) -> Result<(&str, &str), CliError> {
    raw.split_once('=')
        .map(|(id, value)| (id.trim(), value.trim()))
        .ok_or_else(|| CliError::Assignment(raw.to_owned()))
}

fn option_value(ctx: &Ctx, id: &str, raw: &str) -> Result<value::Value, CliError> {
    let option = ctx.schema.get(id).ok_or_else(|| CliError::UnknownOption {
        id: id.to_owned(),
        hint: ctx
            .schema
            .suggest(id)
            .map(|s| format!(" (did you mean {s}?)"))
            .unwrap_or_default(),
    })?;
    option.parse_cli(raw).map_err(|message| CliError::Value {
        id: id.to_owned(),
        message,
    })
}

pub struct Ctx {
    pub root: PathBuf,
    pub schema: Schema,
    pub config_path: PathBuf,
    pub out: PathBuf,
    pub touch_recipes: bool,
}

fn find_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("features.toml").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn print_diags(diags: &[Diag]) {
    for d in diags {
        let tag = match d.level {
            Level::Warn => "warning",
            Level::Error => "error",
        };
        eprintln!("ferric-config: {tag}: {}", d.message);
    }
}

/// Renders an error and its source chain for CLI and TUI diagnostics.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn source_name(s: &Source) -> String {
    match s {
        Source::User => "set".into(),
        Source::Default => "default".into(),
        Source::Selected(by) => format!("selected by {by}"),
        Source::Inactive => "inactive".into(),
    }
}

/// Make variables in .config shadow ferric.toml; say so.
fn dotconfig_shadowing(root: &Path) -> Vec<Diag> {
    let Ok(text) = std::fs::read_to_string(root.join(".config")) else {
        return Vec::new();
    };
    let ours = [
        "ARCH",
        "BOARD",
        "CONFIG_NAME",
        "PODMAN_BUILD",
        "PREFIX_BINARY",
        "REPO_BINARY",
        "REPO_DEBUG",
        "REPO_OFFLINE",
        "REPO_NONSTOP",
        "FILESYSTEM_CONFIG",
        "QEMU_CPU",
    ];
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let name = l.split(['?', ':', '+', '=']).next()?.trim();
            (!l.starts_with('#') && l.contains('=') && ours.contains(&name)).then(|| {
                Diag::warn(format!(
                    ".config sets {name}, which takes precedence over ferric.toml"
                ))
            })
        })
        .collect()
}

fn load(ctx: &Ctx) -> Result<(UserConfig, Resolved, Vec<Diag>), CliError> {
    load_with_overrides(ctx, &Overrides::default())
}

fn load_with_overrides(
    ctx: &Ctx,
    overrides: &Overrides,
) -> Result<(UserConfig, Resolved, Vec<Diag>), CliError> {
    let (user, mut diags) = UserConfig::load(&ctx.config_path, &ctx.schema)?;
    let mut values = user.values.clone();
    for raw in &overrides.values {
        let (id, raw) = assignment(raw)?;
        values.insert(id.to_owned(), option_value(ctx, id, raw)?);
    }
    let r = model::resolve(&ctx.schema, &values);
    diags.extend(r.diags.iter().cloned());
    Ok((user, r, diags))
}

fn option_json(ctx: &Ctx, r: &Resolved, i: usize) -> serde_json::Value {
    let o = &ctx.schema.options[i];
    serde_json::json!({
        "id": o.id,
        "type": o.kind.name(),
        "value": r.values[i].to_json(),
        "default": o.default.to_json(),
        "source": source_name(&r.sources[i]),
        "active": r.active[i],
        "title": o.title,
    })
}

fn run(cli: Cli) -> Result<ExitCode, CliError> {
    let root = match cli.root {
        Some(r) => r,
        None => find_root().ok_or(CliError::RootNotFound)?,
    };
    let schema_path = cli.schema.unwrap_or_else(|| root.join("features.toml"));
    let config_path = match cli.config {
        Some(c) if c.is_relative() && !c.exists() && root.join(&c).exists() => root.join(c),
        Some(c) => c,
        None => root.join("ferric.toml"),
    };
    let ctx = Ctx {
        schema: Schema::load(&schema_path).map_err(|message| CliError::Catalog {
            path: schema_path,
            message,
        })?,
        root,
        config_path,
        out: cli.out,
        touch_recipes: true,
    };

    let command = cli.command.unwrap_or(if std::io::stdout().is_terminal() {
        Cmd::Menu
    } else {
        Cmd::List {
            json: false,
            changed: false,
        }
    });

    match command {
        Cmd::Menu => {
            let (user, _, diags) = load(&ctx)?;
            if diags
                .iter()
                .any(|d| d.message.contains("unknown option") || d.message.contains("expects"))
            {
                print_diags(&diags);
                return Err(CliError::InvalidEditorInput);
            }
            tui::run(&ctx, user).map_err(CliError::Editor)?;
        }
        Cmd::List { json, changed } => {
            let (_, r, diags) = load(&ctx)?;
            let idx: Vec<usize> = (0..ctx.schema.options.len())
                .filter(|&i| !changed || r.sources[i] == Source::User)
                .collect();
            if json {
                let items: Vec<_> = idx.iter().map(|&i| option_json(&ctx, &r, i)).collect();
                println!("{}", serde_json::to_string_pretty(&items).unwrap());
            } else {
                let w = idx
                    .iter()
                    .map(|&i| ctx.schema.options[i].id.len())
                    .max()
                    .unwrap_or(0);
                for i in idx {
                    let o = &ctx.schema.options[i];
                    println!(
                        "{:w$}  {:<24} {}",
                        o.id,
                        r.values[i].to_toml().to_string(),
                        source_name(&r.sources[i])
                    );
                }
            }
            print_diags(&diags);
        }
        Cmd::Get { id, json } => {
            let (_, r, _) = load(&ctx)?;
            let i = ctx
                .schema
                .position(&id)
                .ok_or_else(|| CliError::UnknownOption {
                    id: id.clone(),
                    hint: String::new(),
                })?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&option_json(&ctx, &r, i)).unwrap()
                );
            } else {
                println!("{}", r.values[i].to_toml());
            }
        }
        Cmd::Set { args } => {
            let (mut user, _, _) = load(&ctx)?;
            let mut pairs = Vec::new();
            if args.len() == 2 && !args[0].contains('=') {
                pairs.push((args[0].clone(), args[1].clone()));
            } else {
                for a in &args {
                    let (k, v) = assignment(a)?;
                    pairs.push((k.to_owned(), v.to_owned()));
                }
            }
            for (id, raw) in pairs {
                let v = option_value(&ctx, &id, &raw)?;
                user.set(&id, v).map_err(CliError::Edit)?;
            }
            user.save().map_err(CliError::Save)?;
            let r = model::resolve(&ctx.schema, &user.values);
            print_diags(&r.diags);
            if r.errors() > 0 {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Unset { ids } => {
            let (mut user, _, _) = load(&ctx)?;
            for id in ids {
                if ctx.schema.get(&id).is_none() && !user.values.contains_key(&id) {
                    return Err(CliError::UnknownOption {
                        id,
                        hint: String::new(),
                    });
                }
                user.unset(&id);
            }
            user.save().map_err(CliError::Save)?;
        }
        Cmd::Explain { id } => {
            let (_, r, _) = load(&ctx)?;
            let i = ctx
                .schema
                .position(&id)
                .ok_or_else(|| CliError::UnknownOption {
                    id: id.clone(),
                    hint: String::new(),
                })?;
            print!("{}", tui::describe(&ctx.schema, &r, i));
        }
        Cmd::Check { overrides } => {
            let (_, r, mut diags) = load_with_overrides(&ctx, &overrides)?;
            diags.extend(dotconfig_shadowing(&ctx.root));
            if let Err(errs) = generate::plan(&ctx.schema, &r, &ctx.root, &ctx.out) {
                diags.extend(errs);
            }
            print_diags(&diags);
            let errors = diags.iter().filter(|d| d.level == Level::Error).count();
            if errors > 0 {
                return Ok(ExitCode::FAILURE);
            }
            eprintln!("ferric-config: {} ok", ctx.config_path.display());
        }
        Cmd::Generate {
            no_touch,
            quiet,
            overrides,
        } => {
            let (_, r, mut diags) = load_with_overrides(&ctx, &overrides)?;
            diags.extend(dotconfig_shadowing(&ctx.root));
            let plan = match generate::plan(&ctx.schema, &r, &ctx.root, &ctx.out) {
                Ok(p) => p,
                Err(errs) => {
                    diags.extend(errs);
                    print_diags(&diags);
                    return Ok(ExitCode::FAILURE);
                }
            };
            print_diags(&diags);
            if diags.iter().any(|d| d.level == Level::Error) {
                return Ok(ExitCode::FAILURE);
            }
            let applied = match generate::apply(&ctx.root, &ctx.out, &plan, !no_touch) {
                Ok(applied) => applied,
                Err(error) => {
                    eprintln!("ferric-config: error: {}", error_chain(&error));
                    return Ok(ExitCode::FAILURE);
                }
            };
            if !quiet {
                for p in &applied.written {
                    eprintln!("ferric-config: wrote {}", p.display());
                }
                if no_touch && !applied.stale.is_empty() {
                    eprintln!(
                        "ferric-config: options changed for: {} (not marked for rebuild: --no-touch)",
                        applied.stale.join(", ")
                    );
                } else if applied.stale.iter().any(|s| s == "*") {
                    eprintln!(
                        "ferric-config: options affecting every package changed; marked {} recipes for rebuild",
                        applied.touched
                    );
                } else if !applied.stale.is_empty() {
                    eprintln!(
                        "ferric-config: marked for rebuild: {}",
                        applied.stale.join(", ")
                    );
                }
            }
        }
        Cmd::Schema => {
            let items: Vec<_> = ctx
                .schema
                .options
                .iter()
                .map(|o| {
                    serde_json::json!({
                        "id": o.id,
                        "menu": o.menu,
                        "type": o.kind.name(),
                        "title": o.title,
                        "help": o.help,
                        "default": o.default.to_json(),
                        "choices": (o.kind == Kind::Choice).then(|| o.choices.iter().map(|c| c.value.clone()).collect::<Vec<_>>()),
                        "depends": o.depends.as_ref().map(|(_, s)| s.clone()),
                        "selects": o.selects,
                        "min": o.min,
                        "max": o.max,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&items).unwrap());
        }
        Cmd::Variants { json } => {
            let (user, diags) = UserConfig::load(&ctx.config_path, &ctx.schema)?;
            print_diags(&diags);
            if diags.iter().any(|d| d.level == Level::Error) {
                return Ok(ExitCode::FAILURE);
            }
            let resolved = model::resolve(&ctx.schema, &user.values);
            let variants = variants::enabled(&ctx.schema, &resolved)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&variants).unwrap());
            } else {
                for variant in variants {
                    println!(
                        "{} {} (minimum {}, image {})",
                        variant.arch, variant.level, variant.minimum, variant.image
                    );
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ferric-config: error: {}", error_chain(&e));
            ExitCode::FAILURE
        }
    }
}
