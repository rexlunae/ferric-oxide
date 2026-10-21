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
//!   names it (the `debug` scheme's `call` on a CPU file);
//! - [`IMAGES`]: for each context that had any, its `context` line and `image` lines from
//!   `/scheme/sys/proc` (`kernel.sys_proc`, in the `ferric-procinfo` format): where each ELF
//!   image was loaded, which symbolizes user addresses under ASLR. Missing without `sys_proc`.
//! - [`IMAGE_SPANS`]: for each `image` line of [`IMAGES`], in the same order, the
//!   `<context> <first> <last>` snapshots it was seen in ([`Live`]);
//! - [`BATCHES`]: one `<epoch> <words of CPU 0> <words of CPU 1> …` line for each drain, the
//!   length of each `samples<n>` file after it, which dates every sample ([`Batch`]).
//!
//! [`record`] first takes the profiler for itself by writing `x` to `ctl-profiling`, which
//! stops sampling and makes any other recorder's writes fail (`EBUSY`, reported as
//! [`ProfileError::Busy`]) until this one closes its control file. It then drains whatever
//! earlier runs left in the buffers and throws it away, turns sampling on, runs a command or
//! waits, and drains every buffer every [`DRAIN_INTERVAL`] until sampling is off again, so the
//! buffers never fill. It reads `/scheme/sys/proc` before sampling starts, after the first
//! drain, after every [`SNAPSHOT_DRAINS`]th and after the last (a snapshot; the `n`th is
//! epoch `n`) and keeps every image it saw for each
//! context, so programs that exit before the end, and libraries unloaded during the run, are
//! still covered unless they lived less than one interval. Each sighting of an image is an
//! occurrence with the snapshots it was live in; an image that comes back after missing from
//! a snapshot is a new occurrence. The samples drained after snapshot `n - 1` and up to
//! snapshot `n` were taken between the two, so they carry epoch `n`, and the symbolizer resolves a
//! sample against the occurrences live around its epoch ([`LoadedImage::live`]).
//! [`Recorded::snapshots`] says what the snapshots cost. As with trace captures, the directory is created exclusively
//! (its parents as needed) and holds an `INCOMPLETE` marker ([`crate::capture::INCOMPLETE`])
//! until everything is saved. Sampling is turned off on every path out of [`record`], including
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

/// The file listing each context's loaded images.
pub const IMAGES: &str = "images";

/// The file giving the snapshots each line of [`IMAGES`] was seen in.
pub const IMAGE_SPANS: &str = "image-spans";

/// The file dating the samples: the sample files' lengths after each drain.
pub const BATCHES: &str = "batches";

/// How often [`record`] drains the buffers while sampling.
pub const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

/// [`record`] takes an image snapshot after the first drain and then after every
/// `SNAPSHOT_DRAINS`th, so about every half second. Reading `/scheme/sys/proc` took about
/// 1.6 ms on the CI image (review 4320670273), 1.6% of the time at every drain; this keeps it
/// near 0.3%. An image that lives less than the interval may be missed.
pub const SNAPSHOT_DRAINS: u32 = 5;

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
    /// How many contexts' images were saved (none without `/scheme/sys/proc`).
    pub images: usize,
    /// How often `/scheme/sys/proc` was read for images, and the time that took in all: the
    /// recorder's cost on the profiled system, beside draining.
    pub snapshots: Snapshots,
    /// The command's exit status, for [`Run::Command`].
    pub status: Option<ExitStatus>,
}

/// The image snapshots of one recording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshots {
    /// Reads of `/scheme/sys/proc`.
    pub count: u32,
    /// Their total time, the kernel writing the file included.
    pub total: Duration,
    /// The longest one.
    pub longest: Duration,
    /// The wall time from the start of the first to the end of the last.
    pub span: Duration,
}

impl Snapshots {
    /// The share of [`Snapshots::span`] spent taking snapshots, in parts per million; 0 when
    /// the span is empty.
    pub fn duty_ppm(&self) -> u64 {
        if self.span.is_zero() {
            return 0;
        }
        (self.total.as_nanos() * 1_000_000 / self.span.as_nanos()) as u64
    }
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
    /// The per-process file (or a profile's images file) does not parse.
    #[error("{}", path.display())]
    Proc {
        /// The file.
        path: PathBuf,
        /// Why it does not parse.
        #[source]
        source: ferric_procinfo::FileError,
    },
    /// A line of the names file is not `<id> <name>`.
    #[error("{}: bad line {line:?}", path.display())]
    Names {
        /// The file.
        path: PathBuf,
        /// The line.
        line: String,
    },
    /// A line of the image spans file is not `<context> <first> <last>` with
    /// `first <= last`, or a batches line is not an epoch and one length per CPU.
    #[error("{}: bad line {line:?}", path.display())]
    Dates {
        /// The file.
        path: PathBuf,
        /// The line.
        line: String,
    },
    /// The image spans file does not give one span for each of a context's images.
    #[error("{}: the spans of context {context} do not match its images", path.display())]
    SpanCount {
        /// The spans file.
        path: PathBuf,
        /// The context.
        context: u32,
    },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> ProfileError + '_ {
    move |source| ProfileError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Records a profile into `out`, which must not exist, from the debug scheme at `debug`
/// (`/scheme/debug`), with the images listed by the per-process file `proc`
/// (`/scheme/sys/proc`; skipped if it does not exist). Needs root, and a kernel with
/// `kernel.profiling`.
///
/// # Errors
///
/// [`ProfileError::NoBuffers`] without the profiler, [`ProfileError::Control`] if
/// `ctl-profiling` gives no control line (a kernel without the read), [`ProfileError::TooLong`]
/// for a length past what the clock can count to, [`ProfileError::Busy`] while another
/// recorder runs, [`ProfileError::Command`] if the command does not start or cannot be waited
/// for, [`ProfileError::Proc`] if `proc` does not parse, and [`ProfileError::Io`] naming the
/// file for any other failure. Whatever was saved stays in `out` with its `INCOMPLETE` marker.
pub fn record(debug: &Path, proc: &Path, out: &Path, run: &Run) -> Result<Recorded, ProfileError> {
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
    let mut images = ImageTracker::new(proc);
    let mut batches = String::new();
    let mut drains = 0u32;
    images.snapshot()?;
    sampling.start()?;
    let status = sample(run, &mut || {
        drain_all(&mut cpus, &mut files)?;
        batches.push_str(&batch_line(images.epoch(), &files)?);
        drains = drains.wrapping_add(1);
        if drains == 1 || drains.is_multiple_of(SNAPSHOT_DRAINS) {
            images.snapshot()?;
        }
        Ok(())
    })?;
    sampling.stop()?;
    drain_all(&mut cpus, &mut files)?;
    batches.push_str(&batch_line(images.epoch(), &files)?);
    images.snapshot()?;
    write_file(&out.join(IMAGES), images.text().as_bytes())?;
    write_file(&out.join(IMAGE_SPANS), images.spans().as_bytes())?;
    write_file(&out.join(BATCHES), batches.as_bytes())?;
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
        images: images.blocks.len(),
        snapshots: images.timing,
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

/// The [`BATCHES`] line for a drain before snapshot `epoch`: the epoch, then each sample
/// file's length in words.
fn batch_line(epoch: u32, files: &[(PathBuf, File)]) -> Result<String, ProfileError> {
    let mut line = epoch.to_string();
    for (path, file) in files {
        let bytes = file.metadata().map_err(io_error(path))?.len();
        line.push_str(&format!(" {}", bytes / WORD_BYTES as u64));
    }
    line.push('\n');
    Ok(line)
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

/// The image blocks of the per-process file: for each context, its line and every occurrence
/// of an image in the snapshots, in the order first seen. A library unloaded during the
/// recording keeps its occurrence, for the samples taken while it was mapped; one that comes
/// back after missing from a snapshot (A, then B, then A again at one address) is a new one.
struct ImageTracker {
    path: PathBuf,
    /// Off once the file turned out not to exist (a kernel without `sys_proc`).
    on: bool,
    blocks: BTreeMap<u32, (String, Vec<Occurrence>)>,
    /// Snapshots taken so far: the next one's epoch.
    taken: u32,
    timing: Snapshots,
    /// When the first snapshot started.
    first: Option<Instant>,
}

/// An image line seen in the snapshots `first..=last`, every one of them.
#[derive(Debug)]
struct Occurrence {
    image: String,
    live: Live,
}

impl ImageTracker {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            on: true,
            blocks: BTreeMap::new(),
            taken: 0,
            timing: Snapshots::default(),
            first: None,
        }
    }

    /// The epoch of the next snapshot.
    fn epoch(&self) -> u32 {
        self.taken
    }

    fn snapshot(&mut self) -> Result<(), ProfileError> {
        let epoch = self.taken;
        self.taken = self.taken.saturating_add(1);
        if !self.on {
            return Ok(());
        }
        let started = Instant::now();
        let taken = self.take(epoch);
        let took = started.elapsed();
        self.timing.count = self.timing.count.saturating_add(1);
        self.timing.total = self.timing.total.saturating_add(took);
        self.timing.longest = self.timing.longest.max(took);
        self.timing.span = self.first.get_or_insert(started).elapsed();
        taken
    }

    fn take(&mut self, epoch: u32) -> Result<(), ProfileError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                self.on = false;
                return Ok(());
            }
            Err(source) => return Err(io_error(&self.path)(source)),
        };
        let processes = ferric_procinfo::processes(&text).map_err(|source| ProfileError::Proc {
            path: self.path.clone(),
            source,
        })?;
        for process in processes.iter().filter(|p| !p.images.is_empty()) {
            let (line, seen) = self.blocks.entry(process.context.id).or_default();
            *line = process.context.to_string();
            for image in &process.images {
                let image = image.to_string();
                // Still live if it was in the snapshot before (or already in this one).
                let current = seen.iter_mut().rev().find(|o| {
                    o.image == image
                        && (o.live.last == epoch || o.live.last.checked_add(1) == Some(epoch))
                });
                match current {
                    Some(occurrence) => occurrence.live.last = epoch,
                    None => seen.push(Occurrence {
                        image,
                        live: Live {
                            first: epoch,
                            last: epoch,
                        },
                    }),
                }
            }
        }
        Ok(())
    }

    /// The [`IMAGES`] file: each context line, then a line for each occurrence.
    fn text(&self) -> String {
        self.blocks
            .values()
            .flat_map(|(line, seen)| {
                core::iter::once(line.as_str()).chain(seen.iter().map(|o| o.image.as_str()))
            })
            .collect()
    }

    /// The [`IMAGE_SPANS`] file, in the order of [`ImageTracker::text`]'s image lines.
    fn spans(&self) -> String {
        self.blocks
            .iter()
            .flat_map(|(context, (_, seen))| {
                seen.iter()
                    .map(move |o| format!("{context} {} {}\n", o.live.first, o.live.last))
            })
            .collect()
    }
}

/// Samples for the run, calling `tick` (which drains) every [`DRAIN_INTERVAL`].
fn sample(
    run: &Run,
    tick: &mut dyn FnMut() -> Result<(), ProfileError>,
) -> Result<Option<ExitStatus>, ProfileError> {
    match run {
        Run::For(length) => {
            let end = deadline(*length)?;
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                thread::sleep(left.min(DRAIN_INTERVAL));
                tick()?;
            }
            Ok(None)
        }
        Run::Command(argv) => run_command(argv, tick).map(Some),
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
    /// The images loaded into each context's address space, by debug id, oldest first.
    pub images: BTreeMap<u32, Vec<LoadedImage>>,
    /// Each CPU's batches, which date its samples; empty for a profile saved without
    /// [`BATCHES`].
    pub batches: Vec<Vec<Batch>>,
}

/// One sample of a [`Profile`], or a run of words that is not one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSample<'a> {
    /// The CPU that took it.
    pub cpu: usize,
    /// The epoch of its batch, if the profile dates its samples.
    pub epoch: Option<u32>,
    /// The sample.
    pub sample: Result<Sample<'a>, SampleError>,
}

/// An ELF image in a profiled context's address space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedImage {
    /// The load bias: run-time address minus ELF virtual address.
    pub base: u64,
    /// The path it was loaded from, on the target.
    pub path: String,
    /// The snapshots it was seen in, or `None` for a profile saved without [`IMAGE_SPANS`].
    pub live: Option<Live>,
}

/// The snapshots `first..=last` an image occurrence was seen in, every one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Live {
    /// The first snapshot that listed it.
    pub first: u32,
    /// The last one.
    pub last: u32,
}

impl Live {
    /// Whether a sample of epoch `epoch` (taken between snapshots `epoch - 1` and `epoch`)
    /// may have run in the image: it was mapped some time after snapshot `first - 1` and
    /// unmapped before snapshot `last + 1`.
    pub fn covers(&self, epoch: u32) -> bool {
        self.first <= epoch && epoch <= self.last.saturating_add(1)
    }
}

/// The samples of one drain: the words of a CPU's file up to `end`, after the previous batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Batch {
    /// The end of the batch, in words.
    pub end: usize,
    /// The snapshot taken right after the drain: the batch's samples were taken between
    /// snapshot `epoch - 1` and this one.
    pub epoch: u32,
}

fn parse_images(path: &Path, text: &str) -> Result<BTreeMap<u32, Vec<LoadedImage>>, ProfileError> {
    let processes = ferric_procinfo::processes(text).map_err(|source| ProfileError::Proc {
        path: path.to_owned(),
        source,
    })?;
    Ok(processes
        .into_iter()
        .map(|process| {
            let images = process
                .images
                .iter()
                .map(|image| LoadedImage {
                    base: image.base,
                    path: image.path.0.to_owned(),
                    live: None,
                })
                .collect();
            (process.context.id, images)
        })
        .collect())
}

/// Gives each image in `images` its span from the [`IMAGE_SPANS`] file `text`.
fn apply_spans(
    path: &Path,
    text: &str,
    images: &mut BTreeMap<u32, Vec<LoadedImage>>,
) -> Result<(), ProfileError> {
    let bad = |line: &str| ProfileError::Dates {
        path: path.to_owned(),
        line: line.to_owned(),
    };
    let mut spans: BTreeMap<u32, Vec<Live>> = BTreeMap::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.split(' ').map(str::parse::<u32>);
        let (Some(Ok(context)), Some(Ok(first)), Some(Ok(last)), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(bad(line));
        };
        if first > last {
            return Err(bad(line));
        }
        spans.entry(context).or_default().push(Live { first, last });
    }
    for (context, list) in images.iter_mut() {
        let lives = spans.remove(context).unwrap_or_default();
        if lives.len() != list.len() {
            return Err(ProfileError::SpanCount {
                path: path.to_owned(),
                context: *context,
            });
        }
        for (image, live) in list.iter_mut().zip(lives) {
            image.live = Some(live);
        }
    }
    match spans.into_keys().next() {
        Some(context) => Err(ProfileError::SpanCount {
            path: path.to_owned(),
            context,
        }),
        None => Ok(()),
    }
}

/// The [`BATCHES`] file `text`, for `cpus` CPUs: each CPU's batches in order.
fn parse_batches(path: &Path, text: &str, cpus: usize) -> Result<Vec<Vec<Batch>>, ProfileError> {
    let mut batches = vec![Vec::new(); cpus];
    for line in text.lines().filter(|line| !line.is_empty()) {
        let bad = || ProfileError::Dates {
            path: path.to_owned(),
            line: line.to_owned(),
        };
        let mut fields = line.split(' ');
        let epoch = fields
            .next()
            .and_then(|f| f.parse::<u32>().ok())
            .ok_or_else(bad)?;
        let ends = fields
            .map(|f| f.parse::<usize>().map_err(|_| bad()))
            .collect::<Result<Vec<_>, _>>()?;
        if ends.len() != cpus {
            return Err(bad());
        }
        for (cpu, end) in batches.iter_mut().zip(ends) {
            cpu.push(Batch { end, epoch });
        }
    }
    Ok(batches)
}

/// Reads `path`, or `None` when it does not exist.
fn read_optional(path: &Path) -> Result<Option<String>, ProfileError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ProfileError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

impl Profile {
    /// Loads the profile directory `dir`. A profile marked `INCOMPLETE` loads only with
    /// `partial`; its names file may then be missing.
    ///
    /// # Errors
    ///
    /// [`ProfileError::Incomplete`], [`ProfileError::Control`], [`ProfileError::TornWord`],
    /// [`ProfileError::MissingSamples`] when the CPUs' files are not numbered from 0 without
    /// gaps, [`ProfileError::Names`], [`ProfileError::Proc`] for a bad images file, or
    /// [`ProfileError::Io`] naming the file. A missing images file (a kernel without
    /// `sys_proc`) is no error.
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
        let images_path = dir.join(IMAGES);
        let mut images = match read_optional(&images_path)? {
            Some(text) => parse_images(&images_path, &text)?,
            None => BTreeMap::new(),
        };
        let spans_path = dir.join(IMAGE_SPANS);
        if let Some(text) = read_optional(&spans_path)? {
            apply_spans(&spans_path, &text, &mut images)?;
        }
        let batches_path = dir.join(BATCHES);
        let batches = match read_optional(&batches_path)? {
            Some(text) => parse_batches(&batches_path, &text, cpus.len())?,
            None => Vec::new(),
        };
        Ok(Self {
            kernel_text,
            cpus,
            names,
            images,
            batches,
        })
    }

    /// Every sample with its CPU and epoch ([`Batch::epoch`]; `None` without [`BATCHES`]),
    /// CPU by CPU. Words that are not a whole sample come as one error each run of them, and
    /// the samples after them follow ([`Samples`]).
    pub fn samples(&self) -> impl Iterator<Item = ProfileSample<'_>> {
        self.cpus.iter().enumerate().flat_map(move |(cpu, words)| {
            let batches = self.batches.get(cpu).map_or(&[][..], Vec::as_slice);
            let mut samples = Samples::new(words);
            core::iter::from_fn(move || {
                let at = samples.position();
                let sample = samples.next()?;
                let epoch = batches.iter().find(|b| b.end > at).map(|b| b.epoch);
                Some(ProfileSample { cpu, epoch, sample })
            })
        })
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
        let first = &samples[0];
        assert_eq!(
            (first.cpu, first.epoch, first.sample.unwrap().frames),
            (0, None, &[0x40_2000][..])
        );
        assert_eq!(samples[1].cpu, 1);
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
        let proc = scheme.path().join("proc");
        fs::write(
            &proc,
            "context 9 pid 4 euid 0 egid 0 status runnable cpu 0 running yes active_ns 1 sched_ns 1 name sh\n\
             memory private 0 shared 0 kernel 0\n\
             image 0x7000 path /usr/bin/a b\n\
             context 10 pid 5 euid 0 egid 0 status runnable cpu 0 running no active_ns 1 sched_ns 1 name kmain\n",
        )
        .unwrap();
        let recorded = record(scheme.path(), &proc, &dir, &run).unwrap();
        assert_eq!(recorded.images, 1);
        assert!(recorded.status.unwrap().success());
        assert_eq!(recorded.cpu_bytes, [32]);

        let profile = Profile::load(&dir, false).unwrap();
        assert_eq!(profile.kernel_text.text_start, 0x1000);
        let only = profile.samples().next().unwrap();
        // Drained after sampling started, so after the first snapshot (epoch 0).
        let last = recorded.snapshots.count - 1;
        assert!(matches!(only.epoch, Some(1..)), "{:?}", only.epoch);
        let only = only.sample.unwrap();
        assert_eq!((only.ip, only.context), (2, 9));
        assert_eq!(
            profile.images[&9],
            [LoadedImage {
                base: 0x7000,
                path: "/usr/bin/a b".to_owned(),
                live: Some(Live { first: 0, last }),
            }]
        );
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
        let no_proc = scheme.path().join("no-proc");
        record(scheme.path(), &no_proc, &out, &Run::For(Duration::ZERO)).unwrap();
        assert!(Profile::load(&out, false).is_ok());
    }

    #[test]
    fn record_refuses_an_endless_length() {
        let scheme = stand_in_scheme();
        let out = scheme.path().join("profile");
        assert!(matches!(
            record(
                scheme.path(),
                &scheme.path().join("no-proc"),
                &out,
                &Run::For(Duration::MAX)
            ),
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
            record(
                scheme.path(),
                &scheme.path().join("no-proc"),
                &out,
                &Run::For(Duration::ZERO)
            ),
            Err(ProfileError::NoBuffers { .. })
        ));
        assert!(!out.exists());
    }

    const CONTEXT: &str = "context 9 pid 4 euid 0 egid 0 status runnable cpu 0 running yes active_ns 1 sched_ns 1 name sh\n\
                           memory private 0 shared 0 kernel 0\n";

    /// Takes one snapshot with `lib` the image at 0x7000 (none for `None`).
    fn snapshot_with(tracker: &mut ImageTracker, proc: &Path, lib: Option<&str>) {
        let image = lib.map_or(String::new(), |lib| format!("image 0x7000 path {lib}\n"));
        fs::write(proc, format!("{CONTEXT}{image}")).unwrap();
        tracker.snapshot().unwrap();
    }

    #[test]
    fn the_tracker_keeps_each_occurrence_with_its_span() {
        // Reviews 4320670165 and 4320761258: A at 0x7000, then B there, then A again. Each is
        // its own occurrence, in order, with the snapshots it was live in.
        let dir = tempfile::tempdir().unwrap();
        let proc = dir.path().join("proc");
        let mut tracker = ImageTracker::new(&proc);
        for lib in ["/a.so", "/a.so", "/b.so", "/b.so", "/a.so"] {
            snapshot_with(&mut tracker, &proc, Some(lib));
        }
        // A context without images is not listed, so A is missing from that snapshot and
        // comes back as a new occurrence.
        snapshot_with(&mut tracker, &proc, None);
        snapshot_with(&mut tracker, &proc, Some("/a.so"));
        assert_eq!(tracker.epoch(), 7);
        let mut images = parse_images(&proc, &tracker.text()).unwrap();
        apply_spans(Path::new("spans"), &tracker.spans(), &mut images).unwrap();
        let seen: Vec<(&str, Live)> = images[&9]
            .iter()
            .map(|i| (i.path.as_str(), i.live.unwrap()))
            .collect();
        assert_eq!(
            seen,
            [
                ("/a.so", Live { first: 0, last: 1 }),
                ("/b.so", Live { first: 2, last: 3 }),
                ("/a.so", Live { first: 4, last: 4 }),
                ("/a.so", Live { first: 6, last: 6 }),
            ]
        );
        assert_eq!(tracker.timing.count, 7);
        assert!(tracker.timing.longest <= tracker.timing.total);
    }

    #[test]
    fn a_span_covers_the_batches_around_it() {
        let live = Live { first: 2, last: 3 };
        // Batch 2 ran from snapshot 1 to 2 (mapped some time in it); batch 4 from 3 to 4
        // (unmapped some time in it).
        let covered: Vec<u32> = (0..7).filter(|&e| live.covers(e)).collect();
        assert_eq!(covered, [2, 3, 4]);
    }

    #[test]
    fn spans_and_batches_are_checked() {
        let image = |path: &str| LoadedImage {
            base: 0,
            path: path.to_owned(),
            live: None,
        };
        let mut images = BTreeMap::from([(9, vec![image("/a"), image("/b")])]);
        let spans = Path::new("spans");
        assert!(matches!(
            apply_spans(spans, "9 0 1\n", &mut images.clone()),
            Err(ProfileError::SpanCount { context: 9, .. })
        ));
        assert!(matches!(
            apply_spans(spans, "9 0 1\n9 2 3\n4 0 0\n", &mut images.clone()),
            Err(ProfileError::SpanCount { context: 4, .. })
        ));
        for bad in ["9 3 1\n", "9 x 1\n", "9 0 1 2\n", "9 0\n"] {
            assert!(
                matches!(
                    apply_spans(spans, bad, &mut images.clone()),
                    Err(ProfileError::Dates { .. })
                ),
                "{bad:?}"
            );
        }
        apply_spans(spans, "9 0 1\n9 2 3\n", &mut images).unwrap();
        assert_eq!(images[&9][1].live, Some(Live { first: 2, last: 3 }));

        let batches = parse_batches(spans, "1 4 0\n2 9 5\n", 2).unwrap();
        assert_eq!(
            batches[1],
            [Batch { end: 0, epoch: 1 }, Batch { end: 5, epoch: 2 }]
        );
        assert!(parse_batches(spans, "1 4\n", 2).is_err());
        assert!(parse_batches(spans, "x 4 0\n", 2).is_err());
    }

    #[test]
    fn samples_take_the_epoch_of_their_batch() {
        let words = vec![SAMPLE_MAGIC | 4, 1, 1, 9, SAMPLE_MAGIC | 4, 2, 2, 9];
        let profile = Profile {
            kernel_text: ControlLine { text_start: 0 },
            cpus: vec![words],
            names: BTreeMap::new(),
            images: BTreeMap::new(),
            batches: vec![vec![Batch { end: 4, epoch: 1 }, Batch { end: 8, epoch: 3 }]],
        };
        let epochs: Vec<_> = profile.samples().map(|s| s.epoch).collect();
        assert_eq!(epochs, [Some(1), Some(3)]);
    }

    #[test]
    fn snapshot_duty_is_the_share_of_the_span() {
        let snapshots = Snapshots {
            count: 4,
            total: Duration::from_millis(2),
            longest: Duration::from_micros(600),
            span: Duration::from_secs(2), // 2 ms in 2 s is 0.1%
        };
        assert_eq!(snapshots.duty_ppm(), 1000);
        assert_eq!(Snapshots::default().duty_ppm(), 0);
    }

    /// A recording long enough for several drains snapshots after the first and every
    /// [`SNAPSHOT_DRAINS`]th, not after each.
    #[test]
    fn record_snapshots_less_often_than_it_drains() {
        let scheme = stand_in_scheme();
        let proc = scheme.path().join("proc");
        fs::write(
            &proc,
            "context 9 pid 4 euid 0 egid 0 status runnable cpu 0 running yes active_ns 1 sched_ns 1 name sh\n\
             memory private 0 shared 0 kernel 0\n\
             image 0x7000 path /usr/bin/a\n",
        )
        .unwrap();
        let out = scheme.path().join("profile");
        let length = DRAIN_INTERVAL * (2 * SNAPSHOT_DRAINS + 1);
        let recorded = record(scheme.path(), &proc, &out, &Run::For(length)).unwrap();
        let drains = fs::read_to_string(out.join(BATCHES))
            .unwrap()
            .lines()
            .count() as u32;
        // Before sampling, after the first drain, each SNAPSHOT_DRAINSth, and after the last.
        let expected = 2 + drains / SNAPSHOT_DRAINS + 1;
        assert!(drains > SNAPSHOT_DRAINS, "{drains} drains");
        assert!(
            recorded.snapshots.count.abs_diff(expected) <= 1,
            "{} snapshots for {drains} drains",
            recorded.snapshots.count
        );
        assert!(recorded.snapshots.count < drains);
        assert!(recorded.snapshots.span >= recorded.snapshots.total);
    }
}
