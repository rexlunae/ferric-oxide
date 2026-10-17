use std::process;

use crate::{status_error, target};

fn inner<I: Iterator<Item = String>>(mut args: I) -> anyhow::Result<()> {
    let command = args.next().unwrap();
    let subcommand = args.next().unwrap();

    #[cfg(feature = "cli-exec")]
    let (runner, arguments) = {
        let (runner, arguments) = runner_args(command, args)?;
        let runner = runner
            .into_iter()
            .map(|s| {
                if s.contains([' ', '"', '\'', '\n']) {
                    format!("{s:?}")
                } else {
                    s
                }
            })
            .collect::<Vec<_>>();
        (runner.join(" "), arguments)
    };
    #[cfg(not(feature = "cli-exec"))]
    let (runner, arguments) = {
        let mut matching = true;
        let mut arguments = Vec::new();
        while let Some(arg) = args.next() {
            match (arg.as_str(), matching) {
                (
                    "-f" | "--folder" | "-a" | "--artifact" | "-i" | "--install-config" | "-o"
                    | "--output" | "-g" | "--gui" | "-h" | "--help",
                    true,
                ) => anyhow::bail!("feature 'cli-exec' is not compiled, please omit exec args"),
                ("--", true) => matching = false,
                _ => {
                    matching = false;
                    arguments.push(arg);
                }
            }
        }
        (format!("{command} exec"), arguments)
    };

    let cc_target_var = target().replace("-", "_");
    let cargo_target_var = cc_target_var.to_uppercase();

    crate::env::command("cargo")?
        .arg(subcommand)
        .arg("--target")
        .arg(target())
        .args(arguments)
        .env(format!("CARGO_TARGET_{cargo_target_var}_RUNNER"), runner)
        .status()
        .and_then(status_error)?;

    Ok(())
}

pub fn main(args: &[String]) {
    match inner(args.iter().cloned()) {
        Ok(()) => {
            process::exit(0);
        }
        Err(err) => {
            eprintln!("redoxer cargo: {err}");
            process::exit(1);
        }
    }
}

#[cfg(feature = "cli-exec")]
fn runner_args(
    command: String,
    args: impl Iterator<Item = String>,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    use anyhow::Context;
    let mut config =
        crate::exec::RedoxerExecConfig::new(args).context("Unable to parse exec configuration")?;
    let arguments = std::mem::take(&mut config.arguments);
    config.folders.insert(
        "root".into(),
        format!("{}/", std::fs::canonicalize(".")?.display()),
    );
    let mut runner = vec![command, "exec".to_string()];
    runner.extend(config.to_args());
    Ok((runner, arguments))
}

#[cfg(all(test, feature = "cli-exec"))]
mod tests {
    use super::*;

    #[test]
    fn cargo_runner_preserves_custom_install_configuration() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("custom image.toml");
        std::fs::write(&path, "[general]\nfilesystem_size = 512\n").unwrap();
        let (runner, cargo_args) = runner_args(
            "redoxer".into(),
            [
                "--install-config".into(),
                path.to_string_lossy().into_owned(),
                "--".into(),
                "--release".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(cargo_args, ["--release"]);
        let reconstructed =
            crate::exec::RedoxerExecConfig::new(runner.into_iter().skip(2)).unwrap();
        assert_eq!(reconstructed.install_config, Some(path));
        assert_eq!(reconstructed.config_name, "custom image");
        assert!(reconstructed.config_toml.contains("512"));
        assert!(reconstructed.arguments.is_empty());
    }
}
