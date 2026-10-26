//! `ext4d`: serves an ext4 volume, read-only, as a scheme.
//!
//! ```text
//! ext4d [--offset BYTES] [--length BYTES] DEVICE SCHEME
//! ext4d --features
//! ```
//!
//! `DEVICE` is a disk, a partition or an image file. `--offset` and `--length` pick the
//! volume's bytes out of it, for a partition on a whole disk. The volume appears at
//! `/scheme/SCHEME` once it is mounted, and the daemon serves it in the foreground until
//! the scheme is closed. Only Redox has schemes; elsewhere the program says so and exits.
//!
//! Built with `journal-replay` (features.toml `fs.ext4_journal_replay`), it first replays the
//! journal of a volume that was not cleanly unmounted, opening the device for writing only
//! for that. Without it the device is only ever opened for reading. `write` (features.toml
//! `fs.ext4_write`) opens the device for writing and serves the volume read-write when the
//! library can write to it (`ext4d::write::check_writable`), so far for `futimens` only;
//! when it cannot, or the device does not open for writing, it says why and serves the
//! volume read-only (`ext4d::fs::Fs::mount_preferring_write`). `ext4d --features`
//! prints the optional features it was built with, one per line, so tests can tell.

#![forbid(unsafe_code)]

use std::process::ExitCode;

/// The optional Cargo features this build has, as `--features` prints them.
const FEATURES: &[&str] = &[
    #[cfg(feature = "journal-replay")]
    "journal-replay",
    #[cfg(feature = "write")]
    "write",
];

fn main() -> ExitCode {
    if std::env::args().skip(1).eq(["--features"]) {
        for feature in FEATURES {
            println!("{feature}");
        }
        return ExitCode::SUCCESS;
    }
    #[cfg(target_os = "redox")]
    {
        daemon::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        eprintln!("ext4d: schemes exist only on Redox; this build only runs its tests");
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "redox")]
mod daemon {
    use std::fs::File;
    use std::process::ExitCode;

    use ext4d::device::{Window, WindowError};
    #[cfg(feature = "journal-replay")]
    use ext4d::journal::{self, JournalError, Outcome};
    use ext4d::scheme::{Mount, ServeError};
    use thiserror::Error;

    /// The command line.
    struct Args {
        device: String,
        scheme: String,
        offset: u64,
        length: Option<u64>,
    }

    #[derive(Debug, Error)]
    enum DaemonError {
        #[error("usage: ext4d [--offset BYTES] [--length BYTES] DEVICE SCHEME")]
        Usage,
        #[error("{flag} needs a number of bytes, not {value:?}")]
        Number {
            flag: &'static str,
            value: String,
            #[source]
            source: std::num::ParseIntError,
        },
        #[error("cannot open the device {path}")]
        Open {
            path: String,
            #[source]
            source: std::io::Error,
        },
        #[error("the volume's bytes do not fit the device")]
        Window(#[from] WindowError),
        #[cfg(feature = "journal-replay")]
        #[error("cannot replay the journal")]
        Journal(#[from] JournalError),
        #[error("cannot mount the ext4 volume")]
        Mount(#[from] ext4d::Error),
        #[error("serving the scheme failed")]
        Serve(#[from] ServeError),
    }

    fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, DaemonError> {
        let (mut offset, mut length, mut rest) = (0, None, Vec::new());
        while let Some(arg) = args.next() {
            match arg.as_str() {
                flag @ ("--offset" | "--length") => {
                    let value = args.next().ok_or(DaemonError::Usage)?;
                    let n = value.parse().map_err(|source| DaemonError::Number {
                        flag: if flag == "--offset" {
                            "--offset"
                        } else {
                            "--length"
                        },
                        value,
                        source,
                    })?;
                    if flag == "--offset" {
                        offset = n;
                    } else {
                        length = Some(n);
                    }
                }
                _ => rest.push(arg),
            }
        }
        let [device, scheme] = <[String; 2]>::try_from(rest).map_err(|_| DaemonError::Usage)?;
        Ok(Args {
            device,
            scheme,
            offset,
            length,
        })
    }

    /// Opens the volume, for writing when `write` is set.
    fn open(args: &Args, write: bool) -> Result<Window<File>, DaemonError> {
        let device = File::options()
            .read(true)
            .write(write)
            .open(&args.device)
            .map_err(|source| DaemonError::Open {
                path: args.device.clone(),
                source,
            })?;
        Ok(Window::range(device, args.offset, args.length)?)
    }

    /// Replays the journal if the volume needs it, with the device open for writing only
    /// while it does.
    #[cfg(feature = "journal-replay")]
    fn replay(args: &Args, window: &Window<File>) -> Result<(), DaemonError> {
        if !journal::needs_replay(window)? {
            return Ok(());
        }
        let mut writable = open(args, true)?;
        if let Outcome::Replayed {
            transactions,
            blocks,
            journal_errno,
        } = journal::replay(&mut writable)?
        {
            eprintln!(
                "ext4d: {}: replayed {transactions} journal transactions ({blocks} blocks)",
                args.device
            );
            if journal_errno != 0 {
                eprintln!(
                    "ext4d: {}: the journal was aborted (error {journal_errno}); check the \
                     volume with e2fsck",
                    args.device
                );
            }
        }
        Ok(())
    }

    /// Loads the volume: without `write`, read-only.
    #[cfg(not(feature = "write"))]
    fn mount(_args: &Args, window: Window<File>) -> Result<ext4d::fs::Fs, DaemonError> {
        Ok(ext4d::fs::Fs::new(window)?)
    }

    /// Loads the volume: with `write`, for changes when the library can write to it and the
    /// device opens for writing; otherwise read-only, saying why.
    #[cfg(feature = "write")]
    fn mount(args: &Args, window: Window<File>) -> Result<ext4d::fs::Fs, DaemonError> {
        let (fs, read_only) = ext4d::fs::Fs::mount_preferring_write(window, || open(args, true))?;
        if let Some(reason) = read_only {
            eprint!("ext4d: {}: serving read-only: {reason}", args.device);
            print_sources(&reason);
        }
        Ok(fs)
    }

    /// Prints `err`'s sources after it, then ends the line.
    fn print_sources(err: &dyn std::error::Error) {
        let mut source = err.source();
        while let Some(cause) = source {
            eprint!(": {cause}");
            source = cause.source();
        }
        eprintln!();
    }

    fn run() -> Result<(), DaemonError> {
        let args = parse(std::env::args().skip(1))?;
        let window = open(&args, false)?;
        #[cfg(feature = "journal-replay")]
        replay(&args, &window)?;
        let fs = mount(&args, window)?;
        if fs.needs_recovery() {
            eprintln!(
                "ext4d: {}: the volume needs journal recovery; serving ext4-view's in-memory \
                 view of the journal (fs.ext4_journal_replay replays it onto the disk)",
                args.device
            );
        }
        let mode = if fs.is_writable() {
            "read-write (times, mode, owner and in-place data only)"
        } else {
            "read-only"
        };
        let mount = Mount::new(fs, &args.scheme)?;
        eprintln!(
            "ext4d: {} mounted {mode} at /scheme/{}",
            args.device, args.scheme
        );
        mount.serve()?;
        Ok(())
    }

    pub fn main() -> ExitCode {
        match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprint!("ext4d: {err}");
                print_sources(&err);
                ExitCode::FAILURE
            }
        }
    }
}
