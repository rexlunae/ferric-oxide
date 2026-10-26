//! CPU variant queries and sequential builds into distinct image directories.
//!
//! The feature catalog supplies enabled target/level pairs. Shared Make
//! fingerprint state invalidates source recipes as the effective level changes.

use crate::{Architecture, BuildError, Result, cargo, execute, make_command};
use std::{ffi::OsString, path::Path, process::Command};

#[derive(Debug, serde::Deserialize)]
struct CpuVariant {
    arch: String,
    level: String,
    image: String,
}

fn variants_command(root: &Path, args: &[OsString]) -> Command {
    let mut command = cargo(root);
    command.args([
        "run",
        "--locked",
        "--quiet",
        "--release",
        "--package",
        "ferric-config",
        "--",
    ]);
    if let Some(config) = args
        .iter()
        .rev()
        .find_map(|arg| arg.to_str()?.strip_prefix("FERRIC_CONFIG="))
    {
        command.args(["--config", config]);
    }
    command.args(["variants", "--json"]);
    command
}

fn enabled_variants(root: &Path, args: &[OsString]) -> Result<Vec<CpuVariant>> {
    let mut command = variants_command(root, args);
    eprintln!("+ {command:?}");
    let program = command.get_program().to_owned();
    let output = command.output().map_err(|source| BuildError::Start {
        program: program.clone(),
        source,
    })?;
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(BuildError::Failed {
            program,
            status: output.status,
        });
    }
    serde_json::from_slice(&output.stdout).map_err(|source| BuildError::VariantPlan { source })
}

fn variant_command(
    root: &Path,
    target: &str,
    variant: &CpuVariant,
    podman: bool,
    args: &[OsString],
) -> Result<Command> {
    if args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg.starts_with("BUILD=") || arg.starts_with("FERRIC_CPU_VARIANT="))
    }) {
        return Err(BuildError::VariantAssignment);
    }
    let arch = Architecture::parse(&variant.arch)?;
    let image = args
        .iter()
        .rev()
        .find_map(|arg| arg.to_str()?.strip_prefix("CONFIG_NAME="))
        .unwrap_or(&variant.image);
    let mut command = make_command(root, target, arch, podman, args);
    command.arg(format!("FERRIC_CPU_VARIANT={}", variant.level));
    command.arg(format!(
        "BUILD=build/{}/{image}/variants/{}",
        arch.name(),
        variant.level
    ));
    Ok(command)
}

fn variant_commands(
    root: &Path,
    variants: &[CpuVariant],
    filter: Option<Architecture>,
    podman: bool,
    args: &[OsString],
) -> Result<Vec<Command>> {
    let commands: Vec<_> = variants
        .iter()
        .filter(|variant| filter.is_none_or(|arch| arch.name() == variant.arch))
        .map(|variant| variant_command(root, "qemu-image", variant, podman, args))
        .collect::<Result<_>>()?;
    if commands.is_empty() {
        return Err(BuildError::NoVariants);
    }
    Ok(commands)
}

/// Builds or boots one enabled variant of the selected architecture.
///
/// # Errors
///
/// Returns an error for configuration/query failures, disabled variants,
/// conflicting output assignments, or a failed Make command.
pub(super) fn run_one(
    root: &Path,
    target: &str,
    arch: Architecture,
    level: &str,
    podman: bool,
    args: &[OsString],
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return execute(&mut variants_command(root, args), true);
    }
    let variant = enabled_variants(root, args)?
        .into_iter()
        .find(|variant| variant.arch == arch.name() && variant.level == level)
        .ok_or_else(|| BuildError::DisabledVariant {
            arch: arch.name(),
            level: level.to_owned(),
        })?;
    execute(
        &mut variant_command(root, target, &variant, podman, args)?,
        false,
    )
}

/// Builds all selected variants, optionally restricted to one architecture.
///
/// # Errors
///
/// Returns query/decoding errors, an empty-selection error, conflicting output
/// assignments, or the first failed Make command. Builds run sequentially.
pub(super) fn build_all(
    root: &Path,
    filter: Option<Architecture>,
    podman: bool,
    args: &[OsString],
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return execute(&mut variants_command(root, args), true);
    }
    let variants = enabled_variants(root, args)?;
    for mut command in variant_commands(root, &variants, filter, podman, args)? {
        execute(&mut command, false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;
    use std::ffi::OsStr;
    #[test]
    fn variants_get_distinct_output_paths_and_correct_architecture_level() {
        for (arch, level) in [("x86_64", "v3"), ("x86_64", "v4"), ("aarch64", "armv8.2-a")] {
            let variant = CpuVariant {
                arch: arch.into(),
                level: level.into(),
                image: "native".into(),
            };
            let command =
                variant_command(Path::new("/example"), "qemu-image", &variant, false, &[]).unwrap();
            let args: Vec<_> = command.get_args().collect();
            for expected in [
                format!("ARCH={arch}"),
                format!("FERRIC_CPU_VARIANT={level}"),
                format!("BUILD=build/{arch}/native/variants/{level}"),
            ] {
                assert!(args.contains(&OsStr::new(&expected)), "{args:?}");
            }
            assert!(
                variant_command(
                    Path::new("/example"),
                    "qemu",
                    &variant,
                    false,
                    &["BUILD=shared".into()]
                )
                .is_err()
            );
        }
        assert!(
            Cli::try_parse_from(["xtask", "build", "--all-variants", "--cpu-variant", "v4"])
                .is_err()
        );
    }

    #[test]
    fn variant_query_preserves_config_selection_and_image_override() {
        let args = [
            "FERRIC_CONFIG=/example/config with spaces.toml".into(),
            "CONFIG_NAME=desktop".into(),
        ];
        let command = variants_command(Path::new("/example"), &args);
        assert!(
            command
                .get_args()
                .any(|arg| arg == "/example/config with spaces.toml")
        );
        let variant = CpuVariant {
            arch: "aarch64".into(),
            level: "armv9-a".into(),
            image: "native".into(),
        };
        let command =
            variant_command(Path::new("/example"), "qemu-run", &variant, false, &args).unwrap();
        assert!(
            command
                .get_args()
                .any(|arg| arg == "BUILD=build/aarch64/desktop/variants/armv9-a")
        );
    }
    #[test]
    fn all_variant_plans_filter_architectures_and_never_share_an_output_directory() {
        let variants =
            [("x86_64", "v3"), ("x86_64", "v4"), ("aarch64", "armv8.0-a")].map(|(arch, level)| {
                CpuVariant {
                    arch: arch.into(),
                    level: level.into(),
                    image: "native".into(),
                }
            });
        let commands =
            variant_commands(Path::new("/example"), &variants, None, false, &[]).unwrap();
        let paths: std::collections::BTreeSet<_> = commands
            .iter()
            .map(|cmd| {
                cmd.get_args()
                    .find(|arg| arg.to_str().unwrap().starts_with("BUILD="))
                    .unwrap()
            })
            .collect();
        assert_eq!(paths.len(), 3);
        assert_eq!(
            variant_commands(
                Path::new("/example"),
                &variants,
                Some(Architecture::X86_64),
                false,
                &[]
            )
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            variant_commands(
                Path::new("/example"),
                &variants,
                Some(Architecture::Aarch64),
                false,
                &[]
            )
            .unwrap()
            .len(),
            1
        );
        assert!(matches!(
            variant_commands(Path::new("/example"), &[], None, false, &[]),
            Err(BuildError::NoVariants)
        ));
    }
}
