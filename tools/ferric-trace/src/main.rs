//! `ferric-trace`: saves kernel trace captures on the target (the `cpu<n>` files drained from
//! `/scheme/kernel.trace`), and prints them as text or exports them for Perfetto.

use std::error::Error;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ferric_trace_cli::capture::Capture;
use ferric_trace_cli::{drain, perfetto, text};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// On the target, as root: drain every CPU's trace ring into a capture directory
    Capture {
        /// The directory to write `stats` and the `cpu<n>` files to
        out: PathBuf,
        /// The trace scheme
        #[arg(long, default_value = "/scheme/kernel.trace")]
        scheme: PathBuf,
    },
    /// Print every record, one line each, in timestamp order
    Text {
        /// Capture files (`cpu<n>`, `cpu<n>.bin`) or directories holding them
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
    /// Write Chrome trace event JSON, which ui.perfetto.dev and chrome://tracing open
    Perfetto {
        /// Capture files (`cpu<n>`, `cpu<n>.bin`) or directories holding them
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Where to write the JSON (default: standard output)
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// The TSC frequency of the traced machine in MHz, to turn cycles into time (at least
        /// 1 MHz)
        #[arg(long, default_value = "1000")]
        tsc_mhz: perfetto::TscMhz,
    },
}

/// Why the command failed.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Capture(#[from] ferric_trace_cli::capture::CaptureError),
    #[error("capture failed")]
    Drain(#[from] ferric_trace_cli::drain::DrainError),
    #[error("cannot write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Cmd::Capture { out, scheme } => {
            let drained = drain::drain_scheme(&scheme, &out)?;
            let lost: u64 = drained.iter().map(|c| c.lost).sum();
            for cpu in &drained {
                println!(
                    "cpu {}: {} bytes, {} records lost",
                    cpu.cpu, cpu.bytes, cpu.lost
                );
            }
            println!("saved {} CPUs to {}", drained.len(), out.display());
            if lost > 0 {
                // Losses are counted since boot, so they may predate this capture.
                eprintln!(
                    "ferric-trace: warning: the rings have dropped {lost} records; compare with an earlier stats read"
                );
            }
            Ok(())
        }
        Cmd::Text { inputs } => {
            let capture = Capture::load(&inputs)?;
            let stdout = io::stdout();
            let mut out = BufWriter::new(stdout.lock());
            text::write(&capture, &mut out)
                .and_then(|()| out.flush())
                .map_err(|source| CliError::Write {
                    path: PathBuf::from("<stdout>"),
                    source,
                })
        }
        Cmd::Perfetto {
            inputs,
            output,
            tsc_mhz,
        } => {
            let capture = Capture::load(&inputs)?;
            let json = perfetto::to_json(&capture, tsc_mhz);
            match output {
                Some(path) => {
                    fs::write(&path, json).map_err(|source| CliError::Write { path, source })
                }
                None => io::stdout()
                    .write_all(json.as_bytes())
                    .map_err(|source| CliError::Write {
                        path: PathBuf::from("<stdout>"),
                        source,
                    }),
            }
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprint!("ferric-trace: {err}");
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
