//! `ext4d`: serves an ext4 volume as a scheme, with an explicit read-only policy.
//!
//! ```text
//! ext4d [--read-only] [--offset BYTES] [--length BYTES] DEVICE SCHEME
//! ext4d --features
//! ```
//!
//! `DEVICE` is a disk, a partition or an image file. `--offset` and `--length` pick the
//! volume's bytes out of it, for a partition on a whole disk. The volume appears at
//! `/scheme/SCHEME` once it is mounted, and the daemon serves it in the foreground until
//! the scheme is closed. Started with `FERRIC_READY=stdout` (as `ferric-unlock` does), it
//! writes `ready` to stdout once the scheme is registered (`ferric_ready`). Only Redox has
//! schemes; elsewhere the program says so and exits.
//!
//! Built with `journal-replay` (features.toml `fs.ext4_journal_replay`), it first replays the
//! journal of a volume that was not cleanly unmounted, opening the device for writing only
//! for that. Without it the device is only ever opened for reading. `write` (features.toml
//! `fs.ext4_write`) opens the device for writing and serves the volume read-write when the
//! library can write to it (`ext4d::write::check_writable`), for its supported journaled changes;
//! when it cannot, or the device does not open for writing, it says why and serves the
//! volume read-only (`ext4d::fs::Fs::mount_preferring_write`). `ext4d --features`
//! prints the optional features it was built with, one per line, so tests can tell.
//!
//! `--read-only` keeps the device open only for reading, even when `write` and
//! `journal-replay` are built in. A volume needing recovery is read through its
//! committed journal overlay; its journal is not replayed onto the device.

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

#[cfg(any(target_os = "redox", test))]
mod daemon {
    //! CLI/device startup is host-testable; registration and serving require Redox.

    use std::fs::File;
    use std::path::PathBuf;
    #[cfg(target_os = "redox")]
    use std::process::ExitCode;

    use ext4d::device::{Window, WindowError};
    #[cfg(feature = "journal-replay")]
    use ext4d::journal::{self, JournalError, Outcome};
    #[cfg(target_os = "redox")]
    use ext4d::scheme::{Mount, ServeError};
    use thiserror::Error;

    /// Whether this mount follows the compiled capabilities or forbids writes.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum MountPolicy {
        Configured,
        ReadOnly,
    }

    /// The command line.
    struct Args {
        device: PathBuf,
        scheme: String,
        offset: u64,
        length: Option<u64>,
        policy: MountPolicy,
    }

    #[derive(Debug, Error)]
    enum DaemonError {
        #[error("usage: ext4d [--read-only] [--offset BYTES] [--length BYTES] DEVICE SCHEME")]
        Usage,
        #[error("{flag} needs a number of bytes, not {value:?}")]
        Number {
            flag: &'static str,
            value: String,
            #[source]
            source: std::num::ParseIntError,
        },
        #[error("cannot open the device {}", path.display())]
        Open {
            path: PathBuf,
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
        #[cfg(target_os = "redox")]
        #[error("serving the scheme failed")]
        Serve(#[from] ServeError),
        #[cfg(target_os = "redox")]
        #[error("cannot tell the starter the volume is mounted")]
        Notify(#[source] std::io::Error),
    }

    fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, DaemonError> {
        let (mut offset, mut length, mut rest) = (0, None, Vec::new());
        let mut policy = MountPolicy::Configured;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--read-only" => policy = MountPolicy::ReadOnly,
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
            device: device.into(),
            scheme,
            offset,
            length,
            policy,
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
                args.device.display()
            );
            if journal_errno != 0 {
                eprintln!(
                    "ext4d: {}: the journal was aborted (error {journal_errno}); check the \
                     volume with e2fsck",
                    args.device.display()
                );
            }
        }
        Ok(())
    }

    /// Loads the volume: with `write`, for changes when the library can write to it and the
    /// device opens for writing; otherwise read-only, saying why.
    #[cfg(feature = "write")]
    fn mount(args: &Args, window: Window<File>) -> Result<ext4d::fs::Fs, DaemonError> {
        let (fs, read_only) = ext4d::fs::Fs::mount_preferring_write(window, || open(args, true))?;
        if let Some(reason) = read_only {
            eprint!(
                "ext4d: {}: serving read-only: {reason}",
                args.device.display()
            );
            print_sources(&reason);
        }
        Ok(fs)
    }

    /// Prints `err`'s sources after it, then ends the line.
    #[cfg(any(target_os = "redox", feature = "write"))]
    fn print_sources(err: &dyn std::error::Error) {
        let mut source = err.source();
        while let Some(cause) = source {
            eprint!(": {cause}");
            source = cause.source();
        }
        eprintln!();
    }

    /// Applies the mount policy before registering or serving any scheme.
    fn load(args: &Args) -> Result<ext4d::fs::Fs, DaemonError> {
        let window = open(args, false)?;
        if args.policy == MountPolicy::ReadOnly {
            return Ok(ext4d::fs::Fs::new(window)?);
        }
        #[cfg(feature = "journal-replay")]
        replay(args, &window)?;
        #[cfg(feature = "write")]
        {
            mount(args, window)
        }
        #[cfg(not(feature = "write"))]
        {
            Ok(ext4d::fs::Fs::new(window)?)
        }
    }

    #[cfg(target_os = "redox")]
    fn run() -> Result<(), DaemonError> {
        let args = parse(std::env::args().skip(1))?;
        let fs = load(&args)?;
        if fs.needs_recovery() {
            eprintln!(
                "ext4d: {}: the volume needs journal recovery; serving it as the replay would \
                 leave it, with the journal's committed blocks read in memory over the volume \
                 (the disk is not changed; fs.ext4_journal_replay replays it onto the disk)",
                args.device.display()
            );
        }
        let mode = if fs.is_writable() {
            "read-write"
        } else {
            "read-only"
        };
        let mount = Mount::new(fs, &args.scheme)?;
        // Only now is the name ours; a starter waiting for it may rely on that.
        ferric_ready::notify().map_err(DaemonError::Notify)?;
        eprintln!(
            "ext4d: {} mounted {mode} at /scheme/{}",
            args.device.display(),
            args.scheme
        );
        mount.serve()?;
        Ok(())
    }

    #[cfg(target_os = "redox")]
    /// Runs the native scheme daemon, printing the full error chain on failure.
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

    #[cfg(test)]
    mod tests;
}
