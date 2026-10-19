//! Export to the Chrome trace event JSON format, which the Perfetto UI opens.
//!
//! The output is one JSON object, `{"traceEvents": [...], "displayTimeUnit": "ns"}`, using
//! these event phases from Google's "Trace Event Format" document:
//!
//! - **CPU tracks.** Process "CPUs" has one thread per CPU. With `sched.switch` captured, each
//!   CPU's thread has one slice per context that ran there (`B`/`E`), named after the process
//!   and context. The context a CPU's first switch takes off it gets a slice from that CPU's
//!   first record to the switch.
//! - **Context tracks.** Each process (by kernel pid) has one thread per context (by its debug
//!   id). System calls are slices on the context that made them: `syscall.enter` begins one
//!   and the matching `syscall.exit` ends it, even when the context moved to another CPU in
//!   between. Records are attributed to the context `sched.switch` last put on their CPU.
//!   Records from before a CPU's first switch go to that CPU's "context unknown" thread until
//!   the switch names the context it switches out; they then move to that context, with its
//!   open system calls and requests, so a call it began there still ends when it finishes on
//!   another CPU.
//! - **Scheme requests** are async slices (`b`/`e`, category `scheme`) from `scheme.request` to
//!   the `scheme.response` with the same scheme id and tag, on the caller's track, with the
//!   responder's pid in the end event's arguments. A `scheme.cancel` does not end the
//!   request (the scheme still answers it): it is an async instant (`n`) on the same slice.
//! - A record that has no matching start (its start was not captured) is an instant (`i`).
//!
//! Timestamps are microseconds since the first record. The rate ticks are divided by is a
//! [`TickRate`]: for TSC cycles the TSC frequency the caller gives (a [`TscMhz`]), and for a
//! capture whose `clock` names a clock with a known tick length (the KVM clock, the HPET) that
//! clock's own rate, however slow ([`TickRate::for_clock`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::num::ParseFloatError;
use std::str::FromStr;

use ferric_trace::Event;
use ferric_trace::clock::ClockLine;
use ferric_trace::kernel::{
    SCHED_SWITCH, SCHEME_CANCEL, SCHEME_REQUEST, SCHEME_RESPONSE, SYSCALL_ENTER, SYSCALL_EXIT,
};
use serde::Serialize;
use serde_json::{Map, Value};
use syscall::schemev2::Opcode;

use crate::capture::{Capture, Entry};

/// The pid of the "CPUs" process, whose threads are the CPUs.
pub const CPU_PID: u64 = 1 << 32;

/// The pid of the process holding each CPU's "context unknown" thread.
pub const UNKNOWN_PID: u64 = (1 << 32) + 1;

/// A TSC frequency in MHz: finite and at least [`TscMhz::MIN`], so every cycle count converts
/// to a finite time.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct TscMhz(f64);

/// Why a TSC frequency was refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TscMhzError {
    /// The value is below [`TscMhz::MIN`], infinite or not a number.
    #[error("TSC frequency {value} MHz is not a finite number of at least {} MHz", TscMhz::MIN.0)]
    OutOfRange {
        /// The value given.
        value: f64,
    },
    /// The text is not a number.
    #[error("TSC frequency is not a number")]
    Parse(#[from] ParseFloatError),
}

impl TscMhz {
    /// The lowest frequency accepted, 1 MHz. Real TSCs run at hundreds of MHz or more, so this
    /// refuses nothing real. Dividing any `u64` cycle count by at least 1 gives a finite
    /// result, while a tiny frequency such as `5e-324` overflows timestamps to infinity, which
    /// JSON cannot represent.
    pub const MIN: Self = Self(1.0);

    /// `mhz` as a TSC frequency.
    ///
    /// # Errors
    ///
    /// [`TscMhzError::OutOfRange`] unless `mhz` is finite and at least [`TscMhz::MIN`].
    pub fn new(mhz: f64) -> Result<Self, TscMhzError> {
        if mhz.is_finite() && mhz >= Self::MIN.0 {
            Ok(Self(mhz))
        } else {
            Err(TscMhzError::OutOfRange { value: mhz })
        }
    }

    /// The frequency in MHz.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl FromStr for TscMhz {
    type Err = TscMhzError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text.trim().parse()?)
    }
}

impl fmt::Display for TscMhz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The rate timestamps are divided by to get microseconds: ticks per microsecond, positive,
/// finite and at least [`TickRate::MIN`].
///
/// Unlike [`TscMhz`], which checks a frequency a person typed, it takes the slow clocks a
/// [`ClockLine`] can name, such as an HPET whose tick is longer than a microsecond.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct TickRate(f64);

/// Why a tick rate was refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TickRateError {
    /// The value is below [`TickRate::MIN`], infinite or not a number.
    #[error(
        "tick rate {value} per microsecond is not a finite number of at least {}",
        TickRate::MIN.0
    )]
    OutOfRange {
        /// The value given.
        value: f64,
    },
}

impl TickRate {
    /// The lowest rate accepted, 10^-11 ticks per microsecond. It is below the slowest clock a
    /// [`ClockLine`] can describe (a period of `u64::MAX` femtoseconds is about 5.4 × 10^-11
    /// ticks per microsecond), and dividing any `u64` tick count by it still gives a finite
    /// time, which JSON needs.
    pub const MIN: Self = Self(1e-11);

    /// `ticks_per_us` as a tick rate.
    ///
    /// # Errors
    ///
    /// [`TickRateError::OutOfRange`] unless `ticks_per_us` is finite and at least
    /// [`TickRate::MIN`].
    pub fn new(ticks_per_us: f64) -> Result<Self, TickRateError> {
        if ticks_per_us.is_finite() && ticks_per_us >= Self::MIN.0 {
            Ok(Self(ticks_per_us))
        } else {
            Err(TickRateError::OutOfRange {
                value: ticks_per_us,
            })
        }
    }

    /// Ticks per microsecond.
    pub fn get(self) -> f64 {
        self.0
    }

    /// The rate to turn timestamps of `clock` into time: the clock's ticks per microsecond
    /// when its tick length is known, otherwise `tsc` (TSC cycles, or a capture from a kernel
    /// that does not say).
    pub fn for_clock(clock: Option<ClockLine>, tsc: TscMhz) -> Self {
        clock
            .and_then(|line| line.ticks_per_us())
            .and_then(|rate| Self::new(rate).ok())
            .unwrap_or(tsc.into())
    }
}

impl From<TscMhz> for TickRate {
    /// A TSC frequency in MHz is the TSC's ticks per microsecond.
    fn from(tsc: TscMhz) -> Self {
        Self(tsc.0)
    }
}

impl fmt::Display for TickRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One trace event of the JSON format.
#[derive(Debug, Serialize)]
struct TraceEvent {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cat: Option<&'static str>,
    ph: &'static str,
    ts: f64,
    pid: u64,
    tid: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    s: Option<&'static str>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    args: Map<String, Value>,
}

/// A thread of the output: a process id and a thread id.
type Track = (u64, u64);

/// A scheme request waiting for its response.
struct OpenRequest {
    id: u64,
    name: String,
    track: Track,
}

/// The conversion state.
struct Converter {
    base: u64,
    rate: TickRate,
    events: Vec<TraceEvent>,
    /// The context `sched.switch` last put on each CPU.
    running: HashMap<u32, Track>,
    /// Whether each CPU's track has an open slice.
    cpu_open: HashMap<u32, bool>,
    /// The timestamp of each CPU's first record.
    first_seen: HashMap<u32, u64>,
    /// The system call numbers each context has entered and not left.
    syscalls: HashMap<Track, Vec<u64>>,
    /// Scheme requests by (scheme, tag).
    requests: HashMap<(u64, u64), OpenRequest>,
    next_request: u64,
    /// The context tracks seen, for their names.
    contexts: BTreeSet<Track>,
}

impl Converter {
    fn ts(&self, timestamp: u64) -> f64 {
        timestamp.saturating_sub(self.base) as f64 / self.rate.get()
    }

    fn push(&mut self, entry: &Entry, ph: &'static str, name: String, track: Track) -> usize {
        self.events.push(TraceEvent {
            name,
            cat: None,
            ph,
            ts: self.ts(entry.timestamp),
            pid: track.0,
            tid: track.1,
            id: None,
            s: None,
            args: Map::new(),
        });
        self.events.len() - 1
    }

    /// Adds `entry`'s arguments to event `index`.
    fn with_args(&mut self, index: usize, entry: &Entry) -> &mut TraceEvent {
        let event = &mut self.events[index];
        for (name, value) in entry.event.args().iter().zip(&entry.args) {
            event.args.insert((*name).to_owned(), Value::from(*value));
        }
        event
    }

    /// The track `entry` belongs to.
    fn track(&mut self, entry: &Entry) -> Track {
        let track = self
            .running
            .get(&entry.cpu)
            .copied()
            .unwrap_or((UNKNOWN_PID, u64::from(entry.cpu)));
        if track.0 != UNKNOWN_PID {
            self.contexts.insert(track);
        }
        track
    }

    fn entry(&mut self, entry: &Entry) {
        self.first_seen.entry(entry.cpu).or_insert(entry.timestamp);
        let event: &Event = entry.event;
        if event.id() == SCHED_SWITCH.id() {
            self.switch(entry);
        } else if event.id() == SYSCALL_ENTER.id() {
            let track = self.track(entry);
            let i = self.push(entry, "B", format!("syscall {:#x}", entry.arg(0)), track);
            self.with_args(i, entry).cat = Some("syscall");
            self.syscalls.entry(track).or_default().push(entry.arg(0));
        } else if event.id() == SYSCALL_EXIT.id() {
            self.syscall_exit(entry);
        } else if event.id() == SCHEME_REQUEST.id() {
            self.request(entry);
        } else if event.id() == SCHEME_RESPONSE.id() || event.id() == SCHEME_CANCEL.id() {
            self.response(entry);
        } else {
            let track = self.track(entry);
            let i = self.push(entry, "i", event.name().to_owned(), track);
            self.with_args(i, entry).s = Some("t");
        }
    }

    fn switch(&mut self, entry: &Entry) {
        let cpu_track = (CPU_PID, u64::from(entry.cpu));
        if !self.running.contains_key(&entry.cpu) {
            // The CPU's first switch names the context that ran there until now: move its
            // records to it, and give it the CPU slice from the CPU's first record.
            let prev = (entry.arg(0), entry.arg(2));
            self.reattribute(entry.cpu, prev);
            let first = self.first_seen.get(&entry.cpu).copied();
            if let Some(first) = first.filter(|&t| t < entry.timestamp) {
                let mut begin = meta_event(
                    "B",
                    format!("pid {} ctx {}", prev.0, prev.1),
                    self.ts(first),
                    cpu_track,
                );
                begin.cat = Some("sched");
                self.events.push(begin);
                self.cpu_open.insert(entry.cpu, true);
                self.contexts.insert(prev);
            }
        }
        if self.cpu_open.insert(entry.cpu, true) == Some(true) {
            self.push(entry, "E", String::new(), cpu_track);
        }
        let (next_pid, next_id) = (entry.arg(1), entry.arg(3));
        let i = self.push(
            entry,
            "B",
            format!("pid {next_pid} ctx {next_id}"),
            cpu_track,
        );
        self.with_args(i, entry).cat = Some("sched");
        self.running.insert(entry.cpu, (next_pid, next_id));
        self.contexts.insert((next_pid, next_id));
    }

    /// Moves everything recorded on CPU `cpu`'s "context unknown" track to `context`: the
    /// events already written, the system calls still open and the requests still waiting.
    fn reattribute(&mut self, cpu: u32, context: Track) {
        let unknown = (UNKNOWN_PID, u64::from(cpu));
        let mut moved = false;
        for event in &mut self.events {
            if (event.pid, event.tid) == unknown {
                (event.pid, event.tid) = context;
                moved = true;
            }
        }
        for request in self.requests.values_mut() {
            if request.track == unknown {
                request.track = context;
                moved = true;
            }
        }
        if let Some(open) = self.syscalls.remove(&unknown) {
            moved |= !open.is_empty();
            // Calls the context made elsewhere before it came here are older, so these go on
            // top of them.
            self.syscalls.entry(context).or_default().extend(open);
        }
        if moved {
            self.contexts.insert(context);
        }
    }

    fn syscall_exit(&mut self, entry: &Entry) {
        let track = self.track(entry);
        let nr = entry.arg(0);
        let stack = self.syscalls.entry(track).or_default();
        if stack.last() == Some(&nr) {
            stack.pop();
            let i = self.push(entry, "E", String::new(), track);
            self.with_args(i, entry);
        } else {
            let i = self.push(entry, "i", format!("syscall {nr:#x} exit"), track);
            let event = self.with_args(i, entry);
            event.cat = Some("syscall");
            event.s = Some("t");
        }
    }

    fn request(&mut self, entry: &Entry) {
        let track = self.track(entry);
        let (scheme, tag) = (entry.arg(0), entry.arg(1));
        let name = format!("scheme {scheme} {}", opcode_name(entry.arg(2)));
        let id = self.next_request;
        self.next_request += 1;
        let i = self.push(entry, "b", name.clone(), track);
        let event = self.with_args(i, entry);
        event.cat = Some("scheme");
        event.id = Some(format!("{id:#x}"));
        // A tag still open was never answered in the capture; its slice stays open.
        self.requests
            .insert((scheme, tag), OpenRequest { id, name, track });
    }

    fn response(&mut self, entry: &Entry) {
        let key = (entry.arg(0), entry.arg(1));
        let cancel = entry.event.id() == SCHEME_CANCEL.id();
        let open = if cancel {
            self.requests
                .get(&key)
                .map(|r| (r.id, r.name.clone(), r.track))
        } else {
            self.requests.remove(&key).map(|r| (r.id, r.name, r.track))
        };
        let Some((id, name, track)) = open else {
            let track = self.track(entry);
            let i = self.push(entry, "i", entry.event.name().to_owned(), track);
            let event = self.with_args(i, entry);
            event.cat = Some("scheme");
            event.s = Some("t");
            return;
        };
        let (ph, name) = if cancel {
            ("n", "cancel".to_owned())
        } else {
            ("e", name)
        };
        let i = self.push(entry, ph, name, track);
        let event = self.with_args(i, entry);
        event.cat = Some("scheme");
        event.id = Some(format!("{id:#x}"));
    }

    /// Ends the CPU slices still open at the last record, and adds the track names.
    fn finish(mut self, capture: &Capture) -> Vec<TraceEvent> {
        if let Some(last) = capture.entries().last() {
            let ts = self.ts(last.timestamp);
            for (&cpu, _) in self.cpu_open.iter().filter(|(_, open)| **open) {
                self.events.push(meta_event(
                    "E",
                    String::new(),
                    ts,
                    (CPU_PID, u64::from(cpu)),
                ));
            }
        }
        let mut names = vec![name_event("process_name", "CPUs", (CPU_PID, 0))];
        for &cpu in capture.cpus() {
            names.push(name_event(
                "thread_name",
                &format!("CPU {cpu}"),
                (CPU_PID, u64::from(cpu)),
            ));
            names.push(name_event(
                "thread_name",
                &format!("CPU {cpu} (context unknown)"),
                (UNKNOWN_PID, u64::from(cpu)),
            ));
        }
        names.push(name_event(
            "process_name",
            "context unknown",
            (UNKNOWN_PID, 0),
        ));
        let pids: BTreeMap<u64, ()> = self.contexts.iter().map(|&(pid, _)| (pid, ())).collect();
        for pid in pids.keys() {
            names.push(name_event("process_name", &format!("pid {pid}"), (*pid, 0)));
        }
        for &(pid, id) in &self.contexts {
            names.push(name_event(
                "thread_name",
                &format!("context {id}"),
                (pid, id),
            ));
        }
        names.extend(self.events);
        names
    }
}

/// A scheme opcode's name (`Write`), or `op <n>` for a number `schemev2::Opcode` does not know.
pub(crate) fn opcode_name(raw: u64) -> String {
    u8::try_from(raw)
        .ok()
        .and_then(Opcode::try_from_raw)
        .map_or_else(|| format!("op {raw}"), |op| format!("{op:?}"))
}

/// An event without arguments at `ts`.
fn meta_event(ph: &'static str, name: String, ts: f64, track: Track) -> TraceEvent {
    TraceEvent {
        name,
        cat: None,
        ph,
        ts,
        pid: track.0,
        tid: track.1,
        id: None,
        s: None,
        args: Map::new(),
    }
}

/// A metadata event naming a process or thread.
fn name_event(kind: &str, name: &str, track: Track) -> TraceEvent {
    let mut event = meta_event("M", kind.to_owned(), 0.0, track);
    event.args.insert("name".to_owned(), Value::from(name));
    event
}

/// The JSON document for `capture`, with its timestamps ticking at `rate` (a [`TscMhz`] for
/// TSC cycles, or [`TickRate::for_clock`] for the capture's own clock).
pub fn to_json(capture: &Capture, rate: impl Into<TickRate>) -> String {
    let mut converter = Converter {
        base: capture.entries().first().map_or(0, |e| e.timestamp),
        rate: rate.into(),
        events: Vec::new(),
        running: HashMap::new(),
        cpu_open: HashMap::new(),
        first_seen: HashMap::new(),
        syscalls: HashMap::new(),
        requests: HashMap::new(),
        next_request: 0,
        contexts: BTreeSet::new(),
    };
    for entry in capture.entries() {
        converter.entry(entry);
    }
    let events = converter.finish(capture);
    serde_json::json!({ "traceEvents": events, "displayTimeUnit": "ns" }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CpuCapture, tests::bytes};

    fn mhz(value: f64) -> TscMhz {
        TscMhz::new(value).expect("a valid frequency")
    }

    fn rate(value: f64) -> TickRate {
        TickRate::new(value).expect("a valid rate")
    }

    #[test]
    fn rate_follows_the_clock() {
        use ferric_trace::clock::Clock;
        let tsc = mhz(2400.0);
        assert_eq!(TickRate::for_clock(None, tsc), rate(2400.0));
        let line = |clock, period_fs| ClockLine { clock, period_fs };
        assert_eq!(
            TickRate::for_clock(Some(line(Clock::Tsc, None)), tsc),
            rate(2400.0)
        );
        assert_eq!(
            TickRate::for_clock(Some(line(Clock::KvmClock, Some(1_000_000))), tsc),
            rate(1000.0)
        );
        assert_eq!(
            TickRate::for_clock(Some(line(Clock::Hpet, Some(10_000_000))), tsc),
            rate(100.0)
        );
        // A tick longer than a microsecond is below TscMhz's minimum but still the clock's rate.
        assert_eq!(
            TickRate::for_clock(Some(line(Clock::Hpet, Some(2_000_000_000))), tsc),
            rate(0.5)
        );
        // The slowest clock a line can describe still has a rate.
        let slowest = TickRate::for_clock(Some(line(Clock::Hpet, Some(u64::MAX))), tsc);
        assert_eq!(slowest, rate(1e9 / u64::MAX as f64));
    }

    #[test]
    fn tick_rate_refuses_what_is_not_a_rate() {
        for bad in [0.0, -1.0, f64::INFINITY, f64::NAN, 5e-324, 1e-12] {
            assert!(
                matches!(TickRate::new(bad), Err(TickRateError::OutOfRange { .. })),
                "{bad}"
            );
        }
        assert_eq!(TickRate::new(1e-11).ok(), Some(TickRate::MIN));
        assert_eq!(TickRate::from(mhz(2400.0)), rate(2400.0));
    }

    fn events(json: &str) -> Vec<Value> {
        let doc: Value = serde_json::from_str(json).expect("valid JSON");
        doc["traceEvents"].as_array().expect("an array").clone()
    }

    fn phases<'a>(events: &'a [Value], ph: &str) -> Vec<&'a Value> {
        events.iter().filter(|e| e["ph"] == ph).collect()
    }

    #[test]
    fn syscalls_are_slices_on_their_context() {
        // CPU 0 runs context 7 of pid 3, which enters write and is switched out; it finishes
        // the call on CPU 1.
        let cpu0 = bytes(&[
            (&SCHED_SWITCH, 100, &[0, 3, 1, 7]),
            (&SYSCALL_ENTER, 110, &[4, 9, 0, 16, 0]),
            (&SCHED_SWITCH, 120, &[3, 0, 7, 1]),
        ]);
        let cpu1 = bytes(&[
            (&SCHED_SWITCH, 130, &[0, 3, 2, 7]),
            (&SYSCALL_EXIT, 140, &[4, 16]),
            (&SYSCALL_EXIT, 150, &[5, 0]),
        ]);
        let capture = Capture::from_cpus(vec![
            CpuCapture::decode(0, &cpu0).expect("valid"),
            CpuCapture::decode(1, &cpu1).expect("valid"),
        ]);
        let all = events(&to_json(&capture, mhz(10.0)));

        let begin = phases(&all, "B")
            .into_iter()
            .find(|e| e["cat"] == "syscall")
            .expect("a syscall slice");
        assert_eq!(
            (begin["pid"].as_u64(), begin["tid"].as_u64()),
            (Some(3), Some(7))
        );
        assert_eq!(begin["ts"], 1.0);
        assert_eq!(begin["args"]["c"], 16);
        let end = phases(&all, "E")
            .into_iter()
            .find(|e| e["pid"] == 3)
            .expect("its end, on the same context");
        assert_eq!(
            (end["tid"].as_u64(), end["ts"].as_f64()),
            (Some(7), Some(4.0))
        );
        assert_eq!(end["args"]["ret"], 16);
        // The exit of a call whose start was not captured is an instant.
        assert!(
            phases(&all, "i")
                .iter()
                .any(|e| e["name"] == "syscall 0x5 exit")
        );

        // CPU tracks: three switches open three slices; two are closed by the next switch on
        // their CPU and the rest at the end.
        let cpu_slices = |ph| {
            phases(&all, ph)
                .into_iter()
                .filter(|e| e["pid"] == CPU_PID)
                .count()
        };
        assert_eq!(cpu_slices("B"), 3);
        assert_eq!(cpu_slices("E"), 3);
        let names: Vec<_> = phases(&all, "M")
            .iter()
            .map(|e| e["args"]["name"].clone())
            .collect();
        assert!(names.contains(&Value::from("pid 3")));
        assert!(names.contains(&Value::from("context 7")));
        assert!(names.contains(&Value::from("CPU 1")));
    }

    #[test]
    fn scheme_requests_are_async_slices() {
        let cpu0 = bytes(&[
            // An unknown context requests a write (opcode 6) of 32 bytes on scheme 2, tag 5.
            (&SCHEME_REQUEST, 1000, &[2, 5, 6, 40, 1, 32]),
            (&SCHEME_CANCEL, 1500, &[2, 5]),
        ]);
        let cpu1 = bytes(&[
            (&SCHEME_RESPONSE, 2000, &[2, 5, 32, 12]),
            // Tag 5 again: a new request, then a response nothing asked for.
            (&SCHEME_REQUEST, 3000, &[2, 5, 99, 40, 0, 0]),
            (&SCHEME_RESPONSE, 3500, &[2, 6, 0, 12]),
        ]);
        let capture = Capture::from_cpus(vec![
            CpuCapture::decode(0, &cpu0).expect("valid"),
            CpuCapture::decode(1, &cpu1).expect("valid"),
        ]);
        let all = events(&to_json(&capture, mhz(1000.0)));
        let begins = phases(&all, "b");
        assert_eq!(begins.len(), 2);
        assert_eq!(begins[0]["name"], "scheme 2 Write");
        assert_eq!(begins[1]["name"], "scheme 2 op 99");
        assert_ne!(begins[0]["id"], begins[1]["id"]);
        let end = phases(&all, "e");
        assert_eq!(end.len(), 1);
        assert_eq!(end[0]["id"], begins[0]["id"]);
        assert_eq!(
            (end[0]["pid"].as_u64(), end[0]["tid"].as_u64()),
            (Some(UNKNOWN_PID), Some(0))
        );
        assert_eq!(end[0]["args"]["responder"], 12);
        assert_eq!(end[0]["ts"], 1.0);
        let cancel = phases(&all, "n");
        assert_eq!(cancel.len(), 1);
        assert_eq!(cancel[0]["id"], begins[0]["id"]);
        assert!(
            phases(&all, "i")
                .iter()
                .any(|e| e["name"] == "scheme.response")
        );
    }
    #[test]
    fn tsc_frequency_must_be_finite_and_positive() {
        for bad in [
            0.0,
            -1.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            5e-324,
            f64::MIN_POSITIVE,
            0.999,
        ] {
            assert!(
                matches!(TscMhz::new(bad), Err(TscMhzError::OutOfRange { .. })),
                "{bad}"
            );
        }
        assert!(matches!(
            "0".parse::<TscMhz>(),
            Err(TscMhzError::OutOfRange { .. })
        ));
        assert!(matches!(
            "inf".parse::<TscMhz>(),
            Err(TscMhzError::OutOfRange { .. })
        ));
        assert!(matches!(
            "fast".parse::<TscMhz>(),
            Err(TscMhzError::Parse(_))
        ));
        assert_eq!(
            " 2400.5 ".parse::<TscMhz>().map(TscMhz::get).ok(),
            Some(2400.5)
        );
        assert_eq!(TscMhz::new(1.0).ok(), Some(TscMhz::MIN));
    }

    #[test]
    fn timestamps_stay_finite_at_the_extremes() {
        // The widest possible span of cycles, at the lowest and highest frequencies accepted.
        let cpu0 = bytes(&[
            (&SYSCALL_EXIT, 0, &[1, 0]),
            (&SYSCALL_EXIT, u64::MAX, &[1, 0]),
        ]);
        let capture = Capture::from_cpus(vec![CpuCapture::decode(0, &cpu0).expect("valid")]);
        for tsc in [TickRate::MIN, TscMhz::MIN.into(), mhz(f64::MAX).into()] {
            let all = events(&to_json(&capture, tsc));
            let stamps: Vec<_> = phases(&all, "i").iter().map(|e| e["ts"].as_f64()).collect();
            assert_eq!(stamps.len(), 2);
            assert!(
                stamps.iter().all(|t| t.is_some_and(f64::is_finite)),
                "{tsc}: {stamps:?}"
            );
        }
    }

    #[test]
    fn records_before_the_first_switch_move_to_the_outgoing_context() {
        // CPU 0: pid 3 context 7 enters write and makes a request before any switch was
        // captured, then is switched out (to pid 0 context 1) and migrates to CPU 1, where the
        // call returns.
        let cpu0 = bytes(&[
            (&SYSCALL_ENTER, 10, &[4, 9, 0, 16, 0]),
            (&SCHEME_REQUEST, 15, &[2, 5, 6, 3, 1, 16]),
            (&SCHED_SWITCH, 20, &[3, 0, 7, 1]),
        ]);
        let cpu1 = bytes(&[
            (&SCHED_SWITCH, 25, &[0, 3, 2, 7]),
            (&SCHEME_RESPONSE, 28, &[2, 5, 16, 12]),
            (&SYSCALL_EXIT, 30, &[4, 16]),
        ]);
        let capture = Capture::from_cpus(vec![
            CpuCapture::decode(0, &cpu0).expect("valid"),
            CpuCapture::decode(1, &cpu1).expect("valid"),
        ]);
        let all = events(&to_json(&capture, mhz(1.0)));
        let on_context = |e: &&Value| e["pid"] == 3 && e["tid"] == 7;

        let begin = phases(&all, "B")
            .into_iter()
            .find(|e| e["cat"] == "syscall")
            .expect("the syscall begin");
        assert!(on_context(&begin), "{begin}");
        assert_eq!(begin["ts"], 0.0);
        let end = phases(&all, "E")
            .into_iter()
            .find(on_context)
            .expect("its end, on the same context");
        assert_eq!(end["ts"], 20.0);
        assert!(
            !phases(&all, "i")
                .iter()
                .any(|e| e["name"] == "syscall 0x4 exit"),
            "the exit matched its begin"
        );

        let request = phases(&all, "b");
        assert_eq!(request.len(), 1);
        assert!(on_context(&request[0]), "{}", request[0]);
        let response = phases(&all, "e");
        assert_eq!(response.len(), 1);
        assert!(on_context(&response[0]), "{}", response[0]);

        // Nothing but the track names is left on the unknown context.
        assert!(
            all.iter()
                .filter(|e| e["ph"] != "M")
                .all(|e| e["pid"] != UNKNOWN_PID)
        );
    }

    #[test]
    fn the_first_switch_gives_the_outgoing_context_its_cpu_slice() {
        // CPU 0's first record is a system call at 10; the first switch, at 20, takes pid 3
        // context 7 off for pid 4 context 8.
        let cpu0 = bytes(&[
            (&SYSCALL_ENTER, 10, &[4, 9, 0, 16, 0]),
            (&SCHED_SWITCH, 20, &[3, 4, 7, 8]),
            (&SYSCALL_EXIT, 30, &[5, 0]),
        ]);
        // CPU 1's first record is its switch, so there is no earlier interval to show.
        let cpu1 = bytes(&[(&SCHED_SWITCH, 15, &[0, 3, 1, 7])]);
        let capture = Capture::from_cpus(vec![
            CpuCapture::decode(0, &cpu0).expect("valid"),
            CpuCapture::decode(1, &cpu1).expect("valid"),
        ]);
        let all = events(&to_json(&capture, mhz(1.0)));
        let on_cpu = |cpu: u64, ph: &str| -> Vec<(String, f64)> {
            phases(&all, ph)
                .into_iter()
                .filter(|e| e["pid"] == CPU_PID && e["tid"] == cpu)
                .map(|e| {
                    (
                        e["name"].as_str().unwrap_or_default().to_owned(),
                        e["ts"].as_f64().unwrap_or(f64::NAN),
                    )
                })
                .collect()
        };
        assert_eq!(
            on_cpu(0, "B"),
            [
                ("pid 3 ctx 7".to_owned(), 0.0),
                ("pid 4 ctx 8".to_owned(), 10.0)
            ]
        );
        // The first slice ends at the switch, the second at the last record.
        let ends: Vec<f64> = on_cpu(0, "E").into_iter().map(|(_, t)| t).collect();
        assert_eq!(ends, [10.0, 20.0]);
        assert_eq!(on_cpu(1, "B"), [("pid 3 ctx 7".to_owned(), 5.0)]);
    }

    #[test]
    fn a_cancel_does_not_end_the_request() {
        let cpu0 = bytes(&[
            (&SCHEME_REQUEST, 0, &[2, 5, 6, 3, 1, 16]),
            (&SCHEME_CANCEL, 10, &[2, 5]),
            (&SCHEME_CANCEL, 11, &[2, 5]),
            (&SCHEME_RESPONSE, 20, &[2, 5, (-4i64) as u64, 12]),
        ]);
        let capture = Capture::from_cpus(vec![CpuCapture::decode(0, &cpu0).expect("valid")]);
        let all = events(&to_json(&capture, mhz(1.0)));
        let begin = phases(&all, "b");
        let end = phases(&all, "e");
        assert_eq!((begin.len(), end.len(), phases(&all, "n").len()), (1, 1, 2));
        assert_eq!(end[0]["id"], begin[0]["id"]);
        assert_eq!(end[0]["ts"], 20.0);
    }
}
