//! `ferric-trace`: saves kernel trace captures on the target (the `cpu<n>` files drained from
//! `/scheme/kernel.trace`), prints them as text or exports them for Perfetto, and reads the
//! crash dump a kernel panic left for the next boot, and records profiles from the kernel's
//! sampling profiler.

use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ferric_trace_cli::capture::{Capture, CaptureError};
use ferric_trace_cli::crash::CrashReport;
use ferric_trace_cli::flamegraph::{Folded, Symbolizer};
use ferric_trace_cli::profile::{self, Run};
use ferric_trace_cli::{drain, perfetto, svg, text};

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
    /// On the target, as root, with kernel.profiling: sample every CPU while a command runs
    /// (or for --seconds) and save the samples into a profile directory
    Profile {
        /// The directory to save the profile in; it must not exist yet
        out: PathBuf,
        /// Sample for this many seconds instead of running a command
        #[arg(long, conflicts_with = "command", required_unless_present = "command")]
        seconds: Option<f64>,
        /// The debug scheme
        #[arg(long, default_value = "/scheme/debug")]
        scheme: PathBuf,
        /// The per-process file listing loaded images (skipped if missing)
        #[arg(long, default_value = "/scheme/sys/proc")]
        proc: PathBuf,
        /// The command to profile, after `--`
        #[arg(last = true)]
        command: Vec<OsString>,
    },
    /// Draw a profile as a flame graph SVG, and optionally save its folded stacks
    Flamegraph {
        /// The profile directory `ferric-trace profile` saved
        profile: PathBuf,
        /// The kernel ELF the profile ran (to symbolize kernel frames)
        #[arg(long)]
        kernel: Option<PathBuf>,
        /// The target's root directory on this machine, such as the cookbook sysroot or a
        /// mounted image (to symbolize user frames)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Where to write the SVG (default: standard output)
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Also write the folded stacks here
        #[arg(long)]
        folded: Option<PathBuf>,
        /// The graph's heading
        #[arg(long, default_value = "Ferric Oxide profile")]
        title: String,
        /// Read a profile marked INCOMPLETE anyway
        #[arg(long)]
        partial: bool,
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
}

/// Why the command failed.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Capture(#[from] ferric_trace_cli::capture::CaptureError),
    #[error("capture failed")]
    Drain(#[from] ferric_trace_cli::drain::DrainError),
    #[error("profile failed")]
    Profile(#[from] ferric_trace_cli::profile::ProfileError),
    #[error("cannot symbolize")]
    Flamegraph(#[from] ferric_trace_cli::flamegraph::FlamegraphError),
    #[error("--seconds must be a non-negative number, not {0}")]
    Seconds(f64),
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
        Cmd::Profile {
            out,
            seconds,
            scheme,
            proc,
            command,
        } => record_profile(&scheme, &proc, &out, seconds, command),
        Cmd::Flamegraph {
            profile,
            kernel,
            root,
            output,
            folded,
            title,
            partial,
        } => {
            let profile = profile::Profile::load(&profile, partial)?;
            let mut symbolizer = Symbolizer::new(&profile, kernel.as_deref(), root.as_deref())?;
            let stacks = Folded::new(&profile, &mut symbolizer);
            if let Some(path) = folded {
                fs::write(&path, stacks.text())
                    .map_err(|source| CliError::Write { path, source })?;
            }
            eprintln!(
                "ferric-trace: {} samples, {} distinct stacks, {} unreadable",
                stacks.total(),
                stacks.stacks.len(),
                stacks.bad
            );
            let svg = svg::render(&stacks.stacks, &title);
            match output {
                Some(path) => {
                    fs::write(&path, svg).map_err(|source| CliError::Write { path, source })
                }
                None => io::stdout()
                    .write_all(svg.as_bytes())
                    .map_err(|source| CliError::Write {
                        path: PathBuf::from("<stdout>"),
                        source,
                    }),
            }
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
            let json = perfetto::to_json(&capture, rate);
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

/// `ferric-trace profile`.
fn record_profile(
    scheme: &std::path::Path,
    proc: &std::path::Path,
    out: &std::path::Path,
    seconds: Option<f64>,
    command: Vec<OsString>,
) -> Result<(), CliError> {
    let run = match seconds {
        Some(s) => {
            Run::For(std::time::Duration::try_from_secs_f64(s).map_err(|_| CliError::Seconds(s))?)
        }
        None => Run::Command(command),
    };
    let recorded = profile::record(scheme, proc, out, &run)?;
    for (cpu, bytes) in recorded.cpu_bytes.iter().enumerate() {
        println!("cpu {cpu}: {bytes} bytes of samples");
    }
    println!(
        "named {} contexts, images of {}; saved to {}",
        recorded.names,
        recorded.images,
        out.display()
    );
    let snapshots = recorded.snapshots;
    println!(
        "image snapshots: {} in {:?} (longest {:?}), {} ppm of the {:?} from the first to the last",
        snapshots.count,
        snapshots.total,
        snapshots.longest,
        snapshots.duty_ppm(),
        snapshots.span
    );
    if recorded.images == 0 {
        eprintln!(
            "ferric-trace: warning: no images recorded ({} missing or empty); user frames will \
             not be symbolized",
            proc.display()
        );
    }
    if let Some(status) = recorded.status
        && !status.success()
    {
        eprintln!("ferric-trace: warning: the profiled command exited with {status}");
    }
    Ok(())
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
