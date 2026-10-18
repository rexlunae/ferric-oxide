//! Export to the Chrome trace event JSON format, which the Perfetto UI opens.
//!
//! The output is one JSON object, `{"traceEvents": [...], "displayTimeUnit": "ns"}`, using
//! these event phases from Google's "Trace Event Format" document:
//!
//! - **CPU tracks.** Process "CPUs" has one thread per CPU. With `sched.switch` captured, each
//!   CPU's thread has one slice per context that ran there (`B`/`E`), named after the process
//!   and context.
//! - **Context tracks.** Each process (by kernel pid) has one thread per context (by its debug
//!   id). System calls are slices on the context that made them: `syscall.enter` begins one
//!   and the matching `syscall.exit` ends it, even when the context moved to another CPU in
//!   between. Records are attributed to the context `sched.switch` last put on their CPU; before
//!   the first switch of a CPU they go to that CPU's "context unknown" thread.
//! - **Scheme requests** are async slices (`b`/`e`, category `scheme`) from `scheme.request` to
//!   the `scheme.response` with the same scheme id and tag, on the caller's track, with the
//!   responder's pid in the end event's arguments. A `scheme.cancel` is an async instant (`n`)
//!   on the same slice.
//! - A record that has no matching start (its start was not captured) is an instant (`i`).
//!
//! Timestamps are microseconds since the first record, from TSC cycles and the TSC frequency
//! the caller gives.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ferric_trace::Event;
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
    tsc_mhz: f64,
    events: Vec<TraceEvent>,
    /// The context `sched.switch` last put on each CPU.
    running: HashMap<u32, Track>,
    /// Whether each CPU's track has an open slice.
    cpu_open: HashMap<u32, bool>,
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
        timestamp.saturating_sub(self.base) as f64 / self.tsc_mhz
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
        let opcode = u8::try_from(entry.arg(2))
            .ok()
            .and_then(Opcode::try_from_raw)
            .map_or_else(|| format!("op {}", entry.arg(2)), |op| format!("{op:?}"));
        let name = format!("scheme {scheme} {opcode}");
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

/// The JSON document for `capture`, with the TSC running at `tsc_mhz` MHz.
pub fn to_json(capture: &Capture, tsc_mhz: f64) -> String {
    let mut converter = Converter {
        base: capture.entries().first().map_or(0, |e| e.timestamp),
        tsc_mhz: if tsc_mhz > 0.0 { tsc_mhz } else { 1.0 },
        events: Vec::new(),
        running: HashMap::new(),
        cpu_open: HashMap::new(),
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
        let all = events(&to_json(&capture, 10.0));

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
        let all = events(&to_json(&capture, 1000.0));
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
}
