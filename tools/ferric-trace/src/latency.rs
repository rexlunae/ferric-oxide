//! The latency view: how long system calls, scheme requests, wake-ups and interrupts took in a
//! capture, as histograms.
//!
//! [`Latencies::new`] measures four kinds of interval, in nanoseconds of the capture's clock:
//!
//! - **`syscall`**, by system call number: from `syscall.enter` to the matching `syscall.exit`
//!   on the same context ([`Spans`]), so a call that blocked includes the time it was switched
//!   out and resumed on another CPU.
//! - **`scheme`**, by scheme id and opcode: from `scheme.request` to the `scheme.response` with
//!   the same scheme and tag ([`Spans`]): queueing for the scheme's daemon, its work, and the
//!   kernel accepting the answer.
//! - **`wake_run`**, by the CPU it ran on: `sched.wake_run`'s `wait`, from a context being made
//!   runnable to it running.
//! - **`irq`**, by IRQ: from `irq.deliver` to the `irq.ack` of the same IRQ, CPU and count
//!   ([`Interrupt::pair`]): how long a driver took to handle an interrupt.
//!
//! [`Latencies::write`] prints one line per key and a merged `all` line per kind, with
//! integer-only `key=value` fields like the `counts` view's, from `ferric_iobench`'s
//! fio-layout [`Histogram`]: the minimum, mean and maximum are exact, and a percentile is the
//! middle of the bucket its rank falls in, within 1/64 of the true value (clamped to the
//! minimum and maximum). Some lines of the test fixture capture
//! (`tools/ferric-trace/tests/fixtures/latency`):
//!
//! ```text
//! latency syscall nr=0x9e name=yield count=2 open=0 min_ns=50 mean_ns=75 p50_ns=50 p90_ns=100 p99_ns=100 max_ns=100
//! latency syscall nr=0xf0 name=futex count=0 open=1 min_ns=0 mean_ns=0 p50_ns=0 p90_ns=0 p99_ns=0 max_ns=0
//! latency syscall nr=0x21000004 name=write count=1 open=0 min_ns=3000 mean_ns=3000 p50_ns=3000 p90_ns=3000 p99_ns=3000 max_ns=3000
//! latency syscall nr=all count=3 open=1 unmatched=1 min_ns=50 mean_ns=1050 p50_ns=100 p90_ns=2992 p99_ns=2992 max_ns=3000
//! latency scheme scheme=2 opcode=Write count=1 open=0 min_ns=1900 mean_ns=1900 p50_ns=1900 p90_ns=1900 p99_ns=1900 max_ns=1900
//! latency scheme scheme=all opcode=all count=1 open=0 unmatched=0 min_ns=1900 mean_ns=1900 p50_ns=1900 p90_ns=1900 p99_ns=1900 max_ns=1900
//! latency wake_run cpu=0 count=1 min_ns=200 mean_ns=200 p50_ns=200 p90_ns=200 p99_ns=200 max_ns=200
//! latency wake_run cpu=all count=2 min_ns=50 mean_ns=125 p50_ns=50 p90_ns=200 p99_ns=200 max_ns=200
//! latency irq irq=11 count=1 open=0 min_ns=1100 mean_ns=1100 p50_ns=1100 p90_ns=1100 p99_ns=1100 max_ns=1100
//! latency irq irq=all count=1 open=1 unmatched=0 min_ns=1100 mean_ns=1100 p50_ns=1100 p90_ns=1100 p99_ns=1100 max_ns=1100
//! ```
//!
//! `name` is the call's name where `redox_syscall` defines the number; `opcode` is the
//! `schemev2::Opcode` name, or the number for one it does not know. `open` counts intervals
//! that started in the capture and had not ended by its last record (a call still blocked, an
//! unanswered request), and the `all` line's `unmatched` counts ends whose start the capture
//! does not have. Neither is in the histogram. Both grow when the rings dropped records, so a
//! capture with `lost` records (see the `counts` view) reads as a sample, not a census.
//!
//! Attributing a call to a context needs `sched.switch` in the capture. Without it, every call
//! on a CPU is one context's, and a call that blocks while another context makes a call of a
//! different number on the same CPU is left open.

use std::collections::BTreeMap;
use std::io::{self, Write};

use ferric_iobench::latency::Histogram;
use ferric_trace::kernel::{IRQ_ACK, SCHED_WAKE_RUN, SCHEME_RESPONSE, SYSCALL_EXIT};

use crate::capture::Capture;
use crate::counts::ticks_to_ns;
use crate::describe;
use crate::perfetto::TickRate;
use crate::spans::{Interrupt, SpanKind, Spans};

/// The intervals of one key: their durations, and how many had not ended.
#[derive(Clone, Debug, Default)]
struct Series {
    latency: Histogram,
    open: u64,
}

impl Series {
    /// Records an interval of `ns` nanoseconds, or one that had not ended (`None`).
    fn record(&mut self, ns: Option<u64>) {
        match ns {
            Some(ns) => self.latency.record(ns),
            None => self.open += 1,
        }
    }
}

/// One kind of interval, by key.
#[derive(Clone, Debug)]
struct Kind<K> {
    by_key: BTreeMap<K, Series>,
    /// Ends whose start is not in the capture.
    unmatched: u64,
}

impl<K: Ord> Kind<K> {
    fn new() -> Self {
        Self {
            by_key: BTreeMap::new(),
            unmatched: 0,
        }
    }

    fn record(&mut self, key: K, ns: Option<u64>) {
        self.by_key.entry(key).or_default().record(ns);
    }

    /// Intervals that ended, over every key.
    fn ended(&self) -> u64 {
        self.by_key
            .values()
            .map(|series| series.latency.count())
            .sum()
    }

    /// Sets [`Kind::unmatched`] from the number of end records in the capture.
    fn set_ends(&mut self, ends: u64) {
        self.unmatched = ends.saturating_sub(self.ended());
    }

    fn is_empty(&self) -> bool {
        self.by_key.is_empty() && self.unmatched == 0
    }

    /// Every key's intervals, merged.
    fn merged(&self) -> Series {
        let mut merged = Series::default();
        for series in self.by_key.values() {
            merged.latency.merge(&series.latency);
            merged.open += series.open;
        }
        merged
    }

    /// Writes one line per key, `label(key)` naming it, then the merged line with `all_label`.
    fn write(
        &self,
        out: &mut impl Write,
        kind: &str,
        all_label: &str,
        label: impl Fn(&K) -> String,
    ) -> io::Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        for (key, series) in &self.by_key {
            writeln!(
                out,
                "latency {kind} {} count={} open={} {}",
                label(key),
                series.latency.count(),
                series.open,
                fields(&series.latency)
            )?;
        }
        let all = self.merged();
        writeln!(
            out,
            "latency {kind} {all_label} count={} open={} unmatched={} {}",
            all.latency.count(),
            all.open,
            self.unmatched,
            fields(&all.latency)
        )
    }
}

/// The distribution fields of `latency`.
fn fields(latency: &Histogram) -> String {
    format!(
        "min_ns={} mean_ns={} p50_ns={} p90_ns={} p99_ns={} max_ns={}",
        latency.min(),
        latency.mean(),
        latency.percentile(500_000),
        latency.percentile(900_000),
        latency.percentile(990_000),
        latency.max(),
    )
}

/// The latency distributions of a capture.
#[derive(Clone, Debug)]
pub struct Latencies {
    /// System calls by number.
    syscalls: Kind<u64>,
    /// Scheme requests by scheme id and raw opcode.
    requests: Kind<(u64, u64)>,
    /// `sched.wake_run` waits by CPU.
    wake_run: BTreeMap<u32, Histogram>,
    /// Interrupts by IRQ.
    irqs: Kind<u64>,
}

impl Latencies {
    /// Measures the intervals of `capture`, whose timestamps tick at `rate` (see
    /// [`TickRate::for_clock`]).
    pub fn new(capture: &Capture, rate: TickRate) -> Self {
        let entries = capture.entries();
        let duration = |start: usize, end: Option<usize>| {
            end.map(|end| {
                let ticks = entries[end]
                    .timestamp
                    .saturating_sub(entries[start].timestamp);
                ticks_to_ns(ticks, rate)
            })
        };
        let mut syscalls = Kind::new();
        let mut requests = Kind::new();
        for span in Spans::build(capture).spans {
            match span.kind {
                SpanKind::Syscall { nr } => syscalls.record(nr, duration(span.start, span.end)),
                SpanKind::Request { scheme, opcode, .. } => {
                    requests.record((scheme, opcode), duration(span.start, span.end));
                }
                _ => {}
            }
        }
        let mut irqs = Kind::new();
        for interrupt in Interrupt::pair(capture) {
            irqs.record(interrupt.irq, duration(interrupt.deliver, interrupt.ack));
        }
        let mut wake_run: BTreeMap<u32, Histogram> = BTreeMap::new();
        let (mut exits, mut responses, mut acks) = (0, 0, 0);
        for entry in entries {
            let id = entry.event.id();
            if id == SCHED_WAKE_RUN.id() {
                wake_run
                    .entry(entry.cpu)
                    .or_default()
                    .record(ticks_to_ns(entry.arg(2), rate));
            } else if id == SYSCALL_EXIT.id() {
                exits += 1;
            } else if id == SCHEME_RESPONSE.id() {
                responses += 1;
            } else if id == IRQ_ACK.id() {
                acks += 1;
            }
        }
        syscalls.set_ends(exits);
        requests.set_ends(responses);
        irqs.set_ends(acks);
        Self {
            syscalls,
            requests,
            wake_run,
            irqs,
        }
    }

    /// Writes the view to `out`, as the module documents: system calls, scheme requests,
    /// wake-ups and interrupts, each kind only when the capture has any.
    ///
    /// # Errors
    ///
    /// The error of a failed write.
    pub fn write(&self, out: &mut impl Write) -> io::Result<()> {
        self.syscalls.write(
            out,
            "syscall",
            "nr=all",
            |&nr| match describe::syscall_name(nr) {
                Some(name) => format!("nr={nr:#x} name={name}"),
                None => format!("nr={nr:#x}"),
            },
        )?;
        self.requests.write(
            out,
            "scheme",
            "scheme=all opcode=all",
            |&(scheme, opcode)| {
                let opcode = describe::opcode(opcode)
                    .map_or_else(|| opcode.to_string(), |op| format!("{op:?}"));
                format!("scheme={scheme} opcode={opcode}")
            },
        )?;
        if !self.wake_run.is_empty() {
            let mut all = Histogram::new();
            for (cpu, waits) in &self.wake_run {
                writeln!(
                    out,
                    "latency wake_run cpu={cpu} count={} {}",
                    waits.count(),
                    fields(waits)
                )?;
                all.merge(waits);
            }
            writeln!(
                out,
                "latency wake_run cpu=all count={} {}",
                all.count(),
                fields(&all)
            )?;
        }
        self.irqs
            .write(out, "irq", "irq=all", |&irq| format!("irq={irq}"))
    }
}

#[cfg(test)]
mod tests {
    use ferric_trace::clock::ClockLine;
    use ferric_trace::kernel::{
        IRQ_DELIVER, SCHED_SWITCH, SCHEME_REQUEST, SYSCALL_ENTER, SYSCALL_EXIT,
    };

    use super::*;
    use crate::capture::CpuCapture;
    use crate::capture::tests::bytes;
    use crate::perfetto::TscMhz;

    fn view(cpus: &[Vec<u8>], clock: Option<&str>) -> String {
        let capture = Capture::from_cpus(
            cpus.iter()
                .enumerate()
                .map(|(cpu, b)| {
                    CpuCapture::decode(u32::try_from(cpu).expect("few CPUs"), b).expect("valid")
                })
                .collect(),
        )
        .with_clock(clock.map(|line| ClockLine::parse(line).expect("clock line")));
        let rate = TickRate::for_clock(capture.clock(), TscMhz::new(1000.0).expect("mhz"));
        let mut out = Vec::new();
        Latencies::new(&capture, rate)
            .write(&mut out)
            .expect("writes");
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn an_empty_capture_prints_nothing() {
        assert_eq!(view(&[Vec::new()], None), "");
        // Records of other kinds only.
        let cpu0 = bytes(&[(&SCHED_SWITCH, 1, &[0, 3, 1, 7])]);
        assert_eq!(view(&[cpu0], None), "");
    }

    #[test]
    fn system_calls_by_number_with_open_and_unmatched_ones() {
        let cpu0 = bytes(&[
            (&SCHED_SWITCH, 0, &[0, 3, 1, 7]),
            (&SYSCALL_ENTER, 100, &[0x2100_0004, 1, 0, 8, 0]),
            (&SYSCALL_EXIT, 400, &[0x2100_0004, 8]),
            (&SYSCALL_ENTER, 500, &[0x2100_0004, 1, 0, 8, 0]),
            (&SYSCALL_EXIT, 600, &[0x2100_0004, 8]),
            // An exit whose enter was before the capture, then a call still running at its end.
            (&SYSCALL_EXIT, 700, &[0xdead, 0]),
            (&SYSCALL_ENTER, 800, &[0xdead, 0, 0, 0, 0]),
        ]);
        // TSC cycles at the default 1000 MHz: one cycle is a nanosecond.
        assert_eq!(
            view(&[cpu0], None),
            "latency syscall nr=0xdead count=0 open=1 min_ns=0 mean_ns=0 p50_ns=0 p90_ns=0 \
             p99_ns=0 max_ns=0\n\
             latency syscall nr=0x21000004 name=write count=2 open=0 min_ns=100 mean_ns=200 \
             p50_ns=100 p90_ns=300 p99_ns=300 max_ns=300\n\
             latency syscall nr=all count=2 open=1 unmatched=1 min_ns=100 mean_ns=200 \
             p50_ns=100 p90_ns=300 p99_ns=300 max_ns=300\n"
        );
    }

    #[test]
    fn requests_by_scheme_and_opcode_in_the_capture_clock() {
        let cpu0 = bytes(&[
            (&SCHEME_REQUEST, 10, &[2, 1, 6, 3, 1, 70]),
            (&SCHEME_REQUEST, 12, &[2, 2, 200, 3, 1, 70]),
        ]);
        let cpu1 = bytes(&[
            (&SCHEME_RESPONSE, 30, &[2, 1, 70, 5]),
            (&SCHEME_RESPONSE, 40, &[9, 1, 0, 5]),
        ]);
        // An HPET whose tick is 10 ns: the write took 20 ticks.
        assert_eq!(
            view(&[cpu0, cpu1], Some("clock hpet period_fs 10000000\n")),
            "latency scheme scheme=2 opcode=Write count=1 open=0 min_ns=200 mean_ns=200 \
             p50_ns=200 p90_ns=200 p99_ns=200 max_ns=200\n\
             latency scheme scheme=2 opcode=200 count=0 open=1 min_ns=0 mean_ns=0 p50_ns=0 \
             p90_ns=0 p99_ns=0 max_ns=0\n\
             latency scheme scheme=all opcode=all count=1 open=1 unmatched=1 min_ns=200 \
             mean_ns=200 p50_ns=200 p90_ns=200 p99_ns=200 max_ns=200\n"
        );
    }

    #[test]
    fn wake_ups_by_cpu_and_interrupts_by_irq() {
        let cpu0 = bytes(&[
            (&SCHED_WAKE_RUN, 5, &[3, 7, 40]),
            (&IRQ_DELIVER, 10, &[11, 0, 1, 5, 1]),
        ]);
        let cpu1 = bytes(&[
            (&SCHED_WAKE_RUN, 6, &[4, 8, 2]),
            (&IRQ_ACK, 35, &[11, 0, 5]),
            (&IRQ_ACK, 36, &[11, 0, 4]),
        ]);
        // The KVM clock counts nanoseconds; a wait is in the clock's ticks too.
        assert_eq!(
            view(&[cpu0, cpu1], Some("clock kvmclock period_fs 1000000\n")),
            "latency wake_run cpu=0 count=1 min_ns=40 mean_ns=40 p50_ns=40 p90_ns=40 \
             p99_ns=40 max_ns=40\n\
             latency wake_run cpu=1 count=1 min_ns=2 mean_ns=2 p50_ns=2 p90_ns=2 p99_ns=2 \
             max_ns=2\n\
             latency wake_run cpu=all count=2 min_ns=2 mean_ns=21 p50_ns=2 p90_ns=40 \
             p99_ns=40 max_ns=40\n\
             latency irq irq=11 count=1 open=0 min_ns=25 mean_ns=25 p50_ns=25 p90_ns=25 \
             p99_ns=25 max_ns=25\n\
             latency irq irq=all count=1 open=0 unmatched=1 min_ns=25 mean_ns=25 p50_ns=25 \
             p90_ns=25 p99_ns=25 max_ns=25\n"
        );
    }
}
