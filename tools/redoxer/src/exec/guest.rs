//! Running one command in a prepared guest image as if it ran on the host.
//!
//! A build that runs its tools in a guest (`features.toml` `build.build_tools = "guest"`)
//! needs each tool to behave like a host program: it reads the files its arguments name,
//! writes its outputs where the build expects them, prints exactly what it prints, and
//! exits with its own status. [`run`] provides that over a disk image, because a Redox
//! guest has no file system shared with the host:
//!
//! 1. The image is copied, so every command starts from the prepared environment and
//!    commands running in parallel never share a disk.
//! 2. The command's inputs are written into the copy at the same absolute paths they have
//!    on the host, so its arguments and working directory mean the same thing in the guest.
//! 3. The guest boots and `redoxerd` runs the command in capture mode (see
//!    [`ferric_abi::redoxerd`]): no terminal, the streams kept apart, the exit status
//!    recorded and every result flushed to the disk before the guest stops.
//! 4. The host reads the streams, the status and the files under the command's outputs
//!    back out of the copy, and writes each output file that is new or changed to the host.
//!
//! The image must boot from disk rather than load itself into memory (the installer's
//! default `live_disk = false`), or nothing the guest writes reaches the copy.
//!
//! A guest kernel panic halts the guest without stopping QEMU, so the console is watched
//! and a panic ends the run with [`GuestError::Panicked`] instead of waiting forever. A
//! guest that never gets to the command (an image without the programs its init needs,
//! say) is stopped by the [timeout](GuestCommand::timeout) instead.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use ferric_abi::redoxerd::{
    CaptureRequest, InvalidOutcome, Outcome, CAPTURE, STATUS, STDERR, STDOUT,
};

use super::{
    apply_qemu_args, bios_bootloader, bootloader_firmware, copy_into, firmware_for, installed,
    qemu_default_args, qemu_disk_device, qemu_executable, qemu_has_kvm,
};
use crate::redoxfs::{read_files, write_files, ImageFile};
use crate::target;
use crate::writer::{redoxerd_config_files, GuestEntry};

/// The memory a guest command gets, in MiB. The image boots from disk, so unlike a live
/// image it does not need room for a copy of itself.
const GUEST_MEMORY_MIB: u32 = 2048;
/// What the kernel prints when it panics; the guest halts afterwards.
const KERNEL_PANIC: &[u8] = b"KERNEL PANIC";
/// How much of the console a failure report carries.
const CONSOLE_TAIL: usize = 16 * 1024;
/// How often a running guest is checked for an exit or a panic.
const POLL: Duration = Duration::from_millis(50);

/// One command to run in a guest image.
#[derive(Debug, Clone)]
pub struct GuestCommand {
    image: PathBuf,
    cookbook: PathBuf,
    working_dir: PathBuf,
    program: String,
    args: Vec<String>,
    inputs: Vec<PathBuf>,
    outputs: Vec<PathBuf>,
    timeout: Option<Duration>,
}

impl GuestCommand {
    /// Runs `program` (found on the guest's `PATH` unless it is a path) in `working_dir`,
    /// booting `image`. `cookbook` is the checkout that built the image, which decides
    /// whether it boots through BIOS or UEFI firmware.
    ///
    /// `working_dir` is created in the guest. It is not copied in unless it is also an
    /// input, but it is always an output.
    pub fn new(
        image: impl Into<PathBuf>,
        cookbook: impl Into<PathBuf>,
        working_dir: impl Into<PathBuf>,
        program: impl Into<String>,
    ) -> Self {
        Self {
            image: image.into(),
            cookbook: cookbook.into(),
            working_dir: working_dir.into(),
            program: program.into(),
            args: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            timeout: None,
        }
    }

    /// Stops the guest and fails with [`GuestError::TimedOut`] if the run, boot included,
    /// takes longer than `timeout`. Without one, a guest that never runs the command
    /// waits forever.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Adds arguments to the command.
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Copies the host file or directory at the absolute `path` into the guest at the same
    /// path before the command runs.
    pub fn input(mut self, path: impl Into<PathBuf>) -> Self {
        self.inputs.push(path.into());
        self
    }

    /// Copies each file the command creates or changes at or under the absolute `path`
    /// back to the same path on the host. Files the command deletes are not deleted on
    /// the host.
    pub fn output(mut self, path: impl Into<PathBuf>) -> Self {
        self.outputs.push(path.into());
        self
    }
}

/// What a [`GuestCommand`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestOutput {
    /// How the command ended.
    pub outcome: Outcome,
    /// Its standard output, byte for byte.
    pub stdout: Vec<u8>,
    /// Its standard error, byte for byte.
    pub stderr: Vec<u8>,
    /// The host files written back from the guest, in the order they were written.
    pub written: Vec<PathBuf>,
}

/// Why a [`GuestCommand`] produced no [`GuestOutput`].
///
/// A command that runs and fails is not an error: its status is in
/// [`GuestOutput::outcome`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GuestError {
    /// The image to boot does not exist.
    #[error("guest image {} does not exist; install it before running a command in it", path.display())]
    MissingImage {
        /// The image that was asked for.
        path: PathBuf,
    },
    /// QEMU for the target is not installed.
    #[error("{program} not found, please install it")]
    MissingQemu {
        /// The QEMU executable looked for.
        program: &'static str,
    },
    /// A path or argument cannot be carried into the guest.
    #[error("{value:?} cannot be passed to the guest: {reason}")]
    Unrepresentable {
        /// The path or argument.
        value: String,
        /// Why it cannot.
        reason: &'static str,
    },
    /// A host file could not be read, written or created.
    #[error("{context} {}", path.display())]
    Io {
        /// What was being done.
        context: &'static str,
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The guest image could not be prepared, booted or read.
    #[error("{context}")]
    Image {
        /// What was being done.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The guest kernel panicked.
    #[error("the guest kernel panicked; console:\n{console}")]
    Panicked {
        /// The end of the guest's console.
        console: String,
    },
    /// The run took longer than its [timeout](GuestCommand::timeout).
    #[error("the guest did not finish within {timeout:?}; console:\n{console}")]
    TimedOut {
        /// The timeout.
        timeout: Duration,
        /// The end of the guest's console.
        console: String,
    },
    /// The guest stopped without reporting how the command ended, for example because
    /// `redoxerd` could not start it.
    #[error("the guest stopped without running the command ({status}); console:\n{console}")]
    NoResult {
        /// How QEMU exited.
        status: ExitStatus,
        /// The end of the guest's console.
        console: String,
    },
    /// The recorded status was not one `redoxerd` writes.
    #[error("the guest reported an unreadable status {text:?}")]
    Status {
        /// The status text.
        text: String,
        /// The parse failure.
        #[source]
        source: InvalidOutcome,
    },
}

impl GuestError {
    fn image(context: &'static str, source: anyhow::Error) -> Self {
        Self::Image {
            context,
            source: source.into(),
        }
    }

    fn io(context: &'static str, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io {
            context,
            path,
            source,
        }
    }
}

/// Runs `command` in a copy of its image and returns what it did.
///
/// # Errors
/// Returns a [`GuestError`] when the image is missing, QEMU is not installed, a path or
/// argument cannot be represented in the guest, the image cannot be copied, written, booted
/// or read, the guest kernel panics, or the guest stops without a result.
pub fn run(command: &GuestCommand) -> Result<GuestOutput, GuestError> {
    if !command.image.is_file() {
        return Err(GuestError::MissingImage {
            path: command.image.clone(),
        });
    }
    let qemu = qemu_executable();
    if !installed(qemu).map_err(GuestError::io("unable to look for", qemu))? {
        return Err(GuestError::MissingQemu { program: qemu });
    }

    let outputs: Vec<&Path> = std::iter::once(command.working_dir.as_path())
        .chain(command.outputs.iter().map(PathBuf::as_path))
        .collect();
    let entries = guest_entries(command, &outputs)?;

    let tempdir = tempfile::tempdir().map_err(GuestError::io("unable to create", "a tempdir"))?;
    let image = tempdir.path().join("guest.img");
    copy_sparse(&command.image, &image)?;
    write_files(&image, &entries).map_err(|err| {
        GuestError::image("unable to write the command into the guest image", err)
    })?;

    boot(command, &image, tempdir.path())?;

    let results = read_files(
        &image,
        &[Path::new(STATUS), Path::new(STDOUT), Path::new(STDERR)],
    )
    .map_err(|err| GuestError::image("unable to read the command's results", err))?;
    let result = |path: &str| {
        results
            .iter()
            .find(|file| file.path == Path::new(path))
            .map(|file| file.data.clone())
    };
    let status = String::from_utf8_lossy(&result(STATUS).unwrap_or_default()).into_owned();
    let outcome = status.parse().map_err(|source| GuestError::Status {
        text: status.clone(),
        source,
    })?;

    let files = read_files(&image, &outputs)
        .map_err(|err| GuestError::image("unable to read the command's outputs", err))?;
    let written = write_back(&files)?;

    Ok(GuestOutput {
        outcome,
        stdout: result(STDOUT).unwrap_or_default(),
        stderr: result(STDERR).unwrap_or_default(),
        written,
    })
}

/// The files that put `command` and its inputs into the guest.
fn guest_entries(command: &GuestCommand, outputs: &[&Path]) -> Result<Vec<GuestEntry>, GuestError> {
    let working_dir = guest_path(&command.working_dir)?;
    let mut argv = vec![command.program.clone()];
    argv.extend(command.args.iter().cloned());
    for arg in &argv {
        // `redoxerd` reads one argument per line.
        if arg.contains('\n') {
            return Err(GuestError::Unrepresentable {
                value: arg.clone(),
                reason: "arguments cannot contain a newline",
            });
        }
    }

    let mut entries = redoxerd_config_files(&argv, None)
        .map_err(|err| GuestError::image("unable to describe the command", err.into()))?;
    let mut request = String::new();
    let sync_roots = outputs
        .iter()
        .map(|path| guest_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    CaptureRequest::write(&mut request, working_dir, sync_roots).map_err(|_| {
        GuestError::Unrepresentable {
            value: command.working_dir.display().to_string(),
            reason: "paths cannot be empty or contain a newline",
        }
    })?;
    entries.push(GuestEntry::file(CAPTURE, request.into_bytes(), 0o644));
    entries.push(GuestEntry::directory(&command.working_dir, 0o755));
    for input in &command.inputs {
        guest_path(input)?;
        copy_into(input, input.clone(), &mut entries, 0)
            .map_err(|err| GuestError::image("unable to read an input", err))?;
    }
    Ok(entries)
}

/// `path` as the guest names it: absolute, valid UTF-8 and one line long.
fn guest_path(path: &Path) -> Result<&str, GuestError> {
    let unrepresentable = |reason| GuestError::Unrepresentable {
        value: path.display().to_string(),
        reason,
    };
    if !path.is_absolute() {
        return Err(unrepresentable("guest paths must be absolute"));
    }
    let text = path
        .to_str()
        .ok_or_else(|| unrepresentable("guest paths must be valid UTF-8"))?;
    if text.contains('\n') {
        return Err(unrepresentable("guest paths cannot contain a newline"));
    }
    Ok(text)
}

/// Copies `from` to `to`, keeping the holes of a mostly empty image.
fn copy_sparse(from: &Path, to: &Path) -> Result<(), GuestError> {
    let status = Command::new("cp")
        .arg("--sparse=always")
        .arg(from)
        .arg(to)
        .status()
        .map_err(GuestError::io("unable to copy", from))?;
    if status.success() {
        Ok(())
    } else {
        Err(GuestError::Io {
            context: "unable to copy",
            path: from.to_path_buf(),
            source: io::Error::other(format!("cp exited with {status}")),
        })
    }
}

/// Boots `image` until `redoxerd` stops the guest.
///
/// # Errors
/// Returns [`GuestError::Panicked`] when the guest kernel panics, and
/// [`GuestError::NoResult`] when the guest stops without `redoxerd` reporting success.
fn boot(command: &GuestCommand, image: &Path, scratch: &Path) -> Result<(), GuestError> {
    // The image carries its own boot loader, so the checkout that built it decides the
    // firmware; nothing has to be extracted first.
    let stage = command
        .cookbook
        .join("recipes/core/bootloader/target")
        .join(target())
        .join("stage");
    bootloader_firmware::set(firmware_for(target(), bios_bootloader(&stage).is_some()));

    let console_path = scratch.join("console.log");
    let debug_path = scratch.join("debug.log");
    let console =
        File::create(&console_path).map_err(GuestError::io("unable to create", &console_path))?;
    let console_err = console
        .try_clone()
        .map_err(GuestError::io("unable to share", &console_path))?;

    let mut args =
        qemu_default_args().map_err(|err| GuestError::image("unable to configure QEMU", err))?;
    args.extend([
        "-chardev".to_string(),
        format!("file,id=log,path={}", debug_path.display()),
        "-drive".to_string(),
        format!(
            "file={},format=raw,if={}",
            image.display(),
            qemu_disk_device()
        ),
        "-nographic".to_string(),
        "-vga".to_string(),
        "none".to_string(),
    ]);
    if qemu_has_kvm() {
        args.extend(["-accel".to_string(), "kvm".to_string()]);
    }
    let memory = GUEST_MEMORY_MIB.to_string();
    let mut qemu = Command::new(qemu_executable());
    apply_qemu_args(
        &mut qemu,
        args.iter().map(String::as_str).collect(),
        Some(vec!["-m", &memory]),
    );
    let child = qemu
        .stdin(Stdio::null())
        .stdout(console)
        .stderr(console_err)
        .spawn()
        .map_err(GuestError::io("unable to start", qemu_executable()))?;

    let status = watch(child, &console_path, command.timeout)?;
    // `redoxerd` stops the guest with 51 when it ran the command and 53 when it could not.
    if status.code() == Some(51) {
        return Ok(());
    }
    let mut report = console_tail(&console_path)?;
    report.push_str(&console_tail(&debug_path).unwrap_or_default());
    Err(GuestError::NoResult {
        status,
        console: report,
    })
}

/// Waits for QEMU to exit, killing it if the guest's console shows a kernel panic or the
/// run outlasts `timeout`.
fn watch(
    mut child: Child,
    console_path: &Path,
    timeout: Option<Duration>,
) -> Result<ExitStatus, GuestError> {
    let started = Instant::now();
    let mut console =
        File::open(console_path).map_err(GuestError::io("unable to read", console_path))?;
    let mut scanned = Vec::new();
    loop {
        let exited = child
            .try_wait()
            .map_err(GuestError::io("unable to wait for", qemu_executable()))?;
        // Read what arrived before deciding, so a panic printed just before QEMU exits
        // is still reported as one.
        let mut fresh = Vec::new();
        console
            .read_to_end(&mut fresh)
            .map_err(GuestError::io("unable to read", console_path))?;
        // Keep enough of the previous read to find a marker split across two reads.
        let keep = scanned.len().saturating_sub(KERNEL_PANIC.len());
        scanned.drain(..keep);
        scanned.extend_from_slice(&fresh);
        if contains(&scanned, KERNEL_PANIC) {
            if exited.is_none() {
                // The kernel has halted; the rest of its report is already on the console.
                std::thread::sleep(Duration::from_millis(500));
                stop(&mut child)?;
            }
            return Err(GuestError::Panicked {
                console: console_tail(console_path)?,
            });
        }
        if let Some(status) = exited {
            return Ok(status);
        }
        if let Some(timeout) = timeout.filter(|timeout| started.elapsed() > *timeout) {
            stop(&mut child)?;
            return Err(GuestError::TimedOut {
                timeout,
                console: console_tail(console_path)?,
            });
        }
        std::thread::sleep(POLL);
    }
}

fn stop(child: &mut Child) -> Result<(), GuestError> {
    child
        .kill()
        .map_err(GuestError::io("unable to stop", qemu_executable()))?;
    child
        .wait()
        .map_err(GuestError::io("unable to wait for", qemu_executable()))?;
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The last [`CONSOLE_TAIL`] bytes of the console at `path`, as text.
fn console_tail(path: &Path) -> Result<String, GuestError> {
    let mut file = File::open(path).map_err(GuestError::io("unable to read", path))?;
    let length = file
        .seek(SeekFrom::End(0))
        .map_err(GuestError::io("unable to read", path))?;
    file.seek(SeekFrom::Start(length.saturating_sub(CONSOLE_TAIL as u64)))
        .map_err(GuestError::io("unable to read", path))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)
        .map_err(GuestError::io("unable to read", path))?;
    Ok(String::from_utf8_lossy(&tail).into_owned())
}

/// Writes each of `files` whose host copy is missing or different to the host, with the
/// guest's permission bits, and returns the paths written.
fn write_back(files: &[ImageFile]) -> Result<Vec<PathBuf>, GuestError> {
    let mut written = Vec::new();
    for file in files {
        if fs::read(&file.path).is_ok_and(|host| host == file.data) {
            continue;
        }
        if let Some(parent) = file.path.parent() {
            fs::create_dir_all(parent).map_err(GuestError::io("unable to create", parent))?;
        }
        fs::write(&file.path, &file.data).map_err(GuestError::io("unable to write", &file.path))?;
        fs::set_permissions(
            &file.path,
            fs::Permissions::from_mode(u32::from(file.mode & 0o777)),
        )
        .map_err(GuestError::io("unable to set the mode of", &file.path))?;
        written.push(file.path.clone());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_marker_is_found_across_reads() {
        assert!(contains(b"...KERNEL PANIC: x", KERNEL_PANIC));
        assert!(!contains(b"KERNEL PANI", KERNEL_PANIC));
        assert!(!contains(b"", KERNEL_PANIC));
    }

    #[test]
    fn guest_paths_must_be_absolute_utf8_lines() {
        assert_eq!(guest_path(Path::new("/a/b")).unwrap(), "/a/b");
        assert!(guest_path(Path::new("a/b")).is_err());
        assert!(guest_path(Path::new("/a\nb")).is_err());
    }

    /// The request names the working directory and asks for it and each output to be
    /// flushed; the inputs arrive at their host paths and the command goes to `redoxerd`.
    #[test]
    fn the_command_and_its_inputs_become_guest_entries() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("in.gperf");
        fs::write(&input, b"%%\nfoo\n").unwrap();
        let working_dir = temp.path().join("build");
        let output = temp.path().join("out.h");
        let command = GuestCommand::new("/img", "/cookbook", &working_dir, "gperf")
            .args(["-L", "ANSI-C"])
            .input(&input)
            .output(&output);
        let outputs = [working_dir.as_path(), output.as_path()];
        let entries = guest_entries(&command, &outputs).unwrap();

        let find = |path: &Path| entries.iter().find(|entry| entry.path == path).unwrap();
        assert_eq!(
            find(Path::new(ferric_abi::redoxerd::COMMAND)).data,
            b"gperf\n-L\nANSI-C\n"
        );
        let request = String::from_utf8(find(Path::new(CAPTURE)).data.clone()).unwrap();
        let request = CaptureRequest::parse(&request).unwrap();
        assert_eq!(request.working_dir(), working_dir.to_str().unwrap());
        assert_eq!(
            request.sync_roots().collect::<Vec<_>>(),
            [working_dir.to_str().unwrap(), output.to_str().unwrap()]
        );
        assert_eq!(find(&input).data, b"%%\nfoo\n");
        assert_eq!(find(&working_dir).kind, crate::writer::GuestKind::Directory);
    }

    #[test]
    fn an_argument_with_a_newline_is_refused() {
        let command = GuestCommand::new("/img", "/cookbook", "/work", "tool").args(["a\nb"]);
        assert!(matches!(
            guest_entries(&command, &[Path::new("/work")]),
            Err(GuestError::Unrepresentable { .. })
        ));
    }

    /// Only new or changed files are written, with the guest's mode.
    #[test]
    fn write_back_skips_unchanged_files() {
        let temp = tempfile::tempdir().unwrap();
        let same = temp.path().join("same");
        fs::write(&same, b"x").unwrap();
        let new = temp.path().join("sub/new");
        let files = [
            ImageFile {
                path: same.clone(),
                data: b"x".to_vec(),
                mode: 0o644,
            },
            ImageFile {
                path: new.clone(),
                data: b"y".to_vec(),
                mode: 0o755,
            },
        ];
        assert_eq!(write_back(&files).unwrap(), vec![new.clone()]);
        assert_eq!(fs::read(&new).unwrap(), b"y");
        assert_eq!(
            fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
