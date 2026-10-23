//! `ext4d`: serves an ext4 volume, read-only, as a scheme.
//!
//! ```text
//! ext4d [--offset BYTES] [--length BYTES] DEVICE SCHEME
//! ```
//!
//! `DEVICE` is a disk, a partition or an image file. `--offset` and `--length` pick the
//! volume's bytes out of it, for a partition on a whole disk. The volume appears at
//! `/scheme/SCHEME` once it is mounted, and the daemon serves it in the foreground until
//! the scheme is closed. Only Redox has schemes; elsewhere the program says so and exits.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
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
    use std::error::Error as _;
    use std::fs::File;
    use std::process::ExitCode;

    use ext4d::device::{Window, WindowError};
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

    fn run() -> Result<(), DaemonError> {
        let args = parse(std::env::args().skip(1))?;
        let device = File::open(&args.device).map_err(|source| DaemonError::Open {
            path: args.device.clone(),
            source,
        })?;
        let window = Window::range(device, args.offset, args.length)?;
        let fs = ext4d::fs::Fs::new(window)?;
        let mount = Mount::new(fs, &args.scheme)?;
        eprintln!(
            "ext4d: {} mounted read-only at /scheme/{}",
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
