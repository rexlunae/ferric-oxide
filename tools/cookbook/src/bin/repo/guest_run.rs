//! Running a target-built tool during a recipe's build.
//!
//! A recipe runs its build tools (`build.dev-dependencies`) while it compiles.
//! With `features.toml` `build.build_tools = "guest"` those tools are built for
//! the target, so they are Redox binaries and the host cannot execute them. The
//! build script reaches them through this command, which runs one command in the
//! guest build environment `mk/fstools.mk` installed for the build.
//!
//! The environment is reused between calls: this command never rebuilds or
//! reinstalls it, it only boots a copy of it ([`redoxer::guest`]), so every call
//! is independent and recipes cooking in parallel never share a disk.
//!
//! The tool has to behave as a host tool would, so the files it works on travel
//! with it. Paths keep their host names in the guest, which keeps relative and
//! absolute arguments meaning the same thing. Only paths inside the cookbook
//! checkout travel, because those are the build's own files; anything else an
//! argument names is the guest's (copying the host's `/usr/include` over the
//! guest's would be wrong). Within the checkout:
//!
//! - the working directory is created in the guest, and every file the tool
//!   creates or changes under it comes back;
//! - an argument naming an existing file or directory is copied in, and changes
//!   to it come back;
//! - an argument naming a path that does not exist yet, outside the working
//!   directory, comes back if the tool creates it.
//!
//! An argument of the form `--option=path` is read the same way. The tool's
//! standard output and standard error are replayed byte for byte, and this
//! command exits with the tool's status.
//!
//! A run that takes longer than `COOKBOOK_GUEST_TIMEOUT` seconds (default
//! [`DEFAULT_TIMEOUT`]) is stopped and fails with the end of the guest's console.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cookbook::{Error, Result};
use redoxer::guest::{self, GuestCommand};

/// How long a guest run may take, boot included, when `COOKBOOK_GUEST_TIMEOUT`
/// does not say.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A `repo guest-run` invocation.
#[derive(Debug)]
pub struct GuestRun {
    /// The installed guest build environment's image.
    guest: PathBuf,
    /// The command and its arguments, as the build script passed them.
    command: Vec<String>,
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
        let mut command = Vec::new();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--" => {
                    command.extend(args);
                    break;
                }
                "--guest" => match args.next() {
                    Some(value) if !value.is_empty() => guest = Some(PathBuf::from(value)),
                    _ => {
                        return Err(Error::Options(
                            "guest-run: --guest needs a path".to_string(),
                        ));
                    }
                },
                other => {
                    return Err(Error::Options(format!(
                        "guest-run: unexpected argument {other:?}; put the command after --"
                    )));
                }
            }
        }
        let guest =
            guest.ok_or_else(|| Error::Options("guest-run: --guest is required".to_string()))?;
        if command.is_empty() {
            return Err(Error::Options(
                "guest-run: no command given after --".to_string(),
            ));
        }
        Ok(Self { guest, command })
    }

    /// Runs the command in the guest, replays its output and exits with its status.
    ///
    /// # Errors
    /// Returns an error when the working directory or the checkout cannot be
    /// resolved, the working directory is outside the checkout, or the guest
    /// cannot run the command (see [`guest::GuestError`]). A failing command is
    /// not an error: the process exits with its status, so the build sees the
    /// tool's failure the way it would see a host tool's.
    #[allow(clippy::result_large_err)]
    pub fn run(&self) -> Result<()> {
        let root = canonical(&cookbook_root(), "resolving the cookbook checkout")?;
        let working_dir = canonical(Path::new("."), "resolving the working directory")?;
        if !working_dir.starts_with(&root) {
            return Err(Error::Options(format!(
                "guest-run: the working directory {} is outside the checkout {}, so the guest cannot share it",
                working_dir.display(),
                root.display()
            )));
        }

        let (program, args) = self
            .command
            .split_first()
            .expect("parse requires a command");
        let mut command = GuestCommand::new(&self.guest, &root, &working_dir, program)
            .args(args.iter().cloned())
            .timeout(timeout()?);
        for shared in args
            .iter()
            .filter_map(|arg| share(arg, &working_dir, &root))
        {
            command = match shared {
                Shared::Existing(path) => command.input(&path).output(path),
                Shared::Created(path) => command.output(path),
            };
        }

        let output = guest::run(&command).map_err(|err| Error::GuestRun(Box::new(err)))?;
        replay(&mut std::io::stdout(), &output.stdout)?;
        replay(&mut std::io::stderr(), &output.stderr)?;
        std::process::exit(output.outcome.shell_code());
    }
}

/// A checkout path an argument names.
#[derive(Debug, PartialEq, Eq)]
enum Shared {
    /// It exists: it goes in, and comes back if the tool changes it.
    Existing(PathBuf),
    /// It does not exist yet: it comes back if the tool creates it.
    Created(PathBuf),
}

/// The checkout path `arg` names, if it names one the working directory does not
/// already cover.
fn share(arg: &str, working_dir: &Path, root: &Path) -> Option<Shared> {
    let value = match arg.split_once('=') {
        Some((option, value)) if option.starts_with('-') => value,
        _ => arg,
    };
    if value.is_empty() || value.starts_with('-') {
        return None;
    }
    let path = working_dir.join(value);
    if let Ok(existing) = path.canonicalize() {
        return existing
            .starts_with(root)
            .then_some(Shared::Existing(existing));
    }
    // A new file under the working directory comes back with it.
    let parent = path.parent()?.canonicalize().ok()?;
    let created = parent.join(path.file_name()?);
    (created.starts_with(root) && !created.starts_with(working_dir))
        .then_some(Shared::Created(created))
}

#[allow(clippy::result_large_err)]
fn canonical(path: &Path, context: &'static str) -> Result<PathBuf> {
    path.canonicalize().map_err(|source| Error::Io {
        source,
        path: Some(path.to_path_buf()),
        context,
    })
}

#[allow(clippy::result_large_err)]
fn replay(out: &mut impl Write, data: &[u8]) -> Result<()> {
    out.write_all(data)
        .and_then(|()| out.flush())
        .map_err(|source| Error::Io {
            source,
            path: None,
            context: "replaying the guest tool's output",
        })
}

/// The run timeout `COOKBOOK_GUEST_TIMEOUT` asks for, in seconds.
#[allow(clippy::result_large_err)]
fn timeout() -> Result<Duration> {
    match std::env::var("COOKBOOK_GUEST_TIMEOUT") {
        Ok(seconds) => seconds
            .trim()
            .parse()
            .map(Duration::from_secs)
            .map_err(|_| {
                Error::Options(format!(
                    "guest-run: COOKBOOK_GUEST_TIMEOUT={seconds:?} is not a number of seconds"
                ))
            }),
        Err(_) => Ok(DEFAULT_TIMEOUT),
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
            ["--guest", "/tmp/guest.img", "--", "mdbook", "build"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(run.guest, PathBuf::from("/tmp/guest.img"));
        assert_eq!(run.command, ["mdbook", "build"]);
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

    /// A checkout laid out like a recipe: source and build directories, and a
    /// file outside the checkout.
    fn checkout() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("cookbook");
        let build = root.join("recipes/x/target/build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(root.join("recipes/x/source")).unwrap();
        std::fs::write(root.join("recipes/x/source/in.gperf"), b"%%\n").unwrap();
        std::fs::write(base.join("outside.txt"), b"").unwrap();
        (temp, base, root, build)
    }

    #[test]
    fn existing_checkout_paths_travel_both_ways() {
        let (_temp, _base, root, build) = checkout();
        let input = root.join("recipes/x/source/in.gperf");
        assert_eq!(
            share("../../source/in.gperf", &build, &root),
            Some(Shared::Existing(input.clone()))
        );
        assert_eq!(
            share(&format!("--input={}", input.display()), &build, &root),
            Some(Shared::Existing(input))
        );
    }

    #[test]
    fn a_new_path_outside_the_working_directory_comes_back() {
        let (_temp, _base, root, build) = checkout();
        assert_eq!(
            share("--output-file=../../source/out.h", &build, &root),
            Some(Shared::Created(root.join("recipes/x/source/out.h")))
        );
        // Under the working directory it already comes back with the directory.
        assert_eq!(share("out.h", &build, &root), None);
    }

    #[test]
    fn options_and_paths_outside_the_checkout_stay_as_they_are() {
        let (_temp, base, root, build) = checkout();
        assert_eq!(share("-L", &build, &root), None);
        assert_eq!(share("ANSI-C", &build, &root), None);
        assert_eq!(
            share(base.join("outside.txt").to_str().unwrap(), &build, &root),
            None
        );
        assert_eq!(share("/usr/include", &build, &root), None);
        assert_eq!(share("--flag=", &build, &root), None);
    }
}
