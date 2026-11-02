//! Writes of one 70-byte line: to the system log against the null sink.
//!
//! Services get `/scheme/log` as their standard output and error, so every line a program
//! logs is a request to `logd`. `logd` writes the line on to the kernel's debug scheme, which
//! sends it to the serial port a byte at a time, before it answers. `/scheme/null` is the same
//! kind of request to a user-space scheme daemon (`zerod`) that drops the bytes, so the
//! difference between `write-log` and `write-null` is what the log path costs per line.
//!
//! `write-log` therefore puts its lines on the serial console. They say what they are, and
//! there are at most a few hundred of them.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use super::{BenchError, Config, Measurement, Op, file_error};
use crate::measure::{Monotonic, Plan, measure};

/// The line each write sends: 70 bytes, one line, about the length of a debug log line.
const LINE: &[u8; 70] = b"ferric-perf write-log: one 70-byte line per sample; safe to ignore it\n";

/// The plan for the log: each line reaches the serial console.
const LOG_PLAN: Plan = Plan {
    warmup: 5,
    iters: 200,
    min_iters: 20,
    budget: Duration::from_secs(2),
};

/// The plan for the null sink.
const NULL_PLAN: Plan = Plan {
    warmup: 100,
    iters: 10_000,
    min_iters: 100,
    budget: Duration::from_secs(1),
};

/// `write-log`.
pub fn log(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    write(config, "write-log", &config.log, LOG_PLAN)
}

/// `write-null`.
pub fn null(config: &Config) -> Result<Vec<Measurement>, BenchError> {
    write(config, "write-null", &config.null, NULL_PLAN)
}

/// Writes [`LINE`] to `path` as the plan says, one write per sample.
fn write(
    config: &Config,
    name: &'static str,
    path: &Path,
    plan: Plan,
) -> Result<Vec<Measurement>, BenchError> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(file_error(Op::Open, path))?;
    let [latency] = measure(&config.plan(plan), &mut Monotonic::new(), |laps| {
        file.write_all(LINE).map_err(file_error(Op::Write, path))?;
        laps.lap();
        Ok(())
    })?
    .phases;
    Ok(vec![Measurement {
        name,
        latency,
        bytes: Some(LINE.len() as u64),
        extra: Vec::new(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_is_one_line() {
        assert_eq!(LINE.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(LINE.last(), Some(&b'\n'));
    }
}
