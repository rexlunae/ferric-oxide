//! Saving a capture on the target: draining every CPU's ring of the `kernel.trace` scheme into
//! a directory that [`Capture::load`](crate::capture::Capture::load) reads back.
//!
//! [`drain_scheme`] reads the scheme's `stats` file for the list of CPUs and saves it as
//! `stats.start`, drains each `cpu/<n>` file into `cpu<n>` until a read returns 0 bytes (the
//! ring is empty), then reads `stats` again and saves it as `stats.end`, so records dropped
//! while the capture ran are counted too. A read fails with `EAGAIN` while another reader is
//! draining the same CPU; that read is retried after a short wait, up to [`AGAIN_RETRIES`]
//! times in a row. Any other error stops the capture, naming the file.
//!
//! The capture is written to a hidden sibling of the output directory and renamed into place
//! only once every CPU has been drained, so a failed capture leaves nothing behind. The output
//! directory must not exist yet or be empty, and must not overlap the scheme path.
//!
//! The scheme's interface is [`ferric_trace::abi`].

use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use ferric_trace::abi::{self, LineError, StatsLine};

/// How many `EAGAIN` reads in a row a CPU file may give before the capture fails.
pub const AGAIN_RETRIES: u32 = 200;

/// The name of the `stats` snapshot taken before draining.
pub const STATS_START: &str = "stats.start";

/// The name of the `stats` snapshot taken after draining.
pub const STATS_END: &str = "stats.end";

/// How long to wait after an `EAGAIN` read.
const AGAIN_WAIT: Duration = Duration::from_millis(5);

/// The read size: many records at a time, and never less than [`abi::READ_MIN`].
const READ_BYTES: usize = 64 * 1024;

const _: () = assert!(READ_BYTES >= abi::READ_MIN);

/// Why a capture could not be saved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DrainError {
    /// A file could not be opened, read, created, written or renamed.
    #[error("{}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A line of `stats` does not parse.
    #[error("{}: bad line {line:?}", path.display())]
    Stats {
        /// The `stats` path.
        path: PathBuf,
        /// The line.
        line: String,
        /// Why it does not parse.
        #[source]
        source: LineError,
    },
    /// A CPU file stayed busy (`EAGAIN`) for [`AGAIN_RETRIES`] reads in a row.
    #[error("{}: still busy after {AGAIN_RETRIES} tries; is another reader draining it?", path.display())]
    Busy {
        /// The CPU file.
        path: PathBuf,
    },
    /// The output directory exists and is not empty, so it may hold an earlier capture.
    #[error("{}: already exists and is not empty", path.display())]
    OutputNotEmpty {
        /// The output directory.
        path: PathBuf,
    },
    /// The output directory is inside the scheme path, or holds it.
    #[error("{} overlaps the scheme path {}", out.display(), scheme.display())]
    Overlap {
        /// The output directory.
        out: PathBuf,
        /// The scheme path.
        scheme: PathBuf,
    },
}

/// What was saved for one CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuDrained {
    /// The CPU.
    pub cpu: u32,
    /// The bytes saved.
    pub bytes: u64,
    /// Records the CPU had dropped since boot when the capture started.
    pub lost_before: u64,
    /// Records the CPU dropped while the capture ran.
    pub lost_during: u64,
}

/// Drains every CPU of the trace scheme at `scheme` (normally `/scheme/kernel.trace`) into
/// directory `out`, which must not exist yet or be empty.
///
/// # Errors
///
/// [`DrainError::OutputNotEmpty`] or [`DrainError::Overlap`] for an unusable `out`,
/// [`DrainError::Io`] for a file that cannot be read or written, [`DrainError::Stats`] for a
/// `stats` line that does not parse, and [`DrainError::Busy`] for a CPU file that another
/// reader holds for too long. On any error `out` is left as it was.
pub fn drain_scheme(scheme: &Path, out: &Path) -> Result<Vec<CpuDrained>, DrainError> {
    check_output(scheme, out)?;
    let partial = partial_dir(out);
    // A leftover from a capture that was killed: nothing else writes there.
    let _ = fs::remove_dir_all(&partial);
    fs::create_dir_all(&partial).map_err(io_error(&partial))?;
    let result = drain_into(scheme, &partial).and_then(|drained| {
        // A rename cannot replace a directory everywhere, so an empty `out` goes first.
        if out.exists() {
            fs::remove_dir(out).map_err(io_error(out))?;
        }
        fs::rename(&partial, out).map_err(io_error(out))?;
        Ok(drained)
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&partial);
    }
    result
}

/// Refuses an `out` that holds files or overlaps `scheme`.
fn check_output(scheme: &Path, out: &Path) -> Result<(), DrainError> {
    if let Ok(mut entries) = fs::read_dir(out) {
        if entries.next().is_some() {
            return Err(DrainError::OutputNotEmpty {
                path: out.to_owned(),
            });
        }
    } else if out.exists() {
        return Err(DrainError::OutputNotEmpty {
            path: out.to_owned(),
        });
    }
    let (out_abs, scheme_abs) = (absolute(out), absolute(scheme));
    if out_abs.starts_with(&scheme_abs) || scheme_abs.starts_with(&out_abs) {
        return Err(DrainError::Overlap {
            out: out.to_owned(),
            scheme: scheme.to_owned(),
        });
    }
    Ok(())
}

/// `path` made absolute, with symbolic links in its existing part resolved.
fn absolute(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    // The path does not exist yet: resolve its parent and add the name.
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => absolute(parent).join(name),
        _ => std::path::absolute(path).unwrap_or_else(|_| path.to_owned()),
    }
}

/// The hidden sibling of `out` that a capture is written to first.
fn partial_dir(out: &Path) -> PathBuf {
    let name = out
        .file_name()
        .map_or_else(|| "capture".into(), |n| n.to_string_lossy().into_owned());
    out.with_file_name(format!(".{name}.partial"))
}

/// Saves the start snapshot, every CPU and the end snapshot into `dir`.
fn drain_into(scheme: &Path, dir: &Path) -> Result<Vec<CpuDrained>, DrainError> {
    let start = snapshot(scheme, &dir.join(STATS_START))?;
    let mut drained = Vec::with_capacity(start.len());
    for line in &start {
        let from = scheme.join(abi::CPU_DIR).join(line.cpu.to_string());
        let to = dir.join(format!("cpu{}", line.cpu));
        let mut source = File::open(&from).map_err(io_error(&from))?;
        let mut sink = File::create(&to).map_err(io_error(&to))?;
        let bytes = drain(&mut source, &from, &mut sink, &to, AGAIN_WAIT)?;
        drained.push(CpuDrained {
            cpu: line.cpu,
            bytes,
            lost_before: line.lost,
            lost_during: 0,
        });
    }
    let end = snapshot(scheme, &dir.join(STATS_END))?;
    count_losses(&mut drained, &end);
    Ok(drained)
}

/// Sets each CPU's `lost_during` from the `stats` snapshot taken after draining.
fn count_losses(drained: &mut [CpuDrained], end: &[StatsLine]) {
    for cpu in drained {
        if let Some(line) = end.iter().find(|l| l.cpu == cpu.cpu) {
            cpu.lost_during = line.lost.saturating_sub(cpu.lost_before);
        }
    }
}

/// Reads the scheme's `stats`, saves it at `to`, and parses it.
fn snapshot(scheme: &Path, to: &Path) -> Result<Vec<StatsLine>, DrainError> {
    let path = scheme.join(abi::STATS_PATH);
    let text = fs::read_to_string(&path).map_err(io_error(&path))?;
    let lines = text
        .lines()
        .map(|line| {
            StatsLine::parse(line).map_err(|source| DrainError::Stats {
                path: path.clone(),
                line: line.to_owned(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    fs::write(to, &text).map_err(io_error(to))?;
    Ok(lines)
}

/// Copies `source` (at `from`) to `sink` (at `to`) until a read returns 0 bytes, retrying
/// `EAGAIN` and `EINTR` reads, and returns the bytes copied.
fn drain(
    source: &mut impl Read,
    from: &Path,
    sink: &mut impl Write,
    to: &Path,
    wait: Duration,
) -> Result<u64, DrainError> {
    let mut buf = vec![0u8; READ_BYTES];
    let mut total = 0u64;
    let mut busy = 0u32;
    loop {
        match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                busy = 0;
                sink.write_all(&buf[..n]).map_err(io_error(to))?;
                total += n as u64;
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                busy += 1;
                if busy >= AGAIN_RETRIES {
                    return Err(DrainError::Busy {
                        path: from.to_owned(),
                    });
                }
                thread::sleep(wait);
            }
            Err(err) => return Err(io_error(from)(err)),
        }
    }
    sink.flush().map_err(io_error(to))?;
    Ok(total)
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> DrainError + '_ {
    move |source| DrainError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Capture;
    use crate::capture::tests::bytes;
    use ferric_trace::kernel::{SYSCALL_ENTER, SYSCALL_EXIT};

    /// A reader that gives `results` in turn, then 0 bytes.
    struct Script(Vec<io::Result<Vec<u8>>>);

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let bytes = self.0.remove(0)?;
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    fn again() -> io::Result<Vec<u8>> {
        Err(io::Error::from(ErrorKind::WouldBlock))
    }

    fn run(script: Vec<io::Result<Vec<u8>>>) -> Result<(u64, Vec<u8>), DrainError> {
        let mut sink = Vec::new();
        let n = drain(
            &mut Script(script),
            Path::new("cpu/0"),
            &mut sink,
            Path::new("cpu0"),
            Duration::ZERO,
        )?;
        Ok((n, sink))
    }

    #[test]
    fn retries_busy_and_interrupted_reads() {
        let script = vec![
            again(),
            Ok(vec![1, 2, 3]),
            Err(io::Error::from(ErrorKind::Interrupted)),
            again(),
            again(),
            Ok(vec![4]),
        ];
        assert_eq!(run(script).expect("drains"), (4, vec![1, 2, 3, 4]));
    }

    #[test]
    fn gives_up_on_a_cpu_that_stays_busy() {
        let script = (0..AGAIN_RETRIES).map(|_| again()).collect();
        assert!(matches!(run(script), Err(DrainError::Busy { .. })));
        // One fewer busy read in a row is fine.
        let mut script: Vec<_> = (0..AGAIN_RETRIES - 1).map(|_| again()).collect();
        script.push(Ok(vec![9]));
        assert_eq!(run(script).expect("drains").0, 1);
    }

    #[test]
    fn other_errors_stop_the_capture() {
        let script = vec![
            Ok(vec![1]),
            Err(io::Error::from(ErrorKind::PermissionDenied)),
        ];
        let err = run(script).expect_err("fails");
        assert!(
            matches!(&err, DrainError::Io { path, source }
                if path == Path::new("cpu/0") && source.kind() == ErrorKind::PermissionDenied),
            "{err:?}"
        );
    }

    #[test]
    fn drains_every_cpu_stats_lists() {
        let scheme = stand_in_scheme();
        let out = tempfile::tempdir().expect("temp dir");
        let saved = out.path().join("capture");
        let drained = drain_scheme(scheme.path(), &saved).expect("drains");
        assert_eq!(
            drained,
            [
                CpuDrained {
                    cpu: 0,
                    bytes: 56,
                    lost_before: 0,
                    lost_during: 0
                },
                CpuDrained {
                    cpu: 2,
                    bytes: 32,
                    lost_before: 5,
                    lost_during: 0
                }
            ]
        );
        let capture = Capture::load(std::slice::from_ref(&saved)).expect("loads back");
        assert_eq!(capture.cpus(), [0, 2]);
        assert_eq!(capture.entries().len(), 2);
        assert!(saved.join(STATS_START).is_file() && saved.join(STATS_END).is_file());
        assert!(!partial_dir(&saved).exists());

        // An empty directory may be the output; a non-empty one, such as the last capture,
        // may not.
        let empty = out.path().join("empty");
        fs::create_dir(&empty).expect("mkdir");
        drain_scheme(scheme.path(), &empty).expect("drains into an empty directory");
        assert!(matches!(
            drain_scheme(scheme.path(), &saved),
            Err(DrainError::OutputNotEmpty { .. })
        ));
    }

    #[test]
    fn a_failed_capture_leaves_the_output_alone() {
        let scheme = stand_in_scheme();
        let out = tempfile::tempdir().expect("temp dir");
        let saved = out.path().join("capture");
        fs::remove_file(scheme.path().join("cpu/2")).expect("remove");
        assert!(matches!(
            drain_scheme(scheme.path(), &saved),
            Err(DrainError::Io { path, .. }) if path.ends_with("cpu/2")
        ));
        assert!(!saved.exists());
        assert!(!partial_dir(&saved).exists());

        fs::write(scheme.path().join("stats"), "cpu zero\n").expect("write");
        assert!(matches!(
            drain_scheme(scheme.path(), &saved),
            Err(DrainError::Stats { .. })
        ));
        assert!(!saved.exists());
    }

    #[test]
    fn the_output_must_not_overlap_the_scheme() {
        let scheme = stand_in_scheme();
        for out in [
            scheme.path().to_owned(),
            scheme.path().join("cpu"),
            scheme.path().join("new"),
            scheme.path().parent().expect("a parent").to_owned(),
        ] {
            assert!(
                matches!(
                    check_output(scheme.path(), &out),
                    Err(DrainError::Overlap { .. } | DrainError::OutputNotEmpty { .. })
                ),
                "{}",
                out.display()
            );
        }
        // Not existing yet, so only the overlap can refuse them.
        for out in [scheme.path().join("new"), scheme.path().join("cpu/new")] {
            assert!(matches!(
                check_output(scheme.path(), &out),
                Err(DrainError::Overlap { .. })
            ));
        }
        let elsewhere = tempfile::tempdir().expect("temp dir");
        check_output(scheme.path(), &elsewhere.path().join("capture")).expect("usable");
    }

    #[test]
    fn counts_losses_during_the_capture() {
        let cpu = |cpu, lost_before| CpuDrained {
            cpu,
            bytes: 8,
            lost_before,
            lost_during: 0,
        };
        let mut drained = [cpu(0, 0), cpu(1, 4), cpu(2, 9)];
        let end = ["cpu 0 pending 0 lost 3", "cpu 1 pending 9 lost 4"]
            .map(|line| StatsLine::parse(line).expect("valid"));
        count_losses(&mut drained, &end);
        // CPU 2 is missing from the end snapshot, so nothing is known to have been lost.
        let during: Vec<_> = drained.iter().map(|c| c.lost_during).collect();
        assert_eq!(during, [3, 0, 0]);
    }

    /// A stand-in for the scheme: stats and one file per CPU, CPU 2 not numbered next.
    fn stand_in_scheme() -> tempfile::TempDir {
        let scheme = tempfile::tempdir().expect("temp dir");
        fs::write(
            scheme.path().join("stats"),
            "cpu 0 pending 7 lost 0\ncpu 2 pending 4 lost 5\n",
        )
        .expect("write");
        fs::create_dir(scheme.path().join("cpu")).expect("mkdir");
        fs::write(
            scheme.path().join("cpu/0"),
            bytes(&[(&SYSCALL_ENTER, 10, &[1, 0, 0, 0, 0])]),
        )
        .expect("write");
        fs::write(
            scheme.path().join("cpu/2"),
            bytes(&[(&SYSCALL_EXIT, 20, &[1, 0])]),
        )
        .expect("write");
        scheme
    }
}
