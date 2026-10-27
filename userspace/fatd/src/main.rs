//! `fatd`: serves a FAT12, FAT16, FAT32 or (with the `exfat` feature) exFAT volume,
//! read-only, as a scheme.
//!
//! ```text
//! fatd [--offset BYTES] [--length BYTES] DEVICE SCHEME
//! ```
//!
//! `DEVICE` is a disk, a partition or an image file. `--offset` and `--length` pick the
//! volume's bytes out of it, for a partition on a whole disk. The volume appears at
//! `/scheme/SCHEME` once it is mounted, and the daemon serves it in the foreground until
//! the scheme is closed. The device is only ever opened for reading. Only Redox has
//! schemes; elsewhere the program says so and exits.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "redox")]
    {
        daemon::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        eprintln!("fatd: schemes exist only on Redox; this build only runs its tests");
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "redox")]
mod daemon {
    use std::error::Error as _;
    use std::process::ExitCode;

    use fatd::fs::Fs;
    use fatd::scheme::{Mount, ServeError};
    use ferric_blocklayer::file::FileDevice;
    use ferric_blocklayer::{BlockDevice, OutOfRange, Slice};
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
        #[error("usage: fatd [--offset BYTES] [--length BYTES] DEVICE SCHEME")]
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
        Window(#[from] OutOfRange),
        #[error("cannot mount the volume")]
        Mount(#[from] fatd::Error),
        #[error("serving the scheme failed")]
        Serve(#[from] ServeError),
    }

    fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, DaemonError> {
        let (mut offset, mut length, mut rest) = (0, None, Vec::new());
        while let Some(arg) = args.next() {
            let flag = match arg.as_str() {
                "--offset" => "--offset",
                "--length" => "--length",
                _ => {
                    rest.push(arg);
                    continue;
                }
            };
            let value = args.next().ok_or(DaemonError::Usage)?;
            let n = value.parse().map_err(|source| DaemonError::Number {
                flag,
                value,
                source,
            })?;
            if flag == "--offset" {
                offset = n;
            } else {
                length = Some(n);
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

    /// Opens the volume's bytes of the device, for reading only.
    fn open(args: &Args) -> Result<Slice<FileDevice>, DaemonError> {
        let device =
            FileDevice::open(args.device.as_ref()).map_err(|source| DaemonError::Open {
                path: args.device.clone(),
                source,
            })?;
        let window = match args.length {
            Some(length) => Slice::new(device, args.offset, length)?,
            None => Slice::from_offset(device, args.offset)?,
        };
        Ok(window)
    }

    fn run() -> Result<(), DaemonError> {
        let args = parse(std::env::args().skip(1))?;
        let window = open(&args)?;
        let size = window.size();
        let volume = fatd::fs::mount(window)?;
        let fs = Fs::new(&volume);
        let (format, label) = (fs.format().name(), fs.label());
        let mount = Mount::new(fs, &args.scheme)?;
        eprintln!(
            "fatd: {} ({size} bytes, {format}, label {label:?}) mounted read-only at /scheme/{}",
            args.device, args.scheme
        );
        mount.serve()?;
        Ok(())
    }

    pub fn main() -> ExitCode {
        match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprint!("fatd: {err}");
                let mut source = err.source();
                while let Some(cause) = source {
                    eprint!(": {cause}");
                    source = cause.source();
                }
                eprintln!();
                ExitCode::FAILURE
            }
        }
    }
}
