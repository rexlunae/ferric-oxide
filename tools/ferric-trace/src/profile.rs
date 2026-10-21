//! Profiles from the kernel's sampling profiler (`kernel.profiling`): saving one on the target
//! ([`record`], the `ferric-trace profile` command) and loading it back on the host
//! ([`Profile::load`]).
//!
//! The profiler's interface is [`ferric_trace::profile`]. A profile is a directory of:
//!
//! - [`KERNEL_TEXT`]: the line read from `ctl-profiling`, the run-time address of the kernel's
//!   text, which turns kernel addresses back into link-time ones under KASLR;
//! - `samples<n>` ([`SAMPLES_PREFIX`]): the words drained from `profiling-<n>`, the samples of
//!   ordinary CPU `n`, as native-endian (little-endian on x86_64) `u64`s;
//! - [`NAMES`]: one `<debug id> <name>` line for each context the samples name, as the kernel
//!   names it (the `debug` scheme's `call` on a CPU file).
//!
//! [`record`] first takes the profiler for itself by writing `x` to `ctl-profiling`, which
//! stops sampling and makes any other recorder's writes fail (`EBUSY`, reported as
//! [`ProfileError::Busy`]) until this one closes its control file. It then drains whatever
//! earlier runs left in the buffers and throws it away, turns sampling on, runs a command or
//! waits, and drains every buffer every [`DRAIN_INTERVAL`] until sampling is off again, so the
//! buffers never fill. As with trace captures, the directory is created exclusively (its
//! parents as needed) and holds an `INCOMPLETE` marker ([`crate::capture::INCOMPLETE`]) until
//! everything is saved. Sampling is turned off on every path out of [`record`], including
//! errors, and a command it started is killed and waited for if recording fails.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use ferric_trace::profile::{
    CPU_FILE_PREFIX, CTL_FILE, ControlLine, ControlLineError, Sample, SampleError, Samples,
};

use crate::capture::INCOMPLETE;

/// The file holding the [`ControlLine`].
pub const KERNEL_TEXT: &str = "kernel_text";

/// The prefix of the per-CPU sample files: `samples<cpu>`.
pub const SAMPLES_PREFIX: &str = "samples";

/// The file naming the sampled contexts.
pub const NAMES: &str = "names";

/// How often [`record`] drains the buffers while sampling.
pub const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

/// The read size for the buffers.
const READ_BYTES: usize = 256 * 1024;

/// The longest context name the kernel gives (an `ArrayString<32>`).
const NAME_BYTES: usize = 32;

const WORD_BYTES: usize = size_of::<u64>();

/// What [`record`] samples while profiling is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    /// Sample for this long.
    For(Duration),
    /// Run this command (a program and its arguments) and sample until it exits.
    Command(Vec<OsString>),
}

/// What [`record`] saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The bytes saved for each CPU, in CPU order.
    pub cpu_bytes: Vec<u64>,
    /// How many contexts were named.
    pub names: usize,
    /// The command's exit status, for [`Run::Command`].
    pub status: Option<ExitStatus>,
}

/// Why a profile could not be recorded or loaded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileError {
    /// A file could not be opened, read, created or written.
    #[error("{}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The control line does not parse.
    #[error("{}: bad control line {line:?}", path.display())]
    Control {
        /// The file it came from.
        path: PathBuf,
        /// The line.
        line: String,
        /// Why it does not parse.
        #[source]
        source: ControlLineError,
    },
    /// The command could not be started or waited for.
    #[error("cannot run {program:?}")]
    Command {
        /// The program.
        program: OsString,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// [`Run::Command`] with no program.
    #[error("no command to run")]
    NoCommand,
    /// The debug scheme has no `profiling-0`: the kernel was built without `kernel.profiling`.
    #[error("{} has no profiling buffers; is kernel.profiling on?", path.display())]
    NoBuffers {
        /// The debug scheme path.
        path: PathBuf,
    },
    /// Another recorder owns the profiler (its write of `x` to `ctl-profiling` came first).
    #[error("{}: the profiler is in use by another recorder", path.display())]
    Busy {
        /// The control file.
        path: PathBuf,
    },
    /// [`Run::For`] a length no clock deadline can represent.
    #[error("cannot sample for {length:?}")]
    TooLong {
        /// The length asked for.
        length: Duration,
    },
    /// A CPU's sample file is missing while a higher CPU's is there.
    #[error("{}: no samples for CPU {cpu}, but samples for a later CPU", path.display())]
    MissingSamples {
        /// The profile directory.
        path: PathBuf,
        /// The CPU without a file.
        cpu: usize,
    },
    /// The profile is marked `INCOMPLETE` and was not loaded as partial.
    #[error("{} is incomplete (the recording failed or was killed)", path.display())]
    Incomplete {
        /// The profile directory.
        path: PathBuf,
    },
    /// A sample file's length is not a whole number of words.
    #[error("{}: {len} bytes is not a whole number of words", path.display())]
    TornWord {
        /// The file.
        path: PathBuf,
        /// Its length.
        len: usize,
    },
    /// A line of the names file is not `<id> <name>`.
    #[error("{}: bad line {line:?}", path.display())]
    Names {
        /// The file.
        path: PathBuf,
        /// The line.
        line: String,
    },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> ProfileError + '_ {
    move |source| ProfileError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Records a profile into `out`, which must not exist, from the debug scheme at `debug`
/// (`/scheme/debug`). Needs root, and a kernel with `kernel.profiling`.
///
/// # Errors
///
/// [`ProfileError::NoBuffers`] without the profiler, [`ProfileError::Control`] if
/// `ctl-profiling` gives no control line (a kernel without the read), [`ProfileError::TooLong`]
/// for a length past what the clock can count to, [`ProfileError::Busy`] while another
/// recorder runs, [`ProfileError::Command`] if the command does not start or cannot be waited
/// for, and [`ProfileError::Io`] naming the file for any other failure. Whatever was saved
/// stays in `out` with its `INCOMPLETE` marker.
pub fn record(debug: &Path, out: &Path, run: &Run) -> Result<Recorded, ProfileError> {
    let ctl_path = debug.join(CTL_FILE);
    let text = fs::read_to_string(&ctl_path).map_err(io_error(&ctl_path))?;
    let control = parse_control(&ctl_path, &text)?;
    let mut cpus = open_buffers(debug)?;
    if let Run::For(length) = run {
        deadline(*length)?;
    }
    let mut sampling = Sampling::claim(&ctl_path)?;

    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    fs::create_dir(out).map_err(io_error(out))?;
    let marker = out.join(INCOMPLETE);
    fs::write(&marker, b"").map_err(io_error(&marker))?;
    write_file(&out.join(KERNEL_TEXT), control.to_string().as_bytes())?;
    let mut files = cpus
        .iter()
        .enumerate()
        .map(|(cpu, _)| {
            let path = out.join(format!("{SAMPLES_PREFIX}{cpu}"));
            File::create_new(&path)
                .map(|file| (path.clone(), file))
                .map_err(io_error(&path))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Samples from an earlier run would be attributed to this one. Sampling is off since the
    // claim, so the buffers stay empty once drained.
    for buffer in &mut cpus {
        drain(buffer, &mut io::sink())?;
    }
    sampling.start()?;
    let status = sample(run, &mut cpus, &mut files)?;
    sampling.stop()?;
    drain_all(&mut cpus, &mut files)?;
    let cpu_bytes = files
        .iter()
        .map(|(path, file)| file.metadata().map(|m| m.len()).map_err(io_error(path)))
        .collect::<Result<_, _>>()?;

    let names = name_contexts(
        &cpus,
        &files.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
    )?;
    let names_path = out.join(NAMES);
    write_file(&names_path, names_file(&names).as_bytes())?;
    fs::remove_file(&marker).map_err(io_error(&marker))?;
    Ok(Recorded {
        cpu_bytes,
        names: names.len(),
        status,
    })
}

fn parse_control(path: &Path, text: &str) -> Result<ControlLine, ProfileError> {
    ControlLine::parse(text).map_err(|source| ProfileError::Control {
        path: path.to_owned(),
        line: text.to_owned(),
        source,
    })
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), ProfileError> {
    File::create_new(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(io_error(path))
}

/// A CPU buffer file, open for draining.
struct Buffer {
    path: PathBuf,
    file: File,
}

/// Opens `profiling-0`, `profiling-1`, … until one does not exist.
fn open_buffers(debug: &Path) -> Result<Vec<Buffer>, ProfileError> {
    let mut buffers = Vec::new();
    loop {
        let path = debug.join(format!("{CPU_FILE_PREFIX}{}", buffers.len()));
        match File::open(&path) {
            Ok(file) => buffers.push(Buffer { path, file }),
            Err(err) if err.kind() == ErrorKind::NotFound => break,
            Err(source) => return Err(ProfileError::Io { path, source }),
        }
    }
    if buffers.is_empty() {
        return Err(ProfileError::NoBuffers {
            path: debug.to_owned(),
        });
    }
    Ok(buffers)
}

/// Copies everything the buffer holds now into `out`.
fn drain(buffer: &mut Buffer, out: &mut impl Write) -> Result<(), ProfileError> {
    let mut chunk = vec![0; READ_BYTES];
    loop {
        let n = match buffer.file.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(ProfileError::Io {
                    path: buffer.path.clone(),
                    source,
                });
            }
        };
        out.write_all(&chunk[..n])
            .map_err(|source| ProfileError::Io {
                path: buffer.path.clone(),
                source,
            })?;
    }
}

fn drain_all(cpus: &mut [Buffer], files: &mut [(PathBuf, File)]) -> Result<(), ProfileError> {
    for (buffer, (path, file)) in cpus.iter_mut().zip(files.iter_mut()) {
        drain(buffer, file)?;
        file.flush().map_err(io_error(path))?;
    }
    Ok(())
}

/// The profiler, owned through its control file from [`Sampling::claim`] until dropped;
/// sampling is on from [`Sampling::start`] until [`Sampling::stop`]. Dropping it unstopped (on
/// an error path) switches sampling off too, as does the kernel when the file closes.
struct Sampling {
    path: PathBuf,
    ctl: Option<File>,
}

impl Sampling {
    /// Opens the control file and takes the profiler (`x`), which also stops sampling.
    fn claim(path: &Path) -> Result<Self, ProfileError> {
        let mut ctl = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(io_error(path))?;
        ctl.write_all(b"x").map_err(|source| {
            if source.kind() == ErrorKind::ResourceBusy {
                ProfileError::Busy {
                    path: path.to_owned(),
                }
            } else {
                io_error(path)(source)
            }
        })?;
        Ok(Self {
            path: path.to_owned(),
            ctl: Some(ctl),
        })
    }

    /// Switches sampling on.
    fn start(&mut self) -> Result<(), ProfileError> {
        match &mut self.ctl {
            Some(ctl) => ctl.write_all(b"1").map_err(io_error(&self.path)),
            None => Ok(()),
        }
    }

    fn stop(mut self) -> Result<(), ProfileError> {
        match self.ctl.take() {
            Some(mut ctl) => ctl.write_all(b"0").map_err(io_error(&self.path)),
            None => Ok(()),
        }
    }
}

impl Drop for Sampling {
    fn drop(&mut self) {
        if let Some(ctl) = &mut self.ctl {
            // Only reached while another error is being returned, which is the one to report.
            let _ = ctl.write_all(b"0");
        }
    }
}

/// Samples for the run, draining as it goes.
fn sample(
    run: &Run,
    cpus: &mut [Buffer],
    files: &mut [(PathBuf, File)],
) -> Result<Option<ExitStatus>, ProfileError> {
    match run {
        Run::For(length) => {
            let end = deadline(*length)?;
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                thread::sleep(left.min(DRAIN_INTERVAL));
                drain_all(cpus, files)?;
            }
            Ok(None)
        }
        Run::Command(argv) => run_command(argv, || drain_all(cpus, files)).map(Some),
    }
}

/// When sampling for `length` from now ends.
fn deadline(length: Duration) -> Result<Instant, ProfileError> {
    Instant::now()
        .checked_add(length)
        .ok_or(ProfileError::TooLong { length })
}

/// Runs `argv` and calls `tick` every [`DRAIN_INTERVAL`] until it exits. If `tick` or waiting
/// fails, the command is killed and waited for, and that first error is returned.
fn run_command(
    argv: &[OsString],
    mut tick: impl FnMut() -> Result<(), ProfileError>,
) -> Result<ExitStatus, ProfileError> {
    let (program, args) = argv.split_first().ok_or(ProfileError::NoCommand)?;
    let command_error = |source| ProfileError::Command {
        program: program.clone(),
        source,
    };
    let mut child = Command::new(program)
        .args(args)
        .spawn()
        .map_err(command_error)?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(source) => return Err(stop_child(&mut child, command_error(source))),
        }
        thread::sleep(DRAIN_INTERVAL);
        if let Err(err) = tick() {
            return Err(stop_child(&mut child, err));
        }
    }
}

/// Kills and reaps `child` after `err`, which stays the error reported: a failure to kill
/// (it may have exited already) or to wait would only hide it.
fn stop_child(child: &mut Child, err: ProfileError) -> ProfileError {
    let _ = child.kill();
    let _ = child.wait();
    err
}

/// Names every context the saved samples mention.
fn name_contexts(
    cpus: &[Buffer],
    sample_files: &[PathBuf],
) -> Result<BTreeMap<u32, String>, ProfileError> {
    let mut ids = BTreeSet::new();
    for path in sample_files {
        let words = load_words(path)?;
        ids.extend(
            Samples::new(&words)
                .filter_map(Result::ok)
                .map(|s| s.context),
        );
    }
    let Some(buffer) = cpus.first() else {
        return Ok(BTreeMap::new());
    };
    let mut names = BTreeMap::new();
    for id in ids {
        match context_name(&buffer.file, id) {
            Ok(Some(name)) => {
                names.insert(id, name);
            }
            Ok(None) => {}
            Err(source) => {
                return Err(ProfileError::Io {
                    path: buffer.path.clone(),
                    source,
                });
            }
        }
    }
    Ok(names)
}

/// The kernel's name for the context `id`, from a call on a CPU file; `None` once the
/// context is gone.
#[cfg(target_os = "redox")]
fn context_name(file: &File, id: u32) -> io::Result<Option<String>> {
    use std::os::fd::AsRawFd;
    use syscall::flag::CallFlags;

    let fd =
        usize::try_from(file.as_raw_fd()).map_err(|_| io::Error::from(ErrorKind::InvalidInput))?;
    let mut name = [0u8; NAME_BYTES];
    match syscall::call_ro(fd, &mut name, CallFlags::empty(), &[u64::from(id)]) {
        Ok(len) => Ok(Some(
            String::from_utf8_lossy(name.get(..len).unwrap_or(&name)).into_owned(),
        )),
        Err(err) if err.errno == syscall::error::ENOENT => Ok(None),
        Err(err) => Err(io::Error::from_raw_os_error(err.errno)),
    }
}

/// Elsewhere there is no debug scheme to ask.
#[cfg(not(target_os = "redox"))]
fn context_name(_file: &File, _id: u32) -> io::Result<Option<String>> {
    let _ = NAME_BYTES;
    Ok(None)
}

/// The names file's text: one `<id> <name>` line each, newlines in names made spaces.
fn names_file(names: &BTreeMap<u32, String>) -> String {
    names
        .iter()
        .map(|(id, name)| format!("{id} {}\n", name.replace('\n', " ")))
        .collect()
}

fn parse_names(path: &Path, text: &str) -> Result<BTreeMap<u32, String>, ProfileError> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once(' ')
                .and_then(|(id, name)| Some((id.parse().ok()?, name.to_owned())))
                .ok_or_else(|| ProfileError::Names {
                    path: path.to_owned(),
                    line: line.to_owned(),
                })
        })
        .collect()
}

/// The CPU numbers of the `samples<n>` files in `dir`, in order.
fn sample_cpus(dir: &Path) -> Result<Vec<usize>, ProfileError> {
    let mut cpus = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_error(dir))? {
        let entry = entry.map_err(io_error(dir))?;
        let name = entry.file_name();
        let cpu = name
            .to_str()
            .and_then(|name| name.strip_prefix(SAMPLES_PREFIX))
            .and_then(|n| n.parse::<usize>().ok());
        cpus.extend(cpu);
    }
    cpus.sort_unstable();
    Ok(cpus)
}

fn load_words(path: &Path) -> Result<Vec<u64>, ProfileError> {
    let bytes = fs::read(path).map_err(io_error(path))?;
    if bytes.len() % WORD_BYTES != 0 {
        return Err(ProfileError::TornWord {
            path: path.to_owned(),
            len: bytes.len(),
        });
    }
    Ok(bytes
        .chunks_exact(WORD_BYTES)
        .map(|w| u64::from_le_bytes(w.try_into().expect("chunks_exact gives whole words")))
        .collect())
}

/// A profile loaded on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Where the kernel's text was at run time.
    pub kernel_text: ControlLine,
    /// The sample words of each CPU, in CPU order.
    pub cpus: Vec<Vec<u64>>,
    /// The contexts' names, by debug id.
    pub names: BTreeMap<u32, String>,
}

impl Profile {
    /// Loads the profile directory `dir`. A profile marked `INCOMPLETE` loads only with
    /// `partial`; its names file may then be missing.
    ///
    /// # Errors
    ///
    /// [`ProfileError::Incomplete`], [`ProfileError::Control`], [`ProfileError::TornWord`],
    /// [`ProfileError::MissingSamples`] when the CPUs' files are not numbered from 0 without
    /// gaps, [`ProfileError::Names`], or [`ProfileError::Io`] naming the file.
    pub fn load(dir: &Path, partial: bool) -> Result<Self, ProfileError> {
        let incomplete = dir.join(INCOMPLETE).exists();
        if incomplete && !partial {
            return Err(ProfileError::Incomplete {
                path: dir.to_owned(),
            });
        }
        let text_path = dir.join(KERNEL_TEXT);
        let text = fs::read_to_string(&text_path).map_err(io_error(&text_path))?;
        let kernel_text = parse_control(&text_path, &text)?;
        let mut cpus = Vec::new();
        for (index, cpu) in sample_cpus(dir)?.into_iter().enumerate() {
            if cpu != index {
                return Err(ProfileError::MissingSamples {
                    path: dir.to_owned(),
                    cpu: index,
                });
            }
            cpus.push(load_words(&dir.join(format!("{SAMPLES_PREFIX}{cpu}")))?);
        }
        let names_path = dir.join(NAMES);
        let names = match fs::read_to_string(&names_path) {
            Ok(text) => parse_names(&names_path, &text)?,
            Err(err) if incomplete && err.kind() == ErrorKind::NotFound => BTreeMap::new(),
            Err(source) => {
                return Err(ProfileError::Io {
                    path: names_path,
                    source,
                });
            }
        };
        Ok(Self {
            kernel_text,
            cpus,
            names,
        })
    }

    /// Every sample with its CPU, CPU by CPU. Words that are not a whole sample come as one
    /// error each run of them, and the samples after them follow ([`Samples`]).
    pub fn samples(&self) -> impl Iterator<Item = (usize, Result<Sample<'_>, SampleError>)> {
        self.cpus
            .iter()
            .enumerate()
            .flat_map(|(cpu, words)| Samples::new(words).map(move |s| (cpu, s)))
    }
}

#[cfg(test)]
mod tests {
    use ferric_trace::profile::SAMPLE_MAGIC;

    use super::*;

    fn bytes(words: &[u64]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn save(dir: &Path, cpus: &[&[u64]], names: &str) {
        fs::write(dir.join(KERNEL_TEXT), "kernel_text 0xffffffff80200000\n").unwrap();
        for (cpu, words) in cpus.iter().enumerate() {
            fs::write(dir.join(format!("{SAMPLES_PREFIX}{cpu}")), bytes(words)).unwrap();
        }
        fs::write(dir.join(NAMES), names).unwrap();
    }

    #[test]
    fn loads_a_profile() {
        let dir = tempfile::tempdir().unwrap();
        save(
            dir.path(),
            &[
                &[SAMPLE_MAGIC | 5, 0x40_1000, 1, 7, 0x40_2000],
                &[SAMPLE_MAGIC | 4, 0x40_3000, 2, 8],
            ],
            "7 /usr/bin/ion\n8 a name with spaces\n",
        );
        let profile = Profile::load(dir.path(), false).unwrap();
        assert_eq!(profile.kernel_text.text_start, 0xffff_ffff_8020_0000);
        assert_eq!(profile.names[&8], "a name with spaces");
        let samples: Vec<_> = profile.samples().collect();
        assert_eq!(samples.len(), 2);
        let (cpu, first) = &samples[0];
        assert_eq!((*cpu, first.unwrap().frames), (0, &[0x40_2000][..]));
        assert_eq!(samples[1].0, 1);
    }

    #[test]
    fn refuses_bad_profiles() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &[&[]], "7 x\n");
        fs::write(dir.path().join(INCOMPLETE), "").unwrap();
        assert!(matches!(
            Profile::load(dir.path(), false),
            Err(ProfileError::Incomplete { .. })
        ));
        fs::remove_file(dir.path().join(NAMES)).unwrap();
        assert!(Profile::load(dir.path(), true).unwrap().names.is_empty());
        fs::remove_file(dir.path().join(INCOMPLETE)).unwrap();
        fs::write(dir.path().join(NAMES), "seven x\n").unwrap();
        assert!(matches!(
            Profile::load(dir.path(), false),
            Err(ProfileError::Names { .. })
        ));
        fs::write(dir.path().join(NAMES), "").unwrap();
        fs::write(dir.path().join(format!("{SAMPLES_PREFIX}0")), [0u8; 12]).unwrap();
        assert!(matches!(
            Profile::load(dir.path(), false),
            Err(ProfileError::TornWord { len: 12, .. })
        ));
    }

    #[test]
    fn names_round_trip() {
        let names = BTreeMap::from([(3, "init".to_owned()), (9, "two\nlines".to_owned())]);
        let text = names_file(&names);
        assert_eq!(text, "3 init\n9 two lines\n");
        let back = parse_names(Path::new("names"), &text).unwrap();
        assert_eq!(back[&9], "two lines");
    }

    /// The whole recording against a stand-in debug scheme of plain files: a sample file is
    /// read to its end, so the first (discarding) drain empties it and the command's appended
    /// words are what is saved.
    #[test]
    fn records_from_a_scheme() {
        let scheme = tempfile::tempdir().unwrap();
        fs::write(scheme.path().join(CTL_FILE), "kernel_text 0x1000\n").unwrap();
        let cpu0 = scheme.path().join(format!("{CPU_FILE_PREFIX}0"));
        fs::write(&cpu0, bytes(&[SAMPLE_MAGIC | 4, 1, 1, 1])).unwrap();
        let fresh = scheme.path().join("fresh");
        fs::write(&fresh, bytes(&[SAMPLE_MAGIC | 4, 2, 2, 9])).unwrap();

        let out = tempfile::tempdir().unwrap();
        let dir = out.path().join("profile");
        let append = format!("cat {} >> {}", fresh.display(), cpu0.display());
        let run = Run::Command(vec!["sh".into(), "-c".into(), append.into()]);
        let recorded = record(scheme.path(), &dir, &run).unwrap();
        assert!(recorded.status.unwrap().success());
        assert_eq!(recorded.cpu_bytes, [32]);

        let profile = Profile::load(&dir, false).unwrap();
        assert_eq!(profile.kernel_text.text_start, 0x1000);
        let only = profile.samples().next().unwrap().1.unwrap();
        assert_eq!((only.ip, only.context), (2, 9));
        // The profiler was claimed, then sampling switched on and off (each write lands after
        // the last in a plain file).
        let ctl = fs::read_to_string(scheme.path().join(CTL_FILE)).unwrap();
        assert!(ctl.starts_with("x10"), "{ctl:?}");
    }

    /// A stand-in debug scheme with one empty CPU buffer.
    fn stand_in_scheme() -> tempfile::TempDir {
        let scheme = tempfile::tempdir().unwrap();
        fs::write(scheme.path().join(CTL_FILE), "kernel_text 0x1000\n").unwrap();
        fs::write(scheme.path().join(format!("{CPU_FILE_PREFIX}0")), b"").unwrap();
        scheme
    }

    #[test]
    fn record_makes_the_parents() {
        let scheme = stand_in_scheme();
        let out = scheme.path().join("a/b/profile");
        record(scheme.path(), &out, &Run::For(Duration::ZERO)).unwrap();
        assert!(Profile::load(&out, false).is_ok());
    }

    #[test]
    fn record_refuses_an_endless_length() {
        let scheme = stand_in_scheme();
        let out = scheme.path().join("profile");
        assert!(matches!(
            record(scheme.path(), &out, &Run::For(Duration::MAX)),
            Err(ProfileError::TooLong { .. })
        ));
        assert!(!out.exists());
    }

    /// A failed drain kills and reaps the command, and is the error returned.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_drain_stops_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > {}; exec sleep 30", pid_file.display());
        let argv = ["sh".into(), "-c".into(), script.into()];
        let started = Instant::now();
        let result = run_command(&argv, || match fs::read_to_string(&pid_file) {
            Ok(pid) if pid.ends_with('\n') => Err(ProfileError::NoCommand),
            _ => Ok(()),
        });
        assert!(matches!(result, Err(ProfileError::NoCommand)));
        assert!(started.elapsed() < Duration::from_secs(20));
        let pid = fs::read_to_string(&pid_file).unwrap();
        assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists());
    }

    #[test]
    fn load_refuses_a_gap_in_the_cpus() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &[&[], &[], &[]], "");
        fs::remove_file(dir.path().join(format!("{SAMPLES_PREFIX}1"))).unwrap();
        assert!(matches!(
            Profile::load(dir.path(), false),
            Err(ProfileError::MissingSamples { cpu: 1, .. })
        ));
    }

    #[test]
    fn record_needs_the_profiler() {
        let scheme = tempfile::tempdir().unwrap();
        fs::write(scheme.path().join(CTL_FILE), "kernel_text 0x1000\n").unwrap();
        let out = scheme.path().join("profile");
        assert!(matches!(
            record(scheme.path(), &out, &Run::For(Duration::ZERO)),
            Err(ProfileError::NoBuffers { .. })
        ));
        assert!(!out.exists());
    }
}
