//! `ferric-trace`: saves kernel trace captures on the target (the `cpu<n>` files drained from
//! `/scheme/kernel.trace`), prints them as text or exports them for Perfetto or OpenTelemetry, and reads the
//! crash dump a kernel panic left for the next boot.

use std::error::Error;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ferric_trace_cli::capture::{Capture, CaptureError};
use ferric_trace_cli::crash::CrashReport;
use ferric_trace_cli::{drain, otlp, perfetto, text};

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
        /// The directory to save the capture in; it must not exist yet
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
        /// Read a capture marked INCOMPLETE (one that failed or was killed) anyway
        #[arg(long)]
        partial: bool,
    },
    /// Print the panic message and the records of a crash dump (`/scheme/kernel.trace/crash`
    /// after a panic, or a copy of it), and optionally save the records as a capture
    Crash {
        /// The dump
        #[arg(default_value = "/scheme/kernel.trace/crash")]
        dump: PathBuf,
        /// Save the records as a capture directory, which must not exist yet, for `text` or
        /// `perfetto`
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Write Chrome trace event JSON, which ui.perfetto.dev and chrome://tracing open
    Perfetto {
        /// Capture files (`cpu<n>`, `cpu<n>.bin`) or directories holding them
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Read a capture marked INCOMPLETE (one that failed or was killed) anyway
        #[arg(long)]
        partial: bool,
        /// Where to write the JSON (default: standard output)
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// The TSC frequency of the traced machine in MHz, to turn cycles into time (at least
        /// 1 MHz). Used when the capture's timestamps are TSC cycles; a capture whose clock is
        /// the KVM clock or the HPET carries its own rate
        #[arg(long, default_value = "1000")]
        tsc_mhz: perfetto::TscMhz,
    },
    /// Write OpenTelemetry traces as OTLP/JSON, for an OTLP/HTTP collector's /v1/traces or the
    /// Collector's otlpjsonfile receiver
    Otlp {
        /// Capture files (`cpu<n>`, `cpu<n>.bin`) or directories holding them
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Read a capture marked INCOMPLETE (one that failed or was killed) anyway
        #[arg(long)]
        partial: bool,
        /// Where to write the JSON (default: standard output)
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// The TSC frequency of the traced machine in MHz, as for `perfetto`
        #[arg(long, default_value = "1000")]
        tsc_mhz: perfetto::TscMhz,
        /// The Unix time of the first record in nanoseconds (default: 0, so times count from
        /// the first record)
        #[arg(long, default_value = "0")]
        start_unix_ns: u64,
    },
}

/// Why the command failed.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Capture(#[from] ferric_trace_cli::capture::CaptureError),
    #[error("capture failed")]
    Drain(#[from] ferric_trace_cli::drain::DrainError),
    #[error(transparent)]
    Crash(#[from] ferric_trace_cli::crash::CrashDumpError),
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
            for cpu in &drained {
                println!(
                    "cpu {}: {} bytes, {} records lost before the capture, {} during it",
                    cpu.cpu, cpu.bytes, cpu.lost_before, cpu.lost_during
                );
            }
            println!("saved {} CPUs to {}", drained.len(), out.display());
            let during: u64 = drained.iter().map(|c| c.lost_during).sum();
            if during > 0 {
                eprintln!(
                    "ferric-trace: warning: {during} records were dropped while the capture ran"
                );
            }
            Ok(())
        }
        Cmd::Crash { dump, out } => crash(&dump, out.as_deref()),
        Cmd::Text { inputs, partial } => {
            let capture = loader(partial)(&inputs)?;
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
            partial,
            output,
            tsc_mhz,
        } => {
            let capture = loader(partial)(&inputs)?;
            let rate = perfetto::TickRate::for_clock(capture.clock(), tsc_mhz);
            write_output(output, &perfetto::to_json(&capture, rate))
        }
        Cmd::Otlp {
            inputs,
            partial,
            output,
            tsc_mhz,
            start_unix_ns,
        } => {
            let capture = loader(partial)(&inputs)?;
            let options = otlp::Options {
                rate: perfetto::TickRate::for_clock(capture.clock(), tsc_mhz),
                start_unix_ns,
            };
            write_output(output, &otlp::to_json(&capture, options))
        }
    }
}

/// Writes `text` to `output`, or to standard output.
fn write_output(output: Option<PathBuf>, text: &str) -> Result<(), CliError> {
    match output {
        Some(path) => fs::write(&path, text).map_err(|source| CliError::Write { path, source }),
        None => io::stdout()
            .write_all(text.as_bytes())
            .map_err(|source| CliError::Write {
                path: PathBuf::from("<stdout>"),
                source,
            }),
    }
}

/// `ferric-trace crash`.
fn crash(dump: &std::path::Path, out: Option<&std::path::Path>) -> Result<(), CliError> {
    let report = CrashReport::load(dump)?;
    let capture = report
        .capture()
        .map_err(ferric_trace_cli::crash::CrashDumpError::from)?;
    let stdout = io::stdout();
    let mut w = BufWriter::new(stdout.lock());
    let write_error = |source| CliError::Write {
        path: PathBuf::from("<stdout>"),
        source,
    };
    match &report.message {
        Some((cpu, message)) => writeln!(w, "panic on CPU {cpu}: {message}"),
        None => writeln!(w, "no panic message"),
    }
    .map_err(write_error)?;
    match report.clock {
        Some(clock) => write!(w, "{clock}"),
        None => writeln!(w, "clock not recorded (TSC cycles)"),
    }
    .map_err(write_error)?;
    for cpu in &report.cpus {
        writeln!(
            w,
            "cpu {}: {} records ({} bytes), {} lost before the panic, {} bytes torn",
            cpu.cpu,
            cpu.record_count(),
            cpu.records.len(),
            cpu.lost,
            cpu.dropped
        )
        .map_err(write_error)?;
    }
    text::write(&capture, &mut w)
        .and_then(|()| w.flush())
        .map_err(write_error)?;
    if let Some(out) = out {
        report.save(out)?;
    }
    Ok(())
}

/// The capture loader the `--partial` flag picks.
fn loader(partial: bool) -> fn(&[PathBuf]) -> Result<Capture, CaptureError> {
    if partial {
        Capture::load_partial
    } else {
        Capture::load
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
