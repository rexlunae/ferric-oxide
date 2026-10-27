//! Unlocking the volumes of a [`crypttab`](crate::crypttab) at boot (Cargo feature `unlock`,
//! features.toml `fs.unlock_at_boot`).
//!
//! For each volume in order, [`unlock_all`] does three things:
//!
//! 1. It refuses the volume if `/scheme/NAME` is already served, since a volume of that name
//!    could not be told apart from another process's scheme.
//! 2. It starts `ferric-blockd` with the volume's steps and waits for the daemon's
//!    [`ferric_ready`] line, which it sends only once it has registered `/scheme/NAME` itself.
//!    If the daemon exits first (a wrong key, a missing device, a name taken in the meantime)
//!    or does not answer in time, the volume has failed, and a daemon still running is killed
//!    so that it cannot serve the volume after the failure was reported.
//! 3. If the volume has a file system, it does the same for that file system's daemon on
//!    `/scheme/NAME` and its scheme.
//!
//! The daemons keep running after `ferric-unlock` exits and serve the volumes for the rest
//! of the boot. A volume that fails does not stop the others, and the result for each is
//! returned. How the daemons are started and watched is a [`Launcher`], so the order and
//! the error handling are tested on the host. [`Processes`] is the real one.

use std::fs;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::crypttab::Volume;

/// Where `ferric-blockd` is installed.
pub const BLOCKD: &str = "/usr/bin/ferric-blockd";

/// Starts daemons and waits for them to serve their schemes.
pub trait Launcher {
    /// A started daemon.
    type Child;
    /// Why starting or waiting failed.
    type Error: std::error::Error + 'static;

    /// Checks that nothing serves `/scheme/<scheme>` yet.
    ///
    /// # Errors
    ///
    /// When the scheme is already served.
    fn check_free(&mut self, scheme: &str) -> Result<(), Self::Error>;

    /// Starts `program` with `args`, asking it for the [`ferric_ready`] handshake.
    ///
    /// # Errors
    ///
    /// When the program cannot be started.
    fn start(&mut self, program: &str, args: &[String]) -> Result<Self::Child, Self::Error>;

    /// Waits until `child` says it serves `/scheme/<scheme>`.
    ///
    /// # Errors
    ///
    /// When `child` exits or stops talking first, or does not answer in time. A child that
    /// is still running then has been stopped.
    fn wait_ready(&mut self, child: &mut Self::Child, scheme: &str) -> Result<(), Self::Error>;
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
    serve(launcher, BLOCKD, &volume.blockd_args(), &volume.spec.scheme)
        .map_err(UnlockError::Blockd)?;
    let Some(mount) = &volume.mount else {
        return Ok(None);
    };
    serve(
        launcher,
        mount.filesystem.program(),
        &mount.args(&volume.device()),
        &mount.scheme,
    )
    .map_err(UnlockError::Mount)?;
    Ok(Some(mount.scheme.clone()))
}

/// Starts one daemon for a scheme nothing serves yet, and waits for its handshake.
fn serve<L: Launcher>(
    launcher: &mut L,
    program: &str,
    args: &[String],
    scheme: &str,
) -> Result<(), L::Error> {
    launcher.check_free(scheme)?;
    let mut child = launcher.start(program, args)?;
    launcher.wait_ready(&mut child, scheme)
}

/// Why a daemon did not start or serve.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LaunchError {
    /// Something already serves the scheme, so a daemon started for it could not be told
    /// apart from it.
    #[error("/scheme/{scheme} is already served")]
    Taken {
        /// The scheme.
        scheme: String,
    },
    /// The program could not be started.
    #[error("cannot start {program}")]
    Spawn {
        /// The program.
        program: String,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// Reading the daemon's handshake, or waiting for it to exit, failed.
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
    /// The daemon closed its stdout, or wrote something other than the handshake, and was
    /// stopped.
    #[error("it did not say it serves /scheme/{scheme}, and was stopped")]
    NotReady {
        /// The scheme.
        scheme: String,
    },
    /// The daemon did not serve in time, and was stopped.
    #[error("/scheme/{scheme} was not served within {timeout:?}; the daemon was stopped")]
    Timeout {
        /// The scheme.
        scheme: String,
        /// How long was waited.
        timeout: Duration,
    },
}

/// A daemon started by [`Processes`], and the thread reading its handshake.
#[derive(Debug)]
pub struct Daemon {
    child: Child,
    /// The handshake: `true` for the ready line, `false` for anything else.
    ready: Receiver<io::Result<bool>>,
}

impl Daemon {
    /// The daemon's process ID.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Kills the daemon if it still runs, and reaps it.
    fn stop(&mut self) -> Result<(), LaunchError> {
        if self.child.try_wait().map_err(LaunchError::Wait)?.is_none() {
            // It may exit on its own between the check and the kill; reaping it below is
            // what matters.
            let _ = self.child.kill();
        }
        self.child.wait().map_err(LaunchError::Wait)?;
        Ok(())
    }

    /// After the handshake failed: how the daemon exited, if it does so on its own within
    /// `grace`, or else stops it.
    fn exit_status(&mut self, grace: Duration) -> Result<Option<ExitStatus>, LaunchError> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().map_err(LaunchError::Wait)? {
                return Ok(Some(status));
            }
            if start.elapsed() >= grace {
                self.stop()?;
                return Ok(None);
            }
            thread::sleep(POLL);
        }
    }
}

/// How often [`Daemon::exit_status`] checks whether the daemon has exited.
const POLL: Duration = Duration::from_millis(20);

/// How long a daemon that ended its handshake without the ready line may take to exit.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// Reads the handshake from `stdout` on a thread of its own, so that waiting for it can time
/// out.
fn read_handshake(stdout: ChildStdout) -> Receiver<io::Result<bool>> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let ready = ferric_ready::read_ready(&mut BufReader::new(stdout));
        // The receiver is gone only when the waiting side already gave up.
        let _ = send.send(ready);
    });
    receive
}

/// Starts daemons as child processes with the [`ferric_ready`] handshake on their stdout.
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
    type Child = Daemon;
    type Error = LaunchError;

    fn check_free(&mut self, scheme: &str) -> Result<(), LaunchError> {
        if fs::metadata(self.schemes.join(scheme)).is_ok() {
            return Err(LaunchError::Taken {
                scheme: scheme.to_owned(),
            });
        }
        Ok(())
    }

    fn start(&mut self, program: &str, args: &[String]) -> Result<Daemon, LaunchError> {
        // The daemons keep init's stderr (the boot log), read nothing, and write only the
        // handshake to stdout.
        let mut child = Command::new(program)
            .args(args)
            .env(ferric_ready::ENV, ferric_ready::STDOUT)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|source| LaunchError::Spawn {
                program: program.to_owned(),
                source,
            })?;
        let stdout = child
            .stdout
            .take()
            .expect("stdout is piped, so the child has one");
        Ok(Daemon {
            child,
            ready: read_handshake(stdout),
        })
    }

    fn wait_ready(&mut self, daemon: &mut Daemon, scheme: &str) -> Result<(), LaunchError> {
        let scheme = scheme.to_owned();
        match daemon.ready.recv_timeout(self.timeout) {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) | Err(RecvTimeoutError::Disconnected) => {
                match daemon.exit_status(EXIT_GRACE)? {
                    Some(status) => Err(LaunchError::Exited { scheme, status }),
                    None => Err(LaunchError::NotReady { scheme }),
                }
            }
            Ok(Err(err)) => {
                daemon.stop()?;
                Err(LaunchError::Wait(err))
            }
            Err(RecvTimeoutError::Timeout) => {
                daemon.stop()?;
                Err(LaunchError::Timeout {
                    scheme,
                    timeout: self.timeout,
                })
            }
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

    /// Records what it was asked to do. Schemes in `taken` are already served; daemons for
    /// schemes in `failing` never serve.
    #[derive(Default)]
    struct Fake {
        log: Vec<String>,
        taken: Vec<&'static str>,
        failing: Vec<&'static str>,
    }

    impl Launcher for Fake {
        type Child = usize;
        type Error = Refused;

        fn check_free(&mut self, scheme: &str) -> Result<(), Refused> {
            self.log.push(format!("check {scheme}"));
            if self.taken.contains(&scheme) {
                return Err(Refused(format!("taken {scheme}")));
            }
            Ok(())
        }

        fn start(&mut self, program: &str, args: &[String]) -> Result<usize, Refused> {
            self.log.push(format!("start {program} {}", args.join(" ")));
            Ok(self.log.len())
        }

        fn wait_ready(&mut self, _child: &mut usize, scheme: &str) -> Result<(), Refused> {
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
                "check a",
                "start /usr/bin/ferric-blockd a dev:/x slice:0:-",
                "wait a",
                "check a-fs",
                "start /usr/bin/ext4d /scheme/a a-fs",
                "wait a-fs",
                "check b",
                "start /usr/bin/ferric-blockd b dev:/y",
                "wait b",
                "check c",
                "start /usr/bin/ferric-blockd c dev:/z",
                "wait c",
                "check c-fs",
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
    fn a_scheme_that_is_already_served_starts_no_daemon() {
        let volumes = crypttab::parse(TABLE).expect("parses");
        let mut fake = Fake {
            taken: vec!["a", "c-fs"],
            ..Fake::default()
        };
        let outcomes = unlock_all(&volumes, &mut fake);
        assert!(
            !fake
                .log
                .iter()
                .any(|l| l.starts_with("start") && l.contains(" a "))
        );
        assert!(!fake.log.iter().any(|l| l.contains("ext4d /scheme/c")));
        assert!(
            fake.log
                .iter()
                .any(|l| l == "start /usr/bin/ferric-blockd c dev:/z")
        );
        assert!(matches!(
            &outcomes[0].result,
            Err(UnlockError::Blockd(Refused(s))) if s == "taken a"
        ));
        assert!(matches!(
            &outcomes[2].result,
            Err(UnlockError::Mount(Refused(s))) if s == "taken c-fs"
        ));
    }

    /// A stand-in for `/scheme`, removed when dropped.
    struct Schemes(PathBuf);

    impl Schemes {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("ferric-unlock-{name}-{}", std::process::id()));
            fs::create_dir_all(&dir).expect("a directory");
            Self(dir)
        }

        fn processes(&self) -> Processes {
            Processes {
                timeout: Duration::from_millis(300),
                schemes: self.0.clone(),
            }
        }
    }

    impl Drop for Schemes {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["-c".to_owned(), script.to_owned()]
    }

    /// Whether process `pid` is gone (`kill -0` fails once it has been reaped).
    fn gone(pid: u32) -> bool {
        !Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .expect("kill runs")
            .success()
    }

    #[test]
    fn the_handshake_is_what_counts_as_served() {
        let schemes = Schemes::new("ready");
        let mut processes = schemes.processes();
        // Only a daemon asked for the handshake answers it.
        let mut daemon = processes
            .start(
                "sh",
                &sh(r#"[ "$FERRIC_READY" = stdout ] && echo ready; exec sleep 5"#),
            )
            .expect("sh starts");
        assert!(processes.wait_ready(&mut daemon, "v").is_ok());
        daemon.stop().expect("stopped");
        // A scheme of that name appearing is not enough without the line.
        fs::write(schemes.0.join("v"), b"").expect("a stand-in scheme");
        let mut silent = processes
            .start("sleep", &["5".to_owned()])
            .expect("sleep starts");
        let pid = silent.id();
        assert!(matches!(
            processes.wait_ready(&mut silent, "v"),
            Err(LaunchError::Timeout { scheme, .. }) if scheme == "v"
        ));
        assert!(gone(pid), "a daemon that timed out is stopped");
    }

    #[test]
    fn a_scheme_that_exists_is_refused() {
        let schemes = Schemes::new("taken");
        let mut processes = schemes.processes();
        assert!(processes.check_free("data").is_ok());
        fs::write(schemes.0.join("data"), b"").expect("a stand-in scheme");
        assert!(matches!(
            processes.check_free("data"),
            Err(LaunchError::Taken { scheme }) if scheme == "data"
        ));
    }

    #[test]
    fn a_daemon_that_exits_or_says_something_else_is_reported() {
        let schemes = Schemes::new("exits");
        let mut processes = schemes.processes();
        let mut exits = processes.start("false", &[]).expect("false starts");
        assert!(matches!(
            processes.wait_ready(&mut exits, "never"),
            Err(LaunchError::Exited { scheme, status }) if scheme == "never" && !status.success()
        ));
        let mut chatty = processes
            .start("sh", &sh("echo hello; exec sleep 5"))
            .expect("sh starts");
        let pid = chatty.id();
        assert!(matches!(
            processes.wait_ready(&mut chatty, "never"),
            Err(LaunchError::NotReady { scheme }) if scheme == "never"
        ));
        assert!(gone(pid), "a daemon that did not notify is stopped");
        assert!(matches!(
            processes.start("/nonexistent/ferric-blockd", &[]),
            Err(LaunchError::Spawn { .. })
        ));
    }
}
