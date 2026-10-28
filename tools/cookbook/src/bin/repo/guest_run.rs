//! Running a target-built tool during a recipe's build.
//!
//! A recipe runs its build tools (`build.dev-dependencies`) while it compiles.
//! With `features.toml` `build.build_tools = "guest"` those tools are built for
//! the target, so they are Redox binaries and the host cannot execute them. The
//! build script reaches them through this command, which runs one command in the
//! guest build environment `mk/fstools.mk` installed for the build.
//!
//! The environment is reused between calls: this command never rebuilds or
//! reinstalls it, it only boots it. A build that runs the same tool twice pays
//! for the boot, not for another installation.
//!
//! The tool runs from the repository root, with the recipe's working directory
//! available at the same relative place, so a tool that takes paths sees the
//! files it was given.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use cookbook::{Error, Result};

/// A `repo guest-run` invocation.
#[derive(Debug)]
pub struct GuestRun {
    /// The installed guest build environment's image.
    guest: PathBuf,
    /// Suppress the guest's chatter when the recipe asked for quiet builds.
    silent: bool,
    /// The command and its arguments, as the build script passed them.
    command: Vec<OsString>,
}

impl GuestRun {
    /// Whether `args` (the arguments after `guest-run`) asks for this command.
    pub fn matches(args: &[String]) -> bool {
        args.first().is_some_and(|arg| arg == "guest-run")
    }

    /// Parses the arguments after `guest-run`.
    ///
    /// # Errors
    /// Returns an options error when `--guest` is missing, has no value, or the
    /// command after `--` is empty.
    #[allow(clippy::result_large_err)]
    pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut guest = None;
        let mut silent = false;
        let mut command = Vec::new();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--" => {
                    command.extend(args.map(OsString::from));
                    break;
                }
                "--guest" => match args.next() {
                    Some(value) if !value.is_empty() => guest = Some(PathBuf::from(value)),
                    _ => return Err(Error::from("guest-run: --guest needs a path".to_string())),
                },
                "--silent" => {
                    // The build passes 0 or 1, matching the shell's test.
                    silent = args.next().is_some_and(|value| value == "1");
                }
                other => {
                    return Err(Error::from(format!(
                        "guest-run: unexpected argument {other:?}; put the command after --"
                    )));
                }
            }
        }
        let guest =
            guest.ok_or_else(|| Error::from("guest-run: --guest is required".to_string()))?;
        if command.is_empty() {
            return Err(Error::from(
                "guest-run: no command given after --".to_string(),
            ));
        }
        Ok(Self {
            guest,
            silent,
            command,
        })
    }

    /// Runs the command in the guest and mirrors its exit status.
    ///
    /// # Errors
    /// Returns an error when the environment is not installed, or `redoxer` is
    /// missing or cannot be started. A failing command's own status is not an
    /// error here: the caller exits with it, so the build sees the tool's
    /// failure the way it would see a host tool's.
    #[allow(clippy::result_large_err)]
    pub fn run(&self) -> Result<()> {
        if !self.guest.is_file() {
            return Err(Error::from(format!(
                "guest-run: the guest build environment {} is missing; build it with `make build_tools`",
                self.guest.display()
            )));
        }
        let program = self.command[0].to_string_lossy().into_owned();
        let mut arg = self
            .command
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        arg.remove(0);

        // The guest has the tool set installed; the command names the tool.
        let mut exec =
            Command::new(std::env::var_os("REDOXER").unwrap_or_else(|| "redoxer".into()));
        exec.arg("exec");
        if self.silent {
            // Keep the tool's own output; only the guest's boot noise is hidden.
            exec.args(["--output", "-"]);
        }
        exec.arg(program);
        exec.args(arg);
        exec.env("REDOXER_COOKBOOK", cookbook_root());

        let status = exec.status().map_err(|source| Error::Io {
            source,
            path: Some(PathBuf::from("redoxer")),
            context: "running a build tool in the guest",
        })?;
        if !status.success() {
            return Err(Error::from(format!(
                "guest-run: {status} from the guest tool"
            )));
        }
        Ok(())
    }
}

/// The cookbook root the guest's packages come from.
fn cookbook_root() -> PathBuf {
    std::env::var_os("COOKBOOK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_and_its_arguments_follow_the_separator() {
        let run = GuestRun::parse(
            [
                "--guest",
                "/tmp/guest.img",
                "--silent",
                "1",
                "--",
                "mdbook",
                "build",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(run.guest, PathBuf::from("/tmp/guest.img"));
        assert!(run.silent);
        assert_eq!(
            run.command,
            [OsString::from("mdbook"), OsString::from("build")]
        );
    }

    #[test]
    fn the_environment_is_required() {
        let error = GuestRun::parse(["--", "mdbook"].into_iter().map(str::to_owned)).unwrap_err();
        assert!(error.to_string().contains("--guest is required"));
    }

    #[test]
    fn a_missing_command_is_rejected() {
        let error = GuestRun::parse(["--guest", "/tmp/guest.img"].into_iter().map(str::to_owned))
            .unwrap_err();
        assert!(error.to_string().contains("no command"));
    }

    #[test]
    fn arguments_before_the_separator_are_rejected_rather_than_passed_to_the_tool() {
        let error = GuestRun::parse(
            ["--guest", "/tmp/guest.img", "mdbook"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unexpected argument"));
    }
}
