//! Clock comparison under controlled syscall load, with bounded capture and loss diagnostics.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ferric_trace::abi;
use ferric_trace::clock::ClockLine;
use ferric_trace::kernel::{SYSCALL_ENTER, SYSCALL_EXIT};
use ferric_trace::record::{HEADER_WORDS, WORD_BYTES};
use syscall::{EBADF, SYS_WRITE};

use super::{
    BAD_FD, CpuMap, Failure, READ_LEN, Why, check as failure, command, commit, expect_errno,
    read_text,
};
use crate::clock_capture::{Counters, Markers};

const STEP: &str = "trace-clock";
/// The first marker's length; the second is one byte longer and load writes are two longer.
const MARK_LEN: usize = 1931;
/// The minimum interval between the two markers, as in the original clock check.
const INTERVAL: Duration = Duration::from_millis(200);
const POLL: Duration = Duration::from_millis(1);
/// Well below ring capacity even with syscall enter/exit and scheduling records.
const BURST: usize = 64;
/// Budget per CPU: total controlled traffic exceeds twice the combined ring capacity.
/// The writing process may migrate; this does not pin or balance production across CPUs.
const NOISE_PER_CPU: usize = (abi::CPU_RING_WORDS * 2)
    .div_ceil(HEADER_WORDS * 2 + SYSCALL_ENTER.args().len() + SYSCALL_EXIT.args().len());
const _: () = assert!(READ_LEN >= abi::CPU_RING_WORDS * WORD_BYTES);

struct Cpu {
    ring: CpuMap,
    before: Counters,
    words: u64,
    peak_pending: u64,
}

/// Owns the open mappings and pairs each CPU with its own diagnostic counters.
struct Capture {
    cpus: Vec<Cpu>,
    buffer: Vec<u8>,
    markers: Markers,
}

fn markers() -> Markers {
    Markers::new(SYS_WRITE as u64, BAD_FD as u64, MARK_LEN as u64)
}

impl Capture {
    fn new(cpus: &[u32]) -> Result<Self, Failure> {
        let cpus = cpus
            .iter()
            .map(|&cpu| {
                let ring = CpuMap::open(STEP, cpu)?;
                let before = Counters::sample(&ring.view(STEP)?);
                Ok(Cpu {
                    ring,
                    before,
                    words: 0,
                    peak_pending: 0,
                })
            })
            .collect::<Result<Vec<_>, Failure>>()?;
        let mut capture = Self {
            cpus,
            buffer: vec![0; READ_LEN],
            markers: markers(),
        };
        capture.poll()?;
        capture.markers = markers();
        for cpu in &mut capture.cpus {
            cpu.before = Counters::sample(&cpu.ring.view(STEP)?);
            cpu.words = 0;
            cpu.peak_pending = 0;
        }
        Ok(capture)
    }

    /// One peek per CPU: a traced commit cannot extend this pass by refilling the ring.
    fn poll(&mut self) -> Result<(), Failure> {
        for cpu in &mut self.cpus {
            let view = cpu.ring.view(STEP)?;
            cpu.peak_pending = cpu
                .peak_pending
                .max(view.head().saturating_sub(view.tail()));
            let peek = self
                .markers
                .collect(&view, &mut self.buffer)
                .map_err(|source| Failure {
                    step: STEP,
                    why: Why::Capture {
                        cpu: cpu.ring.cpu,
                        source,
                    },
                })?;
            cpu.words += (peek.len() / WORD_BYTES) as u64;
            if !peek.is_empty() {
                commit(cpu.ring.fd(), peek.end()).map_err(|source| Failure {
                    step: STEP,
                    why: Why::Call {
                        call: "commit of clock capture",
                        result: Err(source),
                    },
                })?;
            }
        }
        Ok(())
    }

    fn report(&self) -> Result<(), Failure> {
        for cpu in &self.cpus {
            let after = Counters::sample(&cpu.ring.view(STEP)?);
            let before = cpu.before;
            println!(
                "FERRIC_TRACE_CLOCK cpu={} head_before={} head_after={} tail_before={} tail_after={} lost_before={} lost_after={} lost_during={} words={} peak_pending={}",
                cpu.ring.cpu,
                before.head,
                after.head,
                before.tail,
                after.tail,
                before.lost,
                after.lost,
                after.lost.wrapping_sub(before.lost),
                cpu.words,
                cpu.peak_pending
            );
        }
        Ok(())
    }
}

fn write(marker: &[u8]) -> Result<(), Failure> {
    expect_errno(
        STEP,
        "write to a closed fd",
        syscall::write(BAD_FD, marker),
        EBADF,
    )
}

/// Issues the markers once, consuming between bounded bursts throughout their interval.
fn measure(capture: &mut Capture, marker: &[u8]) -> Result<(Duration, usize), Failure> {
    capture.poll()?;
    write(&marker[..MARK_LEN])?;
    let start = Instant::now();
    let expected = capture
        .cpus
        .len()
        .checked_mul(NOISE_PER_CPU)
        .ok_or_else(|| failure(STEP, "clock background write count overflows"))?;
    let mut issued = 0;
    while issued < expected || start.elapsed() < INTERVAL {
        let burst = BURST.min(expected - issued);
        for _ in 0..burst {
            write(marker)?;
        }
        issued += burst;
        capture.poll()?;
        let left = INTERVAL.saturating_sub(start.elapsed());
        if !left.is_zero() {
            std::thread::sleep(left.min(POLL));
        }
    }
    let wall = start.elapsed();
    write(&marker[..MARK_LEN + 1])?;
    Ok((wall, issued))
}

fn detail(line: &ClockLine, before: u64, after: u64, wall: Duration) -> Result<String, Failure> {
    let Some(ticks) = after.checked_sub(before).filter(|&ticks| ticks > 0) else {
        return Err(failure(
            STEP,
            format!("the second marker's timestamp {after} is not after the first's {before}"),
        ));
    };
    let wall_us = wall.as_secs_f64() * 1e6;
    match line.ticks_per_us() {
        Some(rate) => {
            let traced_us = ticks as f64 / rate;
            if !(0.5..=2.0).contains(&(traced_us / wall_us)) {
                return Err(failure(
                    STEP,
                    format!(
                        "{}: {traced_us:.0} us between the markers' timestamps, {wall_us:.0} us of wall time",
                        line.clock
                    ),
                ));
            }
            Ok(format!(
                "{}, {} fs ticks: {traced_us:.0} us traced over {wall_us:.0} us",
                line.clock,
                line.period_fs.unwrap_or(0)
            ))
        }
        None => Ok(format!(
            "{}: {ticks} ticks over {wall_us:.0} us, about {:.0} MHz",
            line.clock,
            ticks as f64 / wall_us
        )),
    }
}

/// Checks ordering and elapsed-time agreement, retaining both markers under syscall load.
///
/// # Errors
///
/// Fails on malformed records, I/O errors, missing/duplicate writes, backward clocks or
/// a calibrated clock's elapsed-time ratio outside the existing 0.5–2.0 interval.
pub(super) fn check(cpus: &[u32]) -> Result<(), Failure> {
    let text = read_text(STEP, abi::CLOCK_PATH)?;
    let line = ClockLine::parse(&text).map_err(|source| Failure {
        step: STEP,
        why: Why::ClockLine {
            line: text.clone(),
            source,
        },
    })?;
    let mut capture = Capture::new(cpus)?;
    let marker = vec![0x5a; MARK_LEN + 2];
    command(STEP, "enable syscall")?;
    let measured = measure(&mut capture, &marker);
    // Always restore the event state and collect diagnostics after a failed call/read.
    let disabled = command(STEP, "disable syscall");
    let last = capture.poll();
    let diagnostics = capture.report();
    let (wall, issued) = measured?;
    disabled?;
    last?;
    diagnostics?;
    let (before, after) = capture
        .markers
        .timestamps(issued as u64)
        .map_err(|source| Failure {
            step: STEP,
            why: Why::ClockCounts(source),
        })?;
    let detail = detail(&line, before, after, wall)?;
    println!("FERRIC_TEST_PASS trace-clock ({detail}; background_writes={issued})");
    Ok(())
}
