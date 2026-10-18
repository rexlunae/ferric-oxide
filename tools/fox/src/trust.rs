//! `fox trust`: the pinned author keys and trusted builders in
//! `<root>/etc/fox/trust.toml`.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use fox_core::GitUrl;
use fox_core::keys::load_key_file;
use fox_core::trust::{AuthorKey, TrustStore};
use serde::Serialize;

use crate::commands::{CliError, print_json};
use crate::{Cli, TrustCommand};

pub(crate) fn run(cli: &Cli, command: &TrustCommand) -> Result<ExitCode, CliError> {
    let root = &cli.root;
    let mut store = TrustStore::load(root)?;
    let changed = match command {
        TrustCommand::List => return list(cli, &store),
        TrustCommand::AddAuthor { git, key } => {
            let key = author_key(key)?;
            let fingerprint = key.fingerprint();
            store.pin_author(git_url(git)?, key)?;
            report(cli, &format!("pinned {fingerprint} for {git}"));
            true
        }
        TrustCommand::RemoveAuthor { git } => {
            let removed = store.remove_author(&git_url(git)?);
            report(
                cli,
                &if removed {
                    format!("removed the author key of {git}")
                } else {
                    format!("{git} has no pinned author key")
                },
            );
            removed
        }
        TrustCommand::AddBuilder { label, key } => {
            let key = load_key_file(key)?.key;
            store.add_builder(label, key)?;
            report(cli, &format!("trusting builder {label} ({key})"));
            true
        }
        TrustCommand::RemoveBuilder { label } => {
            let removed = store.remove_builder(label);
            report(
                cli,
                &if removed {
                    format!("removed builder {label}")
                } else {
                    format!("no builder named {label}")
                },
            );
            removed
        }
    };
    if changed {
        store.save(root)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn report(cli: &Cli, message: &str) {
    if !cli.json {
        println!("{message}");
    }
}

fn git_url(text: &str) -> Result<GitUrl, CliError> {
    GitUrl::new(text).map_err(|source| CliError::GitUrl {
        url: text.to_owned(),
        source,
    })
}

/// An OpenSSH public key given inline or as a file.
fn author_key(text: &str) -> Result<AuthorKey, CliError> {
    let path = Path::new(text);
    let key_text = if path.is_file() {
        fs::read_to_string(path).map_err(|source| CliError::Output {
            path: path.to_path_buf(),
            source,
        })?
    } else {
        text.to_owned()
    };
    AuthorKey::from_openssh(&key_text).map_err(|source| CliError::AuthorKey {
        key: text.to_owned(),
        source,
    })
}

#[derive(Serialize)]
struct TrustJson {
    authors: Vec<AuthorJson>,
    builders: Vec<BuilderJson>,
}

#[derive(Serialize)]
struct AuthorJson {
    git: String,
    fingerprint: String,
}

#[derive(Serialize)]
struct BuilderJson {
    label: String,
    key: String,
}

fn list(cli: &Cli, store: &TrustStore) -> Result<ExitCode, CliError> {
    let out = TrustJson {
        authors: store
            .authors()
            .iter()
            .map(|entry| AuthorJson {
                git: entry.git.to_string(),
                fingerprint: entry.key.fingerprint(),
            })
            .collect(),
        builders: store
            .builders()
            .iter()
            .map(|entry| BuilderJson {
                label: entry.label.clone(),
                key: entry.key.to_string(),
            })
            .collect(),
    };
    if cli.json {
        print_json(&out)?;
        return Ok(ExitCode::SUCCESS);
    }
    for author in &out.authors {
        println!("author  {} {}", author.git, author.fingerprint);
    }
    for builder in &out.builders {
        println!("builder {} {}", builder.label, builder.key);
    }
    if out.authors.is_empty() && out.builders.is_empty() {
        println!("no pinned authors or trusted builders");
    }
    Ok(ExitCode::SUCCESS)
}
