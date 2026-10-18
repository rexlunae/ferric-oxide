//! Loading and decoding a capture: one file of drained records per CPU.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ferric_trace::kernel::CATALOG;
use ferric_trace::record::{DecodeError, Records};
use ferric_trace::{Event, EventId};

/// Why a capture could not be loaded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CaptureError {
    /// A file or directory could not be read.
    #[error("cannot read {}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A file's name does not say which CPU it is from.
    #[error("{}: capture files are named cpu<n> or cpu<n>.bin", path.display())]
    Name {
        /// The path.
        path: PathBuf,
    },
    /// Two files are for the same CPU.
    #[error("two files for CPU {cpu}")]
    DuplicateCpu {
        /// The CPU.
        cpu: u32,
    },
    /// A file's bytes are not whole, valid records.
    #[error("CPU {cpu}: record {index} does not decode")]
    Decode {
        /// The CPU.
        cpu: u32,
        /// How many records decoded before it.
        index: usize,
        /// The underlying error.
        #[source]
        source: DecodeError,
    },
    /// A record is of an event the kernel catalog does not have.
    #[error("CPU {cpu}: record {index} is of unknown event {event}")]
    UnknownEvent {
        /// The CPU.
        cpu: u32,
        /// How many records decoded before it.
        index: usize,
        /// The event id.
        event: EventId,
    },
    /// A record has a different number of arguments than its catalog event names, so its
    /// fields cannot be trusted.
    #[error("CPU {cpu}: record {index} ({}) has {found} arguments, the event has {expected}", event.name())]
    ArgCount {
        /// The CPU.
        cpu: u32,
        /// How many records decoded before it.
        index: usize,
        /// The record's event.
        event: &'static Event,
        /// The arguments the catalog event names.
        expected: usize,
        /// The arguments the record has.
        found: usize,
    },
    /// The inputs held no capture files.
    #[error("no cpu<n> capture files in the inputs")]
    Empty,
    /// A capture directory holds the [`INCOMPLETE`] marker: the capture failed or was killed,
    /// so CPUs may be missing or cut short.
    #[error("{} is a partial capture (it holds {INCOMPLETE}); pass --partial to read it anyway", path.display())]
    Incomplete {
        /// The capture directory.
        path: PathBuf,
    },
}

/// The file `ferric-trace capture` writes into a capture directory before draining and removes
/// once the capture is complete.
pub const INCOMPLETE: &str = "INCOMPLETE";

/// One decoded record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The CPU whose ring it came from.
    pub cpu: u32,
    /// Its timestamp (TSC cycles).
    pub timestamp: u64,
    /// Its event.
    pub event: &'static Event,
    /// Its arguments, as many as the event names.
    pub args: Vec<u64>,
}

impl Entry {
    /// Argument `index`, or 0 past the last one.
    pub fn arg(&self, index: usize) -> u64 {
        self.args.get(index).copied().unwrap_or(0)
    }
}

/// One CPU's decoded records, in ring order.
#[derive(Clone, Debug)]
pub struct CpuCapture {
    cpu: u32,
    entries: Vec<Entry>,
}

impl CpuCapture {
    /// Decodes the bytes read from CPU `cpu`'s ring.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Decode`] when the bytes are not whole records,
    /// [`CaptureError::UnknownEvent`] for an event the kernel catalog does not have, and
    /// [`CaptureError::ArgCount`] for a record with more or fewer arguments than its event.
    pub fn decode(cpu: u32, bytes: &[u8]) -> Result<Self, CaptureError> {
        Self::decode_inner(cpu, bytes, false)
    }

    /// Decodes like [`CpuCapture::decode`], but for a partial capture: a last record cut off
    /// by the end of the bytes (a capture killed mid-write) is dropped, and the whole records
    /// before it are kept.
    ///
    /// # Errors
    ///
    /// Those of [`CpuCapture::decode`], except a truncated last record.
    pub fn decode_partial(cpu: u32, bytes: &[u8]) -> Result<Self, CaptureError> {
        Self::decode_inner(cpu, bytes, true)
    }

    fn decode_inner(cpu: u32, bytes: &[u8], partial: bool) -> Result<Self, CaptureError> {
        let mut entries = Vec::new();
        for (index, record) in Records::new(bytes).enumerate() {
            let record = match record {
                Ok(record) => record,
                // Only the end of the bytes can cut a record short, so nothing follows it.
                Err(DecodeError::Truncated { .. }) if partial => break,
                Err(source) => return Err(CaptureError::Decode { cpu, index, source }),
            };
            let event = CATALOG
                .get(record.event())
                .ok_or(CaptureError::UnknownEvent {
                    cpu,
                    index,
                    event: record.event(),
                })?;
            // A wrong count would make missing fields read as 0, which the exporters would
            // take for real ids.
            if record.arg_count() != event.args().len() {
                return Err(CaptureError::ArgCount {
                    cpu,
                    index,
                    event,
                    expected: event.args().len(),
                    found: record.arg_count(),
                });
            }
            entries.push(Entry {
                cpu,
                timestamp: record.timestamp(),
                event,
                args: record.args().collect(),
            });
        }
        Ok(Self { cpu, entries })
    }

    /// The CPU.
    pub fn cpu(&self) -> u32 {
        self.cpu
    }

    /// The records, in ring order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

/// Every CPU's records, merged in timestamp order.
#[derive(Clone, Debug)]
pub struct Capture {
    cpus: Vec<u32>,
    entries: Vec<Entry>,
}

impl Capture {
    /// Merges per-CPU captures. Records with equal timestamps keep CPU, then ring, order.
    pub fn from_cpus(mut cpus: Vec<CpuCapture>) -> Self {
        cpus.sort_by_key(CpuCapture::cpu);
        let ids = cpus.iter().map(CpuCapture::cpu).collect();
        let mut entries: Vec<Entry> = cpus.into_iter().flat_map(|c| c.entries).collect();
        // A stable sort keeps the CPU and ring order of equal timestamps.
        entries.sort_by_key(|e| e.timestamp);
        Self { cpus: ids, entries }
    }

    /// Loads the capture files in `inputs`: `cpu<n>` or `cpu<n>.bin` files, or directories
    /// holding them (other files in a directory are ignored).
    ///
    /// # Errors
    ///
    /// [`CaptureError::Io`] when a path cannot be read, [`CaptureError::Name`] for a file not
    /// named after its CPU, [`CaptureError::DuplicateCpu`], [`CaptureError::Empty`] when no
    /// files were found, [`CaptureError::Incomplete`] for a directory (or a file in one) that a
    /// failed capture left, and the errors of [`CpuCapture::decode`].
    pub fn load(inputs: &[PathBuf]) -> Result<Self, CaptureError> {
        Self::load_inner(inputs, false)
    }

    /// Loads like [`Capture::load`], but also reads a partial capture (one marked
    /// [`INCOMPLETE`]), whose CPUs may be missing or cut short: each file is decoded with
    /// [`CpuCapture::decode_partial`].
    ///
    /// # Errors
    ///
    /// Those of [`Capture::load`] except [`CaptureError::Incomplete`] and a truncated last
    /// record.
    pub fn load_partial(inputs: &[PathBuf]) -> Result<Self, CaptureError> {
        Self::load_inner(inputs, true)
    }

    fn load_inner(inputs: &[PathBuf], partial: bool) -> Result<Self, CaptureError> {
        let mut files = Vec::new();
        for input in inputs {
            let meta = fs::metadata(input).map_err(|source| CaptureError::Io {
                path: input.clone(),
                source,
            })?;
            let dir = if meta.is_dir() {
                Some(input.as_path())
            } else {
                input.parent()
            };
            if let Some(dir) = dir.filter(|d| !partial && d.join(INCOMPLETE).exists()) {
                return Err(CaptureError::Incomplete {
                    path: dir.to_owned(),
                });
            }
            if meta.is_dir() {
                files.extend(dir_files(input)?);
            } else {
                let cpu = cpu_of(input).ok_or_else(|| CaptureError::Name {
                    path: input.clone(),
                })?;
                files.push((cpu, input.clone()));
            }
        }
        if files.is_empty() {
            return Err(CaptureError::Empty);
        }
        files.sort();
        if let Some(pair) = files.windows(2).find(|p| p[0].0 == p[1].0) {
            return Err(CaptureError::DuplicateCpu { cpu: pair[0].0 });
        }
        let cpus = files
            .into_iter()
            .map(|(cpu, path)| {
                let bytes = fs::read(&path).map_err(|source| CaptureError::Io { path, source })?;
                if partial {
                    CpuCapture::decode_partial(cpu, &bytes)
                } else {
                    CpuCapture::decode(cpu, &bytes)
                }
            })
            .collect::<Result<_, _>>()?;
        Ok(Self::from_cpus(cpus))
    }

    /// The CPUs captured, in order.
    pub fn cpus(&self) -> &[u32] {
        &self.cpus
    }

    /// All records, in timestamp order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

/// The CPU a capture file is for, from its name: `cpu<n>` or `cpu<n>.bin`.
fn cpu_of(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(".bin").unwrap_or(name);
    name.strip_prefix("cpu")?.parse().ok()
}

/// The capture files in directory `dir`.
fn dir_files(dir: &Path) -> Result<Vec<(u32, PathBuf)>, CaptureError> {
    let io_err = |source| CaptureError::Io {
        path: dir.to_owned(),
        source,
    };
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_err)? {
        let path = entry.map_err(io_err)?.path();
        if let Some(cpu) = cpu_of(&path).filter(|_| path.is_file()) {
            files.push((cpu, path));
        }
    }
    Ok(files)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ferric_trace::kernel::{SCHED_SWITCH, SYSCALL_ENTER, SYSCALL_EXIT};
    use ferric_trace::record::{MAX_RECORD_WORDS, encode};

    /// The bytes of `records`, as a ring read returns them.
    pub(crate) fn bytes(records: &[(&Event, u64, &[u64])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (event, ts, args) in records {
            let mut words = [0u64; MAX_RECORD_WORDS];
            let n = encode(&mut words, event.id(), *ts, args).expect("valid record");
            out.extend(words[..n].iter().flat_map(|w| w.to_le_bytes()));
        }
        out
    }

    #[test]
    fn names_give_the_cpu() {
        assert_eq!(cpu_of(Path::new("x/cpu3")), Some(3));
        assert_eq!(cpu_of(Path::new("cpu12.bin")), Some(12));
        assert_eq!(cpu_of(Path::new("cpu")), None);
        assert_eq!(cpu_of(Path::new("stats")), None);
    }

    #[test]
    fn merges_cpus_by_timestamp() {
        let a = bytes(&[
            (&SYSCALL_ENTER, 10, &[1, 0, 0, 0, 0]),
            (&SYSCALL_EXIT, 30, &[1, 0]),
        ]);
        let b = bytes(&[(&SCHED_SWITCH, 20, &[1, 2, 3, 4])]);
        let capture = Capture::from_cpus(vec![
            CpuCapture::decode(1, &b).expect("valid"),
            CpuCapture::decode(0, &a).expect("valid"),
        ]);
        assert_eq!(capture.cpus(), [0, 1]);
        let order: Vec<_> = capture
            .entries()
            .iter()
            .map(|e| (e.cpu, e.timestamp, e.event.name()))
            .collect();
        assert_eq!(
            order,
            [
                (0, 10, "syscall.enter"),
                (1, 20, "sched.switch"),
                (0, 30, "syscall.exit")
            ]
        );
    }

    #[test]
    fn bad_bytes_are_errors() {
        let mut b = bytes(&[(&SYSCALL_EXIT, 1, &[1, 2])]);
        b.truncate(b.len() - 3);
        assert!(matches!(
            CpuCapture::decode(2, &b),
            Err(CaptureError::Decode {
                cpu: 2,
                index: 0,
                ..
            })
        ));
        let mut words = [0u64; MAX_RECORD_WORDS];
        let n = encode(&mut words, EventId::new(250), 1, &[]).expect("valid");
        let unknown: Vec<u8> = words[..n].iter().flat_map(|w| w.to_le_bytes()).collect();
        assert!(matches!(
            CpuCapture::decode(0, &unknown),
            Err(CaptureError::UnknownEvent { index: 0, .. })
        ));
    }

    /// The bytes of one `event` record with `args`, whatever the catalog says.
    fn raw(event: &Event, args: &[u64]) -> Vec<u8> {
        let mut words = [0u64; MAX_RECORD_WORDS];
        let n = encode(&mut words, event.id(), 1, args).expect("valid record");
        words[..n].iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn wrong_argument_counts_are_errors() {
        let mut short = bytes(&[(&SYSCALL_EXIT, 0, &[1, 2])]);
        short.extend(raw(&SCHED_SWITCH, &[3, 7]));
        assert!(matches!(
            CpuCapture::decode(0, &short),
            Err(CaptureError::ArgCount {
                cpu: 0,
                index: 1,
                expected: 4,
                found: 2,
                ..
            })
        ));
        let long = raw(&SYSCALL_EXIT, &[1, 2, 3]);
        let err = CpuCapture::decode(5, &long).expect_err("too many arguments");
        assert!(matches!(
            err,
            CaptureError::ArgCount {
                cpu: 5,
                index: 0,
                expected: 2,
                found: 3,
                ..
            }
        ));
        assert_eq!(
            err.to_string(),
            "CPU 5: record 0 (syscall.exit) has 3 arguments, the event has 2"
        );
        assert!(CpuCapture::decode(0, &raw(&SYSCALL_EXIT, &[1, 2])).is_ok());
    }

    #[test]
    fn refuses_a_killed_capture_unless_partial() {
        // What a capture killed after draining CPU 0 leaves: the marker and one CPU file.
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(INCOMPLETE), "").expect("write");
        fs::write(
            dir.path().join("cpu0"),
            bytes(&[(&SYSCALL_EXIT, 5, &[1, 2])]),
        )
        .expect("write");
        for input in [dir.path().to_owned(), dir.path().join("cpu0")] {
            assert!(matches!(
                Capture::load(std::slice::from_ref(&input)),
                Err(CaptureError::Incomplete { path }) if path == dir.path()
            ));
            let partial = Capture::load_partial(&[input]).expect("loads");
            assert_eq!(partial.entries().len(), 1);
        }
    }

    #[test]
    fn partial_loads_keep_records_before_a_cut_off_one() {
        let mut cut = bytes(&[
            (&SYSCALL_ENTER, 10, &[1, 0, 0, 0, 0]),
            (&SYSCALL_EXIT, 20, &[1, 0]),
        ]);
        cut.truncate(cut.len() - 5);
        assert!(matches!(
            CpuCapture::decode(0, &cut),
            Err(CaptureError::Decode {
                index: 1,
                source: DecodeError::Truncated { .. },
                ..
            })
        ));
        let kept = CpuCapture::decode_partial(0, &cut).expect("keeps the whole record");
        assert_eq!(kept.entries().len(), 1);
        assert_eq!(kept.entries()[0].timestamp, 10);

        // Through the loaders: strict refuses, partial keeps the first record.
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(INCOMPLETE), "").expect("write");
        fs::write(dir.path().join("cpu0"), &cut).expect("write");
        let input = [dir.path().to_owned()];
        assert_eq!(
            Capture::load_partial(&input)
                .expect("loads")
                .entries()
                .len(),
            1
        );
        fs::remove_file(dir.path().join(INCOMPLETE)).expect("remove");
        assert!(matches!(
            Capture::load(&input),
            Err(CaptureError::Decode { .. })
        ));
        // A corrupt record is still an error in a partial load.
        let mut bad = bytes(&[(&SYSCALL_EXIT, 20, &[1, 0])]);
        bad[4] = 0xff;
        assert!(CpuCapture::decode_partial(0, &bad).is_err());
    }

    #[test]
    fn loads_a_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(
            dir.path().join("cpu0"),
            bytes(&[(&SYSCALL_EXIT, 5, &[1, 2])]),
        )
        .expect("write");
        fs::write(
            dir.path().join("cpu1.bin"),
            bytes(&[(&SYSCALL_EXIT, 4, &[1, 2])]),
        )
        .expect("write");
        fs::write(dir.path().join("stats"), "cpu 0 pending 0 lost 0\n").expect("write");
        let capture = Capture::load(&[dir.path().to_owned()]).expect("loads");
        assert_eq!(capture.cpus(), [0, 1]);
        assert_eq!(capture.entries()[0].cpu, 1);

        let twice = Capture::load(&[dir.path().to_owned(), dir.path().join("cpu0")]);
        assert!(matches!(twice, Err(CaptureError::DuplicateCpu { cpu: 0 })));
        let empty = tempfile::tempdir().expect("temp dir");
        assert!(matches!(
            Capture::load(&[empty.path().to_owned()]),
            Err(CaptureError::Empty)
        ));
    }
}
