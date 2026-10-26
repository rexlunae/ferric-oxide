//! Unlocking the volumes of a [`crypttab`](crate::crypttab) at boot (Cargo feature `unlock`,
//! features.toml `fs.unlock_at_boot`).
//!
//! For each volume in order, [`unlock_all`] does three things:
//!
//! 1. It starts `ferric-blockd` with the volume's steps.
//! 2. It waits for `/scheme/NAME` to answer, or for the daemon to exit (a wrong key, a
//!    missing device).
//! 3. If the volume has a file system, it starts that file system's daemon on
//!    `/scheme/NAME` and waits for its scheme the same way.
//!
//! The daemons keep running after `ferric-unlock` exits and serve the volumes for the rest
//! of the boot. A volume that fails does not stop the others, and the result for each is
//! returned. How the daemons are started and watched is a [`Launcher`], so the order and
//! the error handling are tested on the host. [`Processes`] is the real one.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::crypttab::Volume;

/// Where `ferric-blockd` is installed.
pub const BLOCKD: &str = "/usr/bin/ferric-blockd";

/// Starts daemons and waits for the schemes they serve.
pub trait Launcher {
    /// A started daemon.
    type Child;
    /// Why starting or waiting failed.
    type Error: std::error::Error + 'static;

    /// Starts `program` with `args`.
    ///
    /// # Errors
    ///
    /// When the program cannot be started.
    fn start(&mut self, program: &str, args: &[String]) -> Result<Self::Child, Self::Error>;

    /// Waits until `/scheme/<scheme>` answers.
    ///
    /// # Errors
    ///
    /// When `child` exits first, or the scheme does not appear in time.
    fn wait_for(&mut self, child: &mut Self::Child, scheme: &str) -> Result<(), Self::Error>;
}

/// Why one volume was not unlocked or not mounted.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UnlockError<E: std::error::Error + 'static> {
    /// `ferric-blockd` did not serve the volume.
    #[error("ferric-blockd did not serve the volume")]
    Blockd(#[source] E),
    /// The volume was unlocked, but its file system was not mounted.
    #[error("the volume was unlocked, but its file system was not mounted")]
    Mount(#[source] E),
}

/// What happened to one volume.
#[derive(Debug)]
pub struct Outcome<E: std::error::Error + 'static> {
    /// The volume's scheme name.
    pub scheme: String,
    /// The scheme its file system was mounted as, if it has one and it was mounted.
    pub mounted: Option<String>,
    /// `Ok` when everything the line asked for is being served.
    pub result: Result<(), UnlockError<E>>,
}

/// Unlocks `volumes` in order with `launcher`, and returns what happened to each.
///
/// # Examples
///
/// ```no_run
/// use ferric_blockd::crypttab;
/// use ferric_blockd::unlock::{self, Processes};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let volumes = crypttab::parse(&std::fs::read_to_string(crypttab::PATH)?)?;
/// for outcome in unlock::unlock_all(&volumes, &mut Processes::default()) {
///     if let Err(err) = outcome.result {
///         eprintln!("{}: {err}", outcome.scheme);
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub fn unlock_all<L: Launcher>(volumes: &[Volume], launcher: &mut L) -> Vec<Outcome<L::Error>> {
    volumes
        .iter()
        .map(|volume| {
            let (mounted, result) = match unlock_one(volume, launcher) {
                Ok(mounted) => (mounted, Ok(())),
                Err(err) => (None, Err(err)),
            };
            Outcome {
                scheme: volume.spec.scheme.clone(),
                mounted,
                result,
            }
        })
        .collect()
}

/// Unlocks one volume and mounts its file system; returns the file system's scheme.
fn unlock_one<L: Launcher>(
    volume: &Volume,
    launcher: &mut L,
) -> Result<Option<String>, UnlockError<L::Error>> {
    let mut blockd = launcher
        .start(BLOCKD, &volume.blockd_args())
        .map_err(UnlockError::Blockd)?;
    launcher
        .wait_for(&mut blockd, &volume.spec.scheme)
        .map_err(UnlockError::Blockd)?;
    let Some(mount) = &volume.mount else {
        return Ok(None);
    };
    let mut daemon = launcher
        .start(mount.filesystem.program(), &mount.args(&volume.device()))
        .map_err(UnlockError::Mount)?;
    launcher
        .wait_for(&mut daemon, &mount.scheme)
        .map_err(UnlockError::Mount)?;
    Ok(Some(mount.scheme.clone()))
}

/// Why a daemon did not start or serve.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LaunchError {
    /// The program could not be started.
    #[error("cannot start {program}")]
    Spawn {
        /// The program.
        program: String,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// Waiting on the daemon failed.
    #[error("cannot wait for the daemon")]
    Wait(#[source] io::Error),
    /// The daemon exited before serving its scheme.
    #[error("it exited before serving /scheme/{scheme} ({status})")]
    Exited {
        /// The scheme it was to serve.
        scheme: String,
        /// How it exited.
        status: ExitStatus,
    },
    /// The scheme did not appear in time.
    #[error("/scheme/{scheme} did not appear within {timeout:?}")]
    Timeout {
        /// The scheme.
        scheme: String,
        /// How long was waited.
        timeout: Duration,
    },
}

/// Starts daemons as child processes, and polls for their schemes.
#[derive(Clone, Debug)]
pub struct Processes {
    /// How long a daemon may take to serve: unlocking LUKS2 runs a slow key derivation on
    /// purpose.
    pub timeout: Duration,
    /// Where schemes appear, normally `/scheme`.
    pub schemes: PathBuf,
}

impl Default for Processes {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            schemes: PathBuf::from("/scheme"),
        }
    }
}

impl Launcher for Processes {
    type Child = Child;
    type Error = LaunchError;

    fn start(&mut self, program: &str, args: &[String]) -> Result<Child, LaunchError> {
        // The daemons keep init's stdout and stderr (the boot log) and read nothing.
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|source| LaunchError::Spawn {
                program: program.to_owned(),
                source,
            })
    }

    fn wait_for(&mut self, child: &mut Child, scheme: &str) -> Result<(), LaunchError> {
        let path = self.schemes.join(scheme);
        let start = Instant::now();
        loop {
            if fs::metadata(&path).is_ok() {
                return Ok(());
            }
            if let Some(status) = child.try_wait().map_err(LaunchError::Wait)? {
                return Err(LaunchError::Exited {
                    scheme: scheme.to_owned(),
                    status,
                });
            }
            if start.elapsed() > self.timeout {
                return Err(LaunchError::Timeout {
                    scheme: scheme.to_owned(),
                    timeout: self.timeout,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;
    use crate::crypttab;

    /// Why the fake refused.
    #[derive(Debug, PartialEq, Eq)]
    struct Refused(String);

    impl fmt::Display for Refused {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "refused {}", self.0)
        }
    }

    impl std::error::Error for Refused {}

    /// Records what it was asked to do, and fails for the schemes in `failing`.
    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        failing: Vec<&'static str>,
    }

    impl Launcher for Fake {
        type Child = usize;
        type Error = Refused;

        fn start(&mut self, program: &str, args: &[String]) -> Result<usize, Refused> {
            self.log.push(format!("start {program} {}", args.join(" ")));
            Ok(self.log.len())
        }

        fn wait_for(&mut self, _child: &mut usize, scheme: &str) -> Result<(), Refused> {
            self.log.push(format!("wait {scheme}"));
            if self.failing.contains(&scheme) {
                return Err(Refused(scheme.to_owned()));
            }
            Ok(())
        }
    }

    const TABLE: &str = "\
a dev:/x slice:0:- ext4:a-fs
b dev:/y
c dev:/z ext4:c-fs
";

    #[test]
    fn each_volume_is_served_before_its_file_system_is_mounted() {
        let volumes = crypttab::parse(TABLE).expect("parses");
        let mut fake = Fake::default();
        let outcomes = unlock_all(&volumes, &mut fake);
        assert_eq!(
            fake.log,
            [
                "start /usr/bin/ferric-blockd a dev:/x slice:0:-",
                "wait a",
                "start /usr/bin/ext4d /scheme/a a-fs",
                "wait a-fs",
                "start /usr/bin/ferric-blockd b dev:/y",
                "wait b",
                "start /usr/bin/ferric-blockd c dev:/z",
                "wait c",
                "start /usr/bin/ext4d /scheme/c c-fs",
                "wait c-fs",
            ]
        );
        let mounted: Vec<_> = outcomes.iter().map(|o| o.mounted.as_deref()).collect();
        assert_eq!(mounted, [Some("a-fs"), None, Some("c-fs")]);
        assert!(outcomes.iter().all(|o| o.result.is_ok()));
    }

    #[test]
    fn a_volume_that_fails_does_not_stop_the_others() {
        let volumes = crypttab::parse(TABLE).expect("parses");
        let mut fake = Fake {
            failing: vec!["a", "c-fs"],
            ..Fake::default()
        };
        let outcomes = unlock_all(&volumes, &mut fake);
        // a's file system is not mounted on a volume that was never served.
        assert!(!fake.log.iter().any(|l| l.contains("a-fs")));
        assert!(matches!(
            &outcomes[0].result,
            Err(UnlockError::Blockd(Refused(s))) if s == "a"
        ));
        assert!(outcomes[1].result.is_ok());
        assert!(matches!(
            &outcomes[2].result,
            Err(UnlockError::Mount(Refused(s))) if s == "c-fs"
        ));
        assert_eq!(outcomes[2].mounted, None);
    }

    #[test]
    fn a_daemon_that_exits_or_never_serves_is_reported() {
        let dir = std::env::temp_dir().join(format!("ferric-unlock-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("a directory");
        let mut processes = Processes {
            timeout: Duration::from_millis(200),
            schemes: dir.clone(),
        };
        let mut exits = processes.start("false", &[]).expect("false starts");
        assert!(matches!(
            processes.wait_for(&mut exits, "never"),
            Err(LaunchError::Exited { scheme, status }) if scheme == "never" && !status.success()
        ));
        let mut sleeps = processes
            .start("sleep", &["5".to_owned()])
            .expect("sleep starts");
        assert!(matches!(
            processes.wait_for(&mut sleeps, "never"),
            Err(LaunchError::Timeout { .. })
        ));
        sleeps.kill().expect("killed");
        let _ = sleeps.wait();
        fs::write(dir.join("served"), b"").expect("a stand-in scheme");
        let mut serving = processes
            .start("sleep", &["5".to_owned()])
            .expect("sleep starts");
        assert!(processes.wait_for(&mut serving, "served").is_ok());
        serving.kill().expect("killed");
        let _ = serving.wait();
        assert!(matches!(
            processes.start("/nonexistent/ferric-blockd", &[]),
            Err(LaunchError::Spawn { .. })
        ));
        fs::remove_dir_all(&dir).expect("removed");
    }
}
