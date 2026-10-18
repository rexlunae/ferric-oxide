//! `ferric-trace`: reads kernel trace captures (the `cpu<n>` files drained from
//! `/scheme/kernel.trace`) and prints them as text or exports them for Perfetto.

use std::error::Error;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ferric_trace_cli::capture::Capture;
use ferric_trace_cli::{perfetto, text};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
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
        /// The TSC frequency of the traced machine in MHz, to turn cycles into time (finite
        /// and greater than 0)
        #[arg(long, default_value = "1000")]
        tsc_mhz: perfetto::TscMhz,
    },
}

/// Why the command failed.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Capture(#[from] ferric_trace_cli::capture::CaptureError),
    #[error("cannot write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
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
