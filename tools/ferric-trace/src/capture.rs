//! Loading and decoding a capture: one file of drained records per CPU, and the `clock` file
//! that says what their timestamps count ([`ferric_trace::clock::ClockLine`]; captures from
//! kernels before T3d have none, and count TSC cycles).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ferric_trace::abi::LineError;
use ferric_trace::clock::{Clock, ClockLine};
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
    /// A capture's `clock` file does not parse.
    #[error("{}: bad clock line {line:?}", path.display())]
    Clock {
        /// The `clock` file.
        path: PathBuf,
        /// Its contents.
        line: String,
        /// Why it does not parse.
        #[source]
        source: LineError,
    },
    /// Two inputs name different clocks, so their timestamps cannot be merged.
    #[error("the inputs' clocks differ ({first} and {second})")]
    ClockMismatch {
        /// One clock line, without its newline.
        first: String,
        /// The other.
        second: String,
    },
    /// One input has no clock file, so its timestamps are TSC cycles (it comes from a kernel
    /// that does not say), and another's come from a different clock.
    #[error("an input has no clock file (TSC cycles) and another's clock is {}", clock.clock)]
    LegacyClockMismatch {
        /// The other input's clock.
        clock: ClockLine,
    },
    /// A capture directory holds the [`INCOMPLETE`] marker: the capture failed or was killed,
    /// so CPUs may be missing or cut short.
    #[error("{} is a partial capture (it holds {INCOMPLETE}); pass --partial to read it anyway", path.display())]
    Incomplete {
        /// The capture directory.
        path: PathBuf,
    },
}

/// How strictly a capture is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeMode {
    /// Every file must be whole records, and a capture marked [`INCOMPLETE`] is refused.
    Strict,
    /// A capture marked [`INCOMPLETE`] is read, and a record cut off at the end of a file is
    /// dropped.
    Partial,
}

/// The file `ferric-trace capture` writes into a capture directory before draining and removes
/// once the capture is complete.
pub const INCOMPLETE: &str = "INCOMPLETE";

/// The file in a capture directory that holds the kernel's clock line, as `ferric-trace
/// capture` copied it from the scheme's [`CLOCK_PATH`](ferric_trace::abi::CLOCK_PATH).
pub const CLOCK: &str = ferric_trace::abi::CLOCK_PATH;

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
        Self::decode_inner(cpu, bytes, DecodeMode::Strict)
    }

    /// Decodes like [`CpuCapture::decode`], but for a partial capture: a last record cut off
    /// by the end of the bytes (a capture killed mid-write) is dropped, and the whole records
    /// before it are kept.
    ///
    /// # Errors
    ///
    /// Those of [`CpuCapture::decode`], except a truncated last record.
    pub fn decode_partial(cpu: u32, bytes: &[u8]) -> Result<Self, CaptureError> {
        Self::decode_inner(cpu, bytes, DecodeMode::Partial)
    }

    fn decode_inner(cpu: u32, bytes: &[u8], mode: DecodeMode) -> Result<Self, CaptureError> {
        let mut entries = Vec::new();
        for (index, record) in Records::new(bytes).enumerate() {
            let record = match record {
                Ok(record) => record,
                // Only the end of the bytes can cut a record short, so nothing follows it.
                Err(DecodeError::Truncated { .. }) if mode == DecodeMode::Partial => break,
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
    clock: Option<ClockLine>,
}

impl Capture {
    /// Merges per-CPU captures. Records with equal timestamps keep CPU, then ring, order.
    pub fn from_cpus(mut cpus: Vec<CpuCapture>) -> Self {
        cpus.sort_by_key(CpuCapture::cpu);
        let ids = cpus.iter().map(CpuCapture::cpu).collect();
        let mut entries: Vec<Entry> = cpus.into_iter().flat_map(|c| c.entries).collect();
        // A stable sort keeps the CPU and ring order of equal timestamps.
        entries.sort_by_key(|e| e.timestamp);
        Self {
            cpus: ids,
            entries,
            clock: None,
        }
    }

    /// The capture with `clock` as the clock of its timestamps.
    pub fn with_clock(self, clock: Option<ClockLine>) -> Self {
        Self { clock, ..self }
    }

    /// Loads the capture files in `inputs`: `cpu<n>` or `cpu<n>.bin` files, or directories
    /// holding them (other files in a directory are ignored).
    ///
    /// # Errors
    ///
    /// [`CaptureError::Io`] when a path cannot be read, [`CaptureError::Name`] for a file not
    /// named after its CPU, [`CaptureError::DuplicateCpu`], [`CaptureError::Empty`] when no
    /// files were found, [`CaptureError::Incomplete`] for a directory (or a file in one) that a
    /// failed capture left, [`CaptureError::Clock`] for a `clock` file that does not parse,
    /// [`CaptureError::ClockMismatch`] for inputs with different clocks,
    /// [`CaptureError::LegacyClockMismatch`] for an input without a clock file (TSC cycles)
    /// together with one whose clock is not the TSC, and the errors of
    /// [`CpuCapture::decode`].
    pub fn load(inputs: &[PathBuf]) -> Result<Self, CaptureError> {
        Self::load_inner(inputs, DecodeMode::Strict)
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
        Self::load_inner(inputs, DecodeMode::Partial)
    }

    fn load_inner(inputs: &[PathBuf], mode: DecodeMode) -> Result<Self, CaptureError> {
        let mut files = Vec::new();
        let mut clock = ClockMerge::default();
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
            if let Some(dir) =
                dir.filter(|d| mode == DecodeMode::Strict && d.join(INCOMPLETE).exists())
            {
                return Err(CaptureError::Incomplete {
                    path: dir.to_owned(),
                });
            }
            if let Some(dir) = dir {
                clock.add(read_clock(dir)?)?;
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
                CpuCapture::decode_inner(cpu, &bytes, mode)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self::from_cpus(cpus).with_clock(clock.clock))
    }

    /// The CPUs captured, in order.
    pub fn cpus(&self) -> &[u32] {
        &self.cpus
    }

    /// All records, in timestamp order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The clock of the timestamps, when the capture says.
    pub fn clock(&self) -> Option<ClockLine> {
        self.clock
    }
}

/// The clock line in capture directory `dir`, if it has one.
fn read_clock(dir: &Path) -> Result<Option<ClockLine>, CaptureError> {
    let path = dir.join(CLOCK);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CaptureError::Io { path, source }),
    };
    ClockLine::parse(&text)
        .map(Some)
        .map_err(|source| CaptureError::Clock {
            path,
            line: text,
            source,
        })
}

/// The clock of the inputs read so far, which must agree.
#[derive(Debug, Default)]
struct ClockMerge {
    /// The clock the inputs with a clock file name.
    clock: Option<ClockLine>,
    /// Whether an input had no clock file: its timestamps are TSC cycles.
    legacy: bool,
}

impl ClockMerge {
    /// Adds the clock of one more input, `None` for one without a clock file.
    fn add(&mut self, found: Option<ClockLine>) -> Result<(), CaptureError> {
        match (self.clock, found) {
            (Some(first), Some(second)) if first != second => {
                return Err(CaptureError::ClockMismatch {
                    first: first.to_string().trim_end().to_owned(),
                    second: second.to_string().trim_end().to_owned(),
                });
            }
            (_, Some(found)) => self.clock = Some(found),
            (_, None) => self.legacy = true,
        }
        // An input without a clock counts TSC cycles, which only a TSC clock agrees with.
        match self.clock {
            Some(clock) if self.legacy && clock.clock != Clock::Tsc => {
                Err(CaptureError::LegacyClockMismatch { clock })
            }
            _ => Ok(()),
        }
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
    fn clock_files() {
        use ferric_trace::clock::Clock;
        let write = |clock: &str| {
            let dir = tempfile::tempdir().expect("temp dir");
            fs::write(
                dir.path().join("cpu0"),
                bytes(&[(&SYSCALL_EXIT, 5, &[1, 2])]),
            )
            .expect("write");
            fs::write(dir.path().join(CLOCK), clock).expect("write");
            dir
        };
        let hpet = write("clock hpet period_fs 10000000\n");
        let capture = Capture::load(&[hpet.path().to_owned()]).expect("loads");
        assert_eq!(
            capture.clock(),
            Some(ClockLine {
                clock: Clock::Hpet,
                period_fs: Some(10_000_000)
            })
        );
        // A file named on its own takes the clock of its directory.
        let file = Capture::load(&[hpet.path().join("cpu0")]).expect("loads");
        assert_eq!(file.clock(), capture.clock());

        let bad = write("clock sundial period_fs 1\n");
        assert!(matches!(
            Capture::load(&[bad.path().to_owned()]),
            Err(CaptureError::Clock {
                source: LineError::Clock,
                ..
            })
        ));
        let kvm = write("clock kvmclock period_fs 1000000\n");
        let mut both = vec![hpet.path().to_owned()];
        both.push(kvm.path().join("cpu0"));
        assert!(matches!(
            Capture::load(&both),
            Err(CaptureError::ClockMismatch { .. })
        ));
    }

    #[test]
    fn a_missing_clock_is_the_tsc() {
        use ferric_trace::clock::Clock;
        let line = |clock, period_fs| Some(ClockLine { clock, period_fs });
        let kvm = line(Clock::KvmClock, Some(1_000_000));
        let tsc = line(Clock::Tsc, None);
        for inputs in [[None, kvm], [kvm, None]] {
            let mut merge = ClockMerge::default();
            let result = inputs.into_iter().try_for_each(|clock| merge.add(clock));
            assert!(
                matches!(result, Err(CaptureError::LegacyClockMismatch { clock }) if Some(clock) == kvm),
                "{inputs:?}"
            );
        }
        for inputs in [[None, tsc], [tsc, None], [None, None]] {
            let mut merge = ClockMerge::default();
            for clock in inputs {
                merge.add(clock).expect("TSC cycles agree");
            }
            assert_eq!(merge.clock, inputs.into_iter().flatten().next());
        }
        // A directory without a clock file merged with one that has a KVM clock.
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, cpu) in [("old", "cpu0"), ("new", "cpu1")] {
            let sub = dir.path().join(name);
            fs::create_dir(&sub).expect("mkdir");
            fs::write(sub.join(cpu), bytes(&[(&SYSCALL_EXIT, 5, &[1, 2])])).expect("write");
        }
        fs::write(
            dir.path().join("new").join(CLOCK),
            "clock kvmclock period_fs 1000000\n",
        )
        .expect("write");
        assert!(matches!(
            Capture::load(&[dir.path().join("old/cpu0"), dir.path().join("new")]),
            Err(CaptureError::LegacyClockMismatch { .. })
        ));
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
