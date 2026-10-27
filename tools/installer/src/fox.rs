//! Ferric Oxide: building an image root through `fox`.
//!
//! The installer places the image's packages with fox's store, so they
//! are in fox's database already. A [`FoxImage`] then runs the host `fox`
//! binary against the image root:
//!
//! 1. With a package set (`fox-packages/1`), `fox --root <root> sync
//!    --config <set> --target <image target>` installs its packages from
//!    their registries or git sources and writes `fox.lock` beside the set;
//!    `--locked` makes the build fail unless the lock already pins every
//!    package, for reproducible images. A level (`--level v3` or `v4`)
//!    picks the x86-64 variants the image's packages are built and
//!    downloaded for; fox cannot see the image's CPU, so without one it
//!    uses the set's policy, else the v3 baseline.
//! 2. With an SBOM path, `fox --root <root> sbom --system --name <name>
//!    --output <path>` describes every package in the finished root.
//!
//! Each step is a separate `fox` process, so the installer depends on
//! fox's command line and not on its crates.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

/// How to run `fox` against an image root; see the module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoxImage {
    /// The host `fox` binary.
    pub fox: PathBuf,
    /// The package set to sync into the image, if any.
    pub packages: Option<PathBuf>,
    /// Refuse to resolve anything the lock beside the set does not pin.
    pub locked: bool,
    /// The target triple packages are installed for.
    pub target: String,
    /// The x86-64 level (`v3` or `v4`) packages are installed for, passed
    /// to `fox sync --level`; fox ignores it for other targets.
    pub level: Option<String>,
    /// The system name recorded in the SBOM.
    pub name: String,
    /// Where to write the image SBOM, if anywhere.
    pub sbom: Option<PathBuf>,
}

/// One `fox` step of a [`FoxImage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FoxStep {
    /// `fox sync`.
    Sync,
    /// `fox sbom --system`.
    Sbom,
}

impl fmt::Display for FoxStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Sync => "sync",
            Self::Sbom => "sbom",
        })
    }
}

/// Why building the image through `fox` failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum FoxError {
    /// `fox` could not be started.
    Spawn {
        /// The step.
        step: FoxStep,
        /// The binary.
        fox: PathBuf,
        /// Why it could not start.
        source: io::Error,
    },
    /// `fox` ran and failed; it printed its reasons.
    Failed {
        /// The step.
        step: FoxStep,
        /// Its exit status.
        status: ExitStatus,
    },
}

impl fmt::Display for FoxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { step, fox, .. } => {
                write!(f, "cannot run {} for fox {step}", fox.display())
            }
            Self::Failed { step, status } => write!(f, "fox {step} failed ({status})"),
        }
    }
}

impl std::error::Error for FoxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::Failed { .. } => None,
        }
    }
}

impl FoxImage {
    /// Runs the steps against `root`, in order, stopping at the first that
    /// fails.
    ///
    /// # Errors
    ///
    /// [`FoxError::Spawn`] if `fox` cannot be started and
    /// [`FoxError::Failed`] if a step exits unsuccessfully.
    pub fn run(&self, root: &Path) -> Result<(), FoxError> {
        for (step, args) in self.steps() {
            self.run_step(root, step, &args)?;
        }
        Ok(())
    }

    /// Each step and its arguments after `fox --root <root>`.
    fn steps(&self) -> Vec<(FoxStep, Vec<String>)> {
        let mut steps = Vec::new();
        if let Some(packages) = &self.packages {
            let mut args = vec![
                "sync".into(),
                "--config".into(),
                packages.display().to_string(),
                "--target".into(),
                self.target.clone(),
            ];
            if let Some(level) = &self.level {
                args.push("--level".into());
                args.push(level.clone());
            }
            if self.locked {
                args.push("--locked".into());
            }
            steps.push((FoxStep::Sync, args));
        }
        if let Some(sbom) = &self.sbom {
            steps.push((
                FoxStep::Sbom,
                vec![
                    "sbom".into(),
                    "--system".into(),
                    "--name".into(),
                    self.name.clone(),
                    "--output".into(),
                    sbom.display().to_string(),
                ],
            ));
        }
        steps
    }

    fn run_step(&self, root: &Path, step: FoxStep, args: &[String]) -> Result<(), FoxError> {
        println!("fox {step} in {}", root.display());
        let status = Command::new(&self.fox)
            .arg("--root")
            .arg(root)
            .args(args)
            .status()
            .map_err(|source| FoxError::Spawn {
                step,
                fox: self.fox.clone(),
                source,
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(FoxError::Failed { step, status })
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Serializes the tests that write a script and run it: a process
    /// another test forks while a script is still open for writing holds
    /// that descriptor until it execs, and running the script meanwhile
    /// fails with `ETXTBSY`.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SPAWN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A fake `fox` that logs its arguments and fails on `fail_on`.
    fn fake_fox(dir: &Path, fail_on: &str) -> PathBuf {
        let path = dir.join("fox");
        let log = dir.join("log");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$3\" in {fail_on}) exit 3;; esac\n",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn image(fox: PathBuf) -> FoxImage {
        FoxImage {
            fox,
            packages: None,
            locked: false,
            target: "x86_64-unknown-redox".into(),
            level: None,
            name: "ferric-oxide-desktop".into(),
            sbom: None,
        }
    }

    fn log(dir: &Path) -> String {
        fs::read_to_string(dir.join("log")).unwrap()
    }

    #[test]
    fn without_a_package_set_or_sbom_nothing_runs() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let fox = image(fake_fox(dir.path(), "none"));
        fox.run(Path::new("/image")).unwrap();
        assert!(!dir.path().join("log").exists());
    }

    #[test]
    fn a_package_set_is_synced_for_the_image_target_then_described() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let fox = FoxImage {
            packages: Some("config/packages.toml".into()),
            locked: true,
            level: Some("v4".into()),
            sbom: Some("build/image.cdx.json".into()),
            ..image(fake_fox(dir.path(), "none"))
        };
        fox.run(Path::new("/image")).unwrap();
        assert_eq!(
            log(dir.path()),
            "--root /image sync --config config/packages.toml --target x86_64-unknown-redox --level v4 --locked\n\
             --root /image sbom --system --name ferric-oxide-desktop --output build/image.cdx.json\n"
        );
    }

    #[test]
    fn a_failed_step_stops_the_build() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let fox = FoxImage {
            packages: Some("packages.toml".into()),
            sbom: Some("image.cdx.json".into()),
            ..image(fake_fox(dir.path(), "sync"))
        };
        let error = fox.run(Path::new("/image")).unwrap_err();
        assert!(matches!(
            error,
            FoxError::Failed {
                step: FoxStep::Sync,
                ..
            }
        ));
        // The SBOM is not written for a root that did not sync.
        assert!(!log(dir.path()).contains("sbom"));

        let missing = FoxImage {
            sbom: Some("image.cdx.json".into()),
            ..image(dir.path().join("no-such-fox"))
        };
        assert!(matches!(
            missing.run(Path::new("/image")),
            Err(FoxError::Spawn {
                step: FoxStep::Sbom,
                ..
            })
        ));
    }
}
