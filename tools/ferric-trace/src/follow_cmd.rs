//! `ferric-trace follow`: the command line of [`ferric_trace_cli::follow`] (Linux hosts).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clap::Args;
use ferric_guest_ram::{GuestRam, GuestRamError, RamLayout};
use ferric_trace_cli::follow::{self, FollowError, Options};

/// The arguments of `ferric-trace follow`.
#[derive(Args)]
pub struct FollowArgs {
    /// The guest's RAM file: QEMU's memory-backend-file with share=on
    #[arg(long)]
    guest_ram: PathBuf,
    /// The capture directory to write; it must not exist yet
    #[arg(long)]
    out: PathBuf,
    /// The ring directory's physical address, in hex, from the kernel's line
    /// `trace: host rings at phys 0x...` (default: scan the RAM for it)
    #[arg(long, value_parser = parse_hex)]
    phys: Option<u64>,
    /// The guest's RAM in MiB, for q35's layout of RAM below and above 4 GiB (default: the
    /// file's size)
    #[arg(long)]
    ram_mib: Option<u64>,
    /// Milliseconds between polls of the rings
    #[arg(long, default_value_t = 10)]
    interval_ms: u64,
    /// Print a line a second with each event's records so far and rate, and the records lost
    #[arg(long)]
    live: bool,
    /// Stop after following for this many seconds (default: until SIGINT or SIGTERM, the
    /// RAM file's removal or the guest's next boot)
    #[arg(long)]
    duration: Option<f64>,
}

/// Why `ferric-trace follow` failed.
#[derive(Debug, thiserror::Error)]
pub enum FollowCmdError {
    #[error("cannot map the guest RAM")]
    Ram(#[from] GuestRamError),
    #[error(transparent)]
    Follow(#[from] FollowError),
    #[error("--duration must be a non-negative number of seconds, not {0}")]
    Duration(f64),
    #[error("--ram-mib {0} is too large")]
    RamMib(u64),
    #[error("cannot catch SIGINT and SIGTERM")]
    Signals(#[source] io::Error),
}

/// Parses an address in hex, with or without `0x`.
fn parse_hex(text: &str) -> Result<u64, std::num::ParseIntError> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u64::from_str_radix(digits, 16)
}

/// Runs `ferric-trace follow`.
pub fn run(args: FollowArgs) -> Result<(), FollowCmdError> {
    let duration = args
        .duration
        .map(|seconds| {
            Duration::try_from_secs_f64(seconds).map_err(|_| FollowCmdError::Duration(seconds))
        })
        .transpose()?;
    let layout = args
        .ram_mib
        .map(|mib| {
            mib.checked_mul(1 << 20)
                .map(RamLayout::q35)
                .ok_or(FollowCmdError::RamMib(mib))
        })
        .transpose()?;
    let ram = GuestRam::open(&args.guest_ram, layout)?;
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop)).map_err(FollowCmdError::Signals)?;
    }
    let options = Options {
        phys: args.phys,
        interval: Duration::from_millis(args.interval_ms),
        duration,
        live: args.live,
    };
    let summary = follow::run(&ram, &args.out, &options, &stop, &mut io::stdout())?;
    for cpu in &summary.cpus {
        println!(
            "cpu {}: {} records ({} bytes), {} lost before following, about {} lost while \
             following ({} words in {} skips)",
            cpu.cpu,
            cpu.records,
            cpu.bytes,
            cpu.lost_before,
            cpu.lost_records,
            cpu.lost_words,
            cpu.skips
        );
    }
    println!(
        "followed the rings at {:#x} into {} until {}",
        summary.phys,
        args.out.display(),
        summary.stop.describe()
    );
    let lost: u64 = summary.cpus.iter().map(|cpu| cpu.lost_records).sum();
    if lost > 0 {
        eprintln!(
            "ferric-trace: warning: about {lost} records were overwritten before they were \
             read; try larger rings (FERRIC_TRACE_HOST) or a shorter --interval-ms"
        );
    }
    Ok(())
}
