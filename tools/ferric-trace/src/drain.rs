//! Saving a capture on the target: draining every CPU's ring of the `kernel.trace` scheme into
//! a directory that [`Capture::load`](crate::capture::Capture::load) reads back.
//!
//! [`drain_scheme`] reads the scheme's `stats` file for the list of CPUs, saves it as `stats`
//! in the output directory, then drains each `cpu/<n>` file into `cpu<n>` until a read returns
//! 0 bytes (the ring is empty). A read fails with `EAGAIN` while another reader is draining the
//! same CPU; that read is retried after a short wait, up to [`AGAIN_RETRIES`] times in a row.
//! Any other error stops the capture, naming the file.
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

/// How long to wait after an `EAGAIN` read.
const AGAIN_WAIT: Duration = Duration::from_millis(5);

/// The read size: many records at a time, and never less than [`abi::READ_MIN`].
const READ_BYTES: usize = 64 * 1024;

const _: () = assert!(READ_BYTES >= abi::READ_MIN);

/// Why a capture could not be saved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DrainError {
    /// A file could not be opened, read, created or written.
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
}

/// What was saved for one CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuDrained {
    /// The CPU.
    pub cpu: u32,
    /// The bytes saved.
    pub bytes: u64,
    /// Records the CPU had dropped, from `stats` at the start of the capture.
    pub lost: u64,
}

/// Drains every CPU of the trace scheme at `scheme` (normally `/scheme/kernel.trace`) into
/// directory `out`, which is created if needed.
///
/// # Errors
///
/// [`DrainError::Io`] for a file that cannot be read or written, [`DrainError::Stats`] for a
/// `stats` line that does not parse, and [`DrainError::Busy`] for a CPU file that another
/// reader holds for too long.
pub fn drain_scheme(scheme: &Path, out: &Path) -> Result<Vec<CpuDrained>, DrainError> {
    let stats_path = scheme.join(abi::STATS_PATH);
    let text = fs::read_to_string(&stats_path).map_err(io_error(&stats_path))?;
    let stats = text
        .lines()
        .map(|line| {
            StatsLine::parse(line).map_err(|source| DrainError::Stats {
                path: stats_path.clone(),
                line: line.to_owned(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    fs::create_dir_all(out).map_err(io_error(out))?;
    let saved_stats = out.join(abi::STATS_PATH);
    fs::write(&saved_stats, &text).map_err(io_error(&saved_stats))?;

    let mut drained = Vec::with_capacity(stats.len());
    for line in stats {
        let from = scheme.join(abi::CPU_DIR).join(line.cpu.to_string());
        let to = out.join(format!("cpu{}", line.cpu));
        let mut source = File::open(&from).map_err(io_error(&from))?;
        let mut sink = File::create(&to).map_err(io_error(&to))?;
        let bytes = drain(&mut source, &from, &mut sink, &to, AGAIN_WAIT)?;
        drained.push(CpuDrained {
            cpu: line.cpu,
            bytes,
            lost: line.lost,
        });
    }
    Ok(drained)
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
        // A stand-in for the scheme: stats and one file per CPU, CPU 2 not numbered next.
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

        let out = tempfile::tempdir().expect("temp dir");
        let saved = out.path().join("capture");
        let drained = drain_scheme(scheme.path(), &saved).expect("drains");
        assert_eq!(
            drained,
            [
                CpuDrained {
                    cpu: 0,
                    bytes: 56,
                    lost: 0
                },
                CpuDrained {
                    cpu: 2,
                    bytes: 32,
                    lost: 5
                }
            ]
        );
        let capture = Capture::load(std::slice::from_ref(&saved)).expect("loads back");
        assert_eq!(capture.cpus(), [0, 2]);
        assert_eq!(capture.entries().len(), 2);
        assert!(saved.join("stats").is_file());

        fs::remove_file(scheme.path().join("cpu/2")).expect("remove");
        assert!(matches!(
            drain_scheme(scheme.path(), &saved),
            Err(DrainError::Io { path, .. }) if path.ends_with("cpu/2")
        ));
        fs::write(scheme.path().join("stats"), "cpu zero\n").expect("write");
        assert!(matches!(
            drain_scheme(scheme.path(), &saved),
            Err(DrainError::Stats { .. })
        ));
    }
}
