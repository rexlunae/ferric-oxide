//! Live kernel tracing from the host in trace mode.
//!
//! A trace-mode run backs guest RAM with a shared file under `/dev/shm`. When the trace
//! kernel logs `trace: host rings at phys 0x<hex>, <n> CPUs x <KiB> KiB`, the harness starts
//!
//! ```text
//! ferric-trace follow --guest-ram <file> --phys 0x<hex> --ram-mib <MiB> --out <run>/trace --live
//! ```
//!
//! which maps the file read-only and appends the rings' records to an ordinary capture
//! directory, so `ferric-trace text`, `counts`, `perfetto` and `otlp` read it. Nothing runs
//! in the guest to get the records out. `follow` stops on SIGINT and flushes; the harness
//! stops it after QEMU has exited, so the last records are kept, and removes the RAM file
//! afterwards.
//!
//! When `follow` has stopped, [`CaptureFiles::read`] notes whether its capture directory is
//! whole (no `INCOMPLETE` marker, a `stats.end`), for the verdict's `trace` check.
//!
//! The `ferric-trace` binary is found with `--ferric-trace`, on `PATH`, or as
//! `target/<profile>/ferric-trace` in the checkout (the newest). It is only needed in trace
//! mode.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ferric_trace::capture;
use rustix::process::{Pid, Signal, kill_process};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::observe::HostRings;

/// Finds `ferric-trace`: `explicit`, then `PATH`, then the newest
/// `<repo>/target/*/ferric-trace`.
///
/// # Errors
///
/// [`Error::FerricTraceNotFound`] with every place searched.
pub fn find_ferric_trace(
    explicit: Option<&Path>,
    repo_root: Option<&Path>,
) -> Result<PathBuf, Error> {
    let mut searched = Vec::new();
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        searched.push(path.to_path_buf());
        return Err(Error::FerricTraceNotFound { searched });
    }
    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join("ferric-trace");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        searched.push(PathBuf::from("$PATH"));
    }
    if let Some(root) = repo_root {
        let target = root.join("target");
        let newest = fs::read_dir(&target)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("ferric-trace"))
            .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
            .max();
        if let Some((_, path)) = newest {
            return Ok(path);
        }
        searched.push(target.join("*").join("ferric-trace"));
    }
    Err(Error::FerricTraceNotFound { searched })
}

/// Checks that `binary` has the `follow` subcommand (from the trace-host work) by running
/// `binary follow --help`, so a trace-mode run fails before the guest boots rather than
/// after the kernel announces its rings.
///
/// # Errors
///
/// [`Error::Spawn`] when the program cannot run and [`Error::FollowUnsupported`] when it
/// rejects `follow`.
pub fn check_follow(binary: &Path) -> Result<(), Error> {
    let status = Command::new(binary)
        .args(["follow", "--help"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| Error::Spawn {
            program: binary.to_path_buf(),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::FollowUnsupported {
            program: binary.to_path_buf(),
            code: status.code(),
        })
    }
}

/// The arguments of `ferric-trace follow` for `rings` in the RAM file `ram`.
pub fn follow_args(ram: &Path, rings: HostRings, ram_mib: u64, out: &Path) -> Vec<OsString> {
    vec![
        "follow".into(),
        "--guest-ram".into(),
        ram.into(),
        "--phys".into(),
        format!("{:#x}", rings.phys).into(),
        "--ram-mib".into(),
        ram_mib.to_string().into(),
        "--out".into(),
        out.into(),
        "--live".into(),
    ]
}

/// How a `follow` process ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowExit {
    /// Its exit code, when it exited normally.
    pub code: Option<i32>,
    /// It ignored SIGINT and was killed.
    pub killed: bool,
}

/// What a capture directory held once `follow` had stopped: whether it is whole.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureFiles {
    /// The `INCOMPLETE` marker is still there: `follow` failed or was killed.
    pub incomplete: bool,
    /// `stats.end` was written, which `follow` does only when it reaches its end.
    pub stats_end: bool,
}

impl CaptureFiles {
    /// Looks at the capture directory `dir` (a missing directory has neither file).
    pub fn read(dir: &Path) -> Self {
        Self {
            incomplete: dir.join(capture::INCOMPLETE).exists(),
            stats_end: dir.join(capture::STATS_END).is_file(),
        }
    }

    /// Whether the capture is whole: no `INCOMPLETE` marker, and a `stats.end`.
    pub fn complete(self) -> bool {
        !self.incomplete && self.stats_end
    }
}

/// A running `ferric-trace follow`.
#[derive(Debug)]
pub struct Follow {
    child: Child,
    /// The capture directory it writes.
    pub out: PathBuf,
}

impl Follow {
    /// Starts `binary follow ...`, appending its output to `log`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the log cannot be opened and [`Error::Spawn`] when the program
    /// cannot start.
    pub fn spawn(
        binary: &Path,
        ram: &Path,
        rings: HostRings,
        ram_mib: u64,
        out: &Path,
        log: &Path,
    ) -> Result<Self, Error> {
        let open = || {
            File::options()
                .create(true)
                .append(true)
                .open(log)
                .map_err(|source| Error::Io {
                    path: log.to_path_buf(),
                    source,
                })
        };
        let child = Command::new(binary)
            .args(follow_args(ram, rings, ram_mib, out))
            .stdin(Stdio::null())
            .stdout(open()?)
            .stderr(open()?)
            .spawn()
            .map_err(|source| Error::Spawn {
                program: binary.to_path_buf(),
                source,
            })?;
        Ok(Self {
            child,
            out: out.to_path_buf(),
        })
    }

    /// Whether it is still running.
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Sends SIGINT so it flushes and exits; kills it if it has not exited after `grace`.
    pub fn stop(mut self, grace: Duration) -> FollowExit {
        if let Ok(Some(status)) = self.child.try_wait() {
            return FollowExit {
                code: status.code(),
                killed: false,
            };
        }
        // It may exit between the check and the signal; then there is nothing to stop.
        kill_process(Pid::from_child(&self.child), Signal::INT).ok();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return FollowExit {
                    code: status.code(),
                    killed: false,
                };
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.child.kill().ok();
        let code = self.child.wait().ok().and_then(|s| s.code());
        FollowExit { code, killed: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn arguments() {
        let rings = HostRings {
            phys: 0x7a00_0000,
            cpus: 4,
            kib: 4096,
        };
        let args = follow_args(
            Path::new("/dev/shm/ferric-harness-x"),
            rings,
            2048,
            Path::new("/r/trace"),
        );
        let args: Vec<_> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "follow",
                "--guest-ram",
                "/dev/shm/ferric-harness-x",
                "--phys",
                "0x7a000000",
                "--ram-mib",
                "2048",
                "--out",
                "/r/trace",
                "--live"
            ]
        );
    }

    #[test]
    fn capture_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("trace");
        assert_eq!(CaptureFiles::read(&dir), CaptureFiles::default());
        fs::create_dir(&dir).expect("mkdir");
        fs::write(dir.join("INCOMPLETE"), "").expect("write");
        let partial = CaptureFiles::read(&dir);
        assert!(partial.incomplete && !partial.stats_end && !partial.complete());
        fs::write(dir.join("stats.end"), "").expect("write");
        fs::remove_file(dir.join("INCOMPLETE")).expect("remove");
        assert!(CaptureFiles::read(&dir).complete());
    }

    #[test]
    fn discovery() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("nope");
        assert!(matches!(
            find_ferric_trace(Some(&missing), None),
            Err(Error::FerricTraceNotFound { .. })
        ));
        let release = tmp.path().join("target/release");
        fs::create_dir_all(&release).expect("mkdir");
        let binary = release.join("ferric-trace");
        fs::write(&binary, "").expect("write");
        // PATH may hold a real ferric-trace; the checkout's is found when it does not.
        let found = find_ferric_trace(None, Some(tmp.path())).expect("found");
        assert!(found.ends_with("ferric-trace"));
        assert_eq!(
            find_ferric_trace(Some(&binary), None).expect("explicit"),
            binary
        );
    }

    #[test]
    fn follow_support() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = |name: &str, body: &str| {
            let path = tmp.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        };
        let new = script("new", "[ \"$1 $2\" = 'follow --help' ]");
        check_follow(&new).expect("follow is supported");
        // A ferric-trace from before the trace-host work rejects the subcommand (clap: 2).
        let old = script("old", "echo \"unrecognized subcommand '$1'\" >&2; exit 2");
        assert!(matches!(
            check_follow(&old),
            Err(Error::FollowUnsupported { code: Some(2), .. })
        ));
        assert!(matches!(
            check_follow(&tmp.path().join("absent")),
            Err(Error::Spawn { .. })
        ));
    }

    #[test]
    fn stop_interrupts_and_kills() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = |name: &str, body: &str| {
            let path = tmp.path().join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        };
        let rings = HostRings {
            phys: 0x1000,
            cpus: 1,
            kib: 4,
        };
        let log = tmp.path().join("follow.log");
        let polite = script(
            "polite",
            "echo \"$@\"; trap 'echo flushed; exit 0' INT; while :; do sleep 0.05; done",
        );
        let mut follow =
            Follow::spawn(&polite, Path::new("/ram"), rings, 64, tmp.path(), &log).expect("spawn");
        thread::sleep(Duration::from_millis(200));
        assert!(follow.running());
        let exit = follow.stop(Duration::from_secs(5));
        assert_eq!(
            exit,
            FollowExit {
                code: Some(0),
                killed: false
            }
        );
        let text = fs::read_to_string(&log).expect("log");
        assert!(text.contains("follow --guest-ram /ram --phys 0x1000"));
        assert!(text.contains("flushed"));

        let stubborn = script("stubborn", "trap '' INT; while :; do sleep 0.05; done");
        let follow = Follow::spawn(&stubborn, Path::new("/ram"), rings, 64, tmp.path(), &log)
            .expect("spawn");
        thread::sleep(Duration::from_millis(100));
        assert!(follow.stop(Duration::from_millis(200)).killed);
    }
}
