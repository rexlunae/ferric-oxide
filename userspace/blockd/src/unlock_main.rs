//! `ferric-unlock [TABLE]`: the boot step that unlocks and mounts the volumes listed in
//! `/etc/ferric/crypttab` (or `TABLE`), features.toml `fs.unlock_at_boot`.
//!
//! Init runs it as the oneshot unit `05_ferric-unlock.service`, so the volumes are served
//! before the services that start after it. A missing table is not an error: there is
//! nothing to unlock. A table that does not parse starts nothing. Otherwise every volume is
//! tried, and the exit status is a failure if any of them was not unlocked or not mounted.

use std::error::Error;
use std::io;
use std::process::ExitCode;

use ferric_blockd::crypttab;
use ferric_blockd::unlock::{self, Processes};

fn main() -> ExitCode {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| crypttab::PATH.to_owned());
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!("ferric-unlock: no {path}, nothing to unlock");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("ferric-unlock: cannot read {path}{}", chain(&err));
            return ExitCode::FAILURE;
        }
    };
    let volumes = match crypttab::parse(&text) {
        Ok(volumes) => volumes,
        Err(err) => {
            eprintln!("ferric-unlock: {path}: nothing unlocked{}", chain(&err));
            return ExitCode::FAILURE;
        }
    };
    let mut failed = false;
    for outcome in unlock::unlock_all(&volumes, &mut Processes::default()) {
        match (&outcome.result, &outcome.mounted) {
            (Ok(()), Some(mounted)) => println!(
                "ferric-unlock: /scheme/{} unlocked, mounted at /scheme/{mounted}",
                outcome.scheme
            ),
            (Ok(()), None) => println!("ferric-unlock: /scheme/{} unlocked", outcome.scheme),
            (Err(err), _) => {
                failed = true;
                eprintln!("ferric-unlock: /scheme/{}{}", outcome.scheme, chain(err));
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `": error: its source: ..."`, the whole chain of `err`, for the boot log.
fn chain(err: &dyn Error) -> String {
    let mut text = String::new();
    let mut next = Some(err);
    while let Some(err) = next {
        text.push_str(": ");
        text.push_str(&err.to_string());
        next = err.source();
    }
    text
}
