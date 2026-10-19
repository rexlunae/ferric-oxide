//! Export to OpenTelemetry: a capture's [`Spans`] as an OTLP/JSON traces document.
//!
//! The output is the JSON encoding of OTLP's `ExportTraceServiceRequest`, as the
//! OpenTelemetry Protocol specification (OTLP 1.x, "JSON Protobuf Encoding") defines it over
//! `opentelemetry/proto/trace/v1/trace.proto`: field names in lowerCamelCase, trace and span
//! ids as hexadecimal strings, enumerations as integers and 64-bit integers as decimal
//! strings. An OTLP/HTTP collector takes it as is on `POST /v1/traces` with
//! `Content-Type: application/json`, and the Collector's `otlpjsonfile` receiver reads it from
//! a file.
//!
//! - **Resources.** One resource per process: `service.name` is `pid <n>`, with `process.pid`
//!   and `service.namespace` `ferric-oxide`. Spans of a context the capture never named are
//!   under the resource `context unknown`.
//! - **Spans.** Each [`Span`] becomes one span. A system call is an internal span named
//!   `syscall <nr>`, a scheme request a client span named `scheme <id> <opcode>` (the names
//!   the Perfetto export uses), and a record outside every span a span of zero length named
//!   after its event. A span with no parent starts its own trace; its children share it, so a
//!   system call and the scheme requests it made are one trace.
//! - **Attributes.** `thread.id` is the context's debug id, `cpu.logical_number` the CPU the
//!   span began on (and `ferric.end_cpu` the one it ended on, when another). Each argument of
//!   the opening and closing records is `<event>.<argument>` as a hexadecimal string, the
//!   text view's notation.
//! - **Events.** The records attached to a span (a cancel, or any record made during it) are
//!   span events named after their event, with their arguments.
//! - **Status.** A system call or request whose result is an error number has status `ERROR`
//!   with the error's text; others leave it unset. A span still open when the capture ends
//!   ends at its context's last record ([`Spans::last_seen`]), with `ferric.unfinished` set.
//!
//! Times are Unix nanoseconds: the first record is at [`Options::start_unix_ns`], and later
//! ones are converted with the capture's [`TickRate`]. Trace ids begin with a hash of the
//! start time and every record, so two captures sent to one backend do not share ids unless
//! they are the same capture sent twice with the same start.

use std::collections::BTreeMap;

use ferric_trace::kernel::{SCHEME_RESPONSE, SYSCALL_EXIT};
use serde::Serialize;
use syscall::error::Error as Errno;

use crate::capture::{Capture, Entry};
use crate::perfetto::{TickRate, opcode_name};
use crate::spans::{Context, Span, SpanKind, Spans};

/// The instrumentation scope named in the output.
pub const SCOPE_NAME: &str = "ferric-trace";

/// The `service.namespace` of every resource.
pub const SERVICE_NAMESPACE: &str = "ferric-oxide";

/// How to place a capture's timestamps in time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// The rate the capture's timestamps tick at.
    pub rate: TickRate,
    /// The Unix time of the first record, in nanoseconds. A capture does not record the wall
    /// time, so with 0 every time is the time since the first record.
    pub start_unix_ns: u64,
}

/// `SPAN_KIND_INTERNAL`.
const KIND_INTERNAL: u8 = 1;
/// `SPAN_KIND_CLIENT`.
const KIND_CLIENT: u8 = 3;
/// `STATUS_CODE_ERROR`.
const STATUS_ERROR: u8 = 2;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TracesData {
    resource_spans: Vec<ResourceSpans>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceSpans {
    resource: Resource,
    scope_spans: [ScopeSpans; 1],
}

#[derive(Serialize)]
struct Resource {
    attributes: Vec<KeyValue>,
}

#[derive(Serialize)]
struct ScopeSpans {
    scope: Scope,
    spans: Vec<OtlpSpan>,
}

#[derive(Serialize)]
struct Scope {
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OtlpSpan {
    trace_id: String,
    span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_span_id: Option<String>,
    name: String,
    kind: u8,
    start_time_unix_nano: String,
    end_time_unix_nano: String,
    attributes: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    events: Vec<SpanEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<Status>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SpanEvent {
    time_unix_nano: String,
    name: &'static str,
    attributes: Vec<KeyValue>,
}

#[derive(Serialize)]
struct Status {
    code: u8,
    message: &'static str,
}

#[derive(Serialize)]
struct KeyValue {
    key: String,
    value: AnyValue,
}

/// OTLP's `AnyValue`, one of its fields set.
#[derive(Serialize)]
enum AnyValue {
    #[serde(rename = "stringValue")]
    String(String),
    /// An `int64`, as a decimal string.
    #[serde(rename = "intValue")]
    Int(String),
    #[serde(rename = "boolValue")]
    Bool(bool),
}

fn string(key: impl Into<String>, value: impl Into<String>) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: AnyValue::String(value.into()),
    }
}

fn int(key: &str, value: u64) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: AnyValue::Int(value.to_string()),
    }
}

/// The conversion state.
struct Exporter<'a> {
    entries: &'a [Entry],
    spans: &'a Spans,
    options: Options,
    base: u64,
    /// The trace ids' first half, from the capture.
    seed: u64,
    /// Each span's trace id, once known; a parent comes before its children.
    traces: Vec<u64>,
    next_trace: u64,
}

impl Exporter<'_> {
    fn ns(&self, index: usize) -> String {
        let ticks = self.entries[index].timestamp.saturating_sub(self.base);
        // `as` saturates, and a TickRate keeps the quotient finite.
        let since = (ticks as f64 / self.options.rate.get() * 1000.0).round() as u64;
        self.options.start_unix_ns.saturating_add(since).to_string()
    }

    fn trace_of(&mut self, span: &Span) -> u64 {
        match span.parent {
            Some(parent) => self.traces[parent],
            None => {
                self.next_trace += 1;
                self.next_trace
            }
        }
    }

    fn span(&mut self, index: usize) -> OtlpSpan {
        let span = &self.spans.spans[index];
        let trace = self.trace_of(span);
        self.traces.push(trace);
        let start = &self.entries[span.start];
        // An unfinished span ends where its context was last seen, not at whatever another
        // CPU recorded last.
        let end = span
            .end
            .or_else(|| self.spans.last_seen(span.context))
            .unwrap_or(span.start)
            .max(span.start);
        let (name, kind) = match span.kind {
            SpanKind::Syscall { nr } => (format!("syscall {nr:#x}"), KIND_INTERNAL),
            SpanKind::Request { scheme, opcode, .. } => (
                format!("scheme {scheme} {}", opcode_name(opcode)),
                KIND_CLIENT,
            ),
            SpanKind::Instant => (start.event.name().to_owned(), KIND_INTERNAL),
        };
        let mut attributes = context_attributes(span.context);
        attributes.push(int("cpu.logical_number", u64::from(start.cpu)));
        attributes.extend(record_attributes(start, true));
        if let Some(end) = span.end.filter(|&e| e != span.start) {
            let end = &self.entries[end];
            if end.cpu != start.cpu {
                attributes.push(int("ferric.end_cpu", u64::from(end.cpu)));
            }
            attributes.extend(record_attributes(end, true));
        }
        if span.end.is_none() {
            attributes.push(KeyValue {
                key: "ferric.unfinished".to_owned(),
                value: AnyValue::Bool(true),
            });
        }
        OtlpSpan {
            trace_id: format!("{:016x}{trace:016x}", self.seed),
            span_id: span_id(index),
            parent_span_id: span.parent.map(span_id),
            name,
            kind,
            start_time_unix_nano: self.ns(span.start),
            end_time_unix_nano: self.ns(end),
            attributes,
            events: span.events.iter().map(|&e| self.event(e)).collect(),
            status: span.end.and_then(|e| status(&self.entries[e])),
        }
    }

    fn event(&self, index: usize) -> SpanEvent {
        let entry = &self.entries[index];
        let mut attributes = vec![int("cpu.logical_number", u64::from(entry.cpu))];
        attributes.extend(record_attributes(entry, false));
        SpanEvent {
            time_unix_nano: self.ns(index),
            name: entry.event.name(),
            attributes,
        }
    }
}

/// Span ids are the span's index plus one, since an all-zero id is invalid.
fn span_id(index: usize) -> String {
    format!("{:016x}", index as u64 + 1)
}

fn context_attributes(context: Context) -> Vec<KeyValue> {
    match context {
        Context::Known { id, .. } => vec![int("thread.id", id)],
        Context::Unknown { .. } => Vec::new(),
    }
}

/// `entry`'s arguments as attributes, prefixed with the event's name when `prefixed`.
fn record_attributes(entry: &Entry, prefixed: bool) -> impl Iterator<Item = KeyValue> + '_ {
    entry
        .event
        .args()
        .iter()
        .zip(&entry.args)
        .map(move |(name, value)| {
            let key = if prefixed {
                format!("{}.{name}", entry.event.name())
            } else {
                (*name).to_owned()
            };
            string(key, format!("{value:#x}"))
        })
}

/// The status a closing record gives its span: `ERROR` when its result is an error number.
fn status(end: &Entry) -> Option<Status> {
    let ret = if end.event.id() == SYSCALL_EXIT.id() {
        end.arg(1)
    } else if end.event.id() == SCHEME_RESPONSE.id() {
        end.arg(2)
    } else {
        return None;
    };
    let errno = Errno::demux(usize::try_from(ret).ok()?).err()?;
    Some(Status {
        code: STATUS_ERROR,
        message: errno.text(),
    })
}

/// The resource of the spans of `pid` (`None`: contexts the capture never named).
fn resource(pid: Option<u64>) -> Resource {
    let mut attributes = vec![string("service.namespace", SERVICE_NAMESPACE)];
    match pid {
        Some(pid) => {
            attributes.push(string("service.name", format!("pid {pid}")));
            attributes.push(int("process.pid", pid));
        }
        None => attributes.push(string("service.name", "context unknown")),
    }
    Resource { attributes }
}

/// The trace ids' seed: a hash of the start time and every record (CPU, event, timestamp and
/// arguments), so captures that differ in any record get different ids.
fn seed(entries: &[Entry], start_unix_ns: u64) -> u64 {
    let mut words = vec![start_unix_ns, entries.len() as u64];
    for entry in entries {
        words.extend([
            u64::from(entry.cpu),
            u64::from(entry.event.id().get()),
            entry.timestamp,
        ]);
        words.extend(&entry.args);
    }
    fnv1a(&words)
}

/// FNV-1a over `words`: a stable hash that needs no dependency.
fn fnv1a(words: &[u64]) -> u64 {
    words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// The OTLP/JSON traces document for `capture`.
///
/// # Examples
///
/// ```
/// use ferric_trace::kernel::{SYSCALL_ENTER, SYSCALL_EXIT};
/// use ferric_trace_cli::capture::{Capture, CpuCapture};
/// use ferric_trace_cli::otlp::{Options, to_json};
/// use ferric_trace_cli::perfetto::TickRate;
///
/// let mut bytes = Vec::new();
/// for (event, ts, args) in [(&SYSCALL_ENTER, 10, &[3u64, 0, 0, 0, 0][..]), (&SYSCALL_EXIT, 20, &[3, 0])] {
///     let mut words = [0u64; ferric_trace::record::MAX_RECORD_WORDS];
///     let n = ferric_trace::record::encode(&mut words, event.id(), ts, args).expect("fits");
///     bytes.extend(words[..n].iter().flat_map(|w| w.to_le_bytes()));
/// }
/// let capture = Capture::from_cpus(vec![CpuCapture::decode(0, &bytes).expect("valid")]);
/// let rate = TickRate::new(1000.0).expect("a valid rate");
/// let json = to_json(&capture, Options { rate, start_unix_ns: 1_000_000_000 });
/// assert!(json.contains("\"endTimeUnixNano\":\"1000000010\""));
/// ```
pub fn to_json(capture: &Capture, options: Options) -> String {
    let entries = capture.entries();
    let spans = Spans::build(capture);
    let first = entries.first().map_or(0, |entry| entry.timestamp);
    let mut exporter = Exporter {
        entries,
        spans: &spans,
        options,
        base: first,
        seed: seed(entries, options.start_unix_ns),
        traces: Vec::with_capacity(spans.spans.len()),
        next_trace: 0,
    };
    let mut by_pid: BTreeMap<Option<u64>, Vec<OtlpSpan>> = BTreeMap::new();
    for (index, span) in spans.spans.iter().enumerate() {
        let pid = match span.context {
            Context::Known { pid, .. } => Some(pid),
            Context::Unknown { .. } => None,
        };
        by_pid.entry(pid).or_default().push(exporter.span(index));
    }
    let data = TracesData {
        resource_spans: by_pid
            .into_iter()
            .map(|(pid, spans)| ResourceSpans {
                resource: resource(pid),
                scope_spans: [ScopeSpans {
                    scope: Scope {
                        name: SCOPE_NAME,
                        version: env!("CARGO_PKG_VERSION"),
                    },
                    spans,
                }],
            })
            .collect(),
    };
    serde_json::to_string(&data).expect("the document has only strings, integers and booleans")
}

#[cfg(test)]
mod tests {
    use ferric_trace::kernel::{SCHED_SWITCH, SCHEME_CANCEL, SCHEME_REQUEST, SYSCALL_ENTER};
    use serde_json::Value;

    use super::*;
    use crate::capture::{CpuCapture, tests::bytes};

    type Records<'a> = &'a [(&'a ferric_trace::Event, u64, &'a [u64])];

    fn export(records: Records<'_>, start_unix_ns: u64) -> Value {
        export_cpus(&[records], start_unix_ns)
    }

    fn export_cpus(cpus: &[Records<'_>], start_unix_ns: u64) -> Value {
        let capture = Capture::from_cpus(
            cpus.iter()
                .enumerate()
                .map(|(cpu, records)| {
                    CpuCapture::decode(cpu as u32, &bytes(records)).expect("valid")
                })
                .collect(),
        );
        let rate = TickRate::new(1000.0).expect("a valid rate");
        let json = to_json(
            &capture,
            Options {
                rate,
                start_unix_ns,
            },
        );
        serde_json::from_str(&json).expect("valid JSON")
    }

    fn attribute<'a>(attributes: &'a Value, key: &str) -> Option<&'a Value> {
        attributes
            .as_array()?
            .iter()
            .find(|kv| kv["key"] == key)
            .map(|kv| &kv["value"])
    }

    /// A write (opcode 6) on scheme 2 made by a system call of context 7 of pid 3.
    const CALL: &[(&ferric_trace::Event, u64, &[u64])] = &[
        (&SCHED_SWITCH, 1000, &[0, 3, 1, 7]),
        (&SYSCALL_ENTER, 2000, &[0x21, 9, 0, 16, 0]),
        (&SCHEME_REQUEST, 3000, &[2, 5, 6, 3, 1, 16]),
        (&SCHEME_CANCEL, 3500, &[2, 5]),
        (&SCHEME_RESPONSE, 4000, &[2, 5, 16, 12]),
        // EBADF (9), muxed as `-9`.
        (&SYSCALL_EXIT, 5000, &[0x21, 9u64.wrapping_neg()]),
    ];

    #[test]
    fn a_request_is_a_child_span_of_its_syscall() {
        let doc = export(CALL, 1_700_000_000_000_000_000);
        // The first switch puts out context 1 of pid 0: an instant of that resource.
        let [_, resource] = doc["resourceSpans"]
            .as_array()
            .expect("an array")
            .as_slice()
        else {
            panic!("two resources: {doc}");
        };
        let res = &resource["resource"]["attributes"];
        assert_eq!(
            attribute(res, "service.name").map(|v| &v["stringValue"]),
            Some(&Value::from("pid 3"))
        );
        assert_eq!(
            attribute(res, "process.pid").map(|v| &v["intValue"]),
            Some(&Value::from("3"))
        );
        let scope = &resource["scopeSpans"][0];
        assert_eq!(scope["scope"]["name"], SCOPE_NAME);
        let [call, request] = scope["spans"].as_array().expect("an array").as_slice() else {
            panic!("two spans: {scope}");
        };
        assert_eq!(call["name"], "syscall 0x21");
        assert_eq!(call["kind"], KIND_INTERNAL);
        // 1000 ticks per microsecond: the call starts 1 µs after the first record.
        assert_eq!(call["startTimeUnixNano"], "1700000000000001000");
        assert_eq!(call["endTimeUnixNano"], "1700000000000004000");
        assert_eq!(call["status"]["code"], STATUS_ERROR);
        assert_eq!(
            attribute(&call["attributes"], "thread.id").map(|v| &v["intValue"]),
            Some(&Value::from("7"))
        );
        assert_eq!(
            attribute(&call["attributes"], "syscall.exit.ret").map(|v| &v["stringValue"]),
            Some(&Value::from("0xfffffffffffffff7"))
        );
        assert!(call.get("parentSpanId").is_none());

        assert_eq!(request["name"], "scheme 2 Write");
        assert_eq!(request["kind"], KIND_CLIENT);
        assert_eq!(request["traceId"], call["traceId"]);
        assert_eq!(request["parentSpanId"], call["spanId"]);
        assert_ne!(request["spanId"], call["spanId"]);
        assert!(
            request.get("status").is_none(),
            "16 bytes written is no error"
        );
        let [cancel] = request["events"].as_array().expect("an array").as_slice() else {
            panic!("one event: {request}");
        };
        assert_eq!(cancel["name"], "scheme.cancel");
        assert_eq!(cancel["timeUnixNano"], "1700000000000002500");
        assert_eq!(
            attribute(&cancel["attributes"], "tag").map(|v| &v["stringValue"]),
            Some(&Value::from("0x5"))
        );
        for span in [call, request] {
            let trace = span["traceId"].as_str().expect("a string");
            assert_eq!(trace.len(), 32);
            assert_eq!(span["spanId"].as_str().map(str::len), Some(16));
        }
    }

    #[test]
    fn roots_start_their_own_traces() {
        let doc = export(
            &[
                (&SYSCALL_ENTER, 100, &[1, 0, 0, 0, 0]),
                (&SYSCALL_EXIT, 200, &[1, 0]),
                (&SYSCALL_ENTER, 300, &[2, 0, 0, 0, 0]),
            ],
            0,
        );
        let [resource] = doc["resourceSpans"]
            .as_array()
            .expect("an array")
            .as_slice()
        else {
            panic!("one resource: {doc}");
        };
        assert_eq!(
            attribute(&resource["resource"]["attributes"], "service.name")
                .map(|v| &v["stringValue"]),
            Some(&Value::from("context unknown"))
        );
        let spans = resource["scopeSpans"][0]["spans"]
            .as_array()
            .expect("an array");
        assert_eq!(spans.len(), 2);
        assert_ne!(spans[0]["traceId"], spans[1]["traceId"]);
        // Times count from the first record when no start is given.
        assert_eq!(spans[0]["startTimeUnixNano"], "0");
        // The second call never returned: it ends at the last record, marked unfinished.
        assert_eq!(spans[1]["endTimeUnixNano"], "200");
        assert_eq!(
            attribute(&spans[1]["attributes"], "ferric.unfinished"),
            Some(&serde_json::json!({ "boolValue": true }))
        );
    }

    #[test]
    fn captures_get_different_trace_ids() {
        let a = export(CALL, 0);
        let b = export(CALL, 1);
        let id =
            |doc: &Value| doc["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["traceId"].clone();
        assert_ne!(id(&a), id(&b));
        assert_eq!(id(&a), id(&export(CALL, 0)), "the export is deterministic");
    }

    #[test]
    fn records_decide_the_trace_ids() {
        let id =
            |doc: &Value| doc["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["traceId"].clone();
        let a = export(
            &[
                (&SYSCALL_ENTER, 100, &[4, 0, 0, 0, 0]),
                (&SYSCALL_EXIT, 200, &[4, 0]),
            ],
            0,
        );
        let b = export(
            &[
                (&SYSCALL_ENTER, 100, &[5, 0, 0, 0, 0]),
                (&SYSCALL_EXIT, 200, &[5, 0]),
            ],
            0,
        );
        assert_ne!(id(&a), id(&b));
    }

    #[test]
    fn an_unfinished_span_ends_where_its_context_was_last_seen() {
        let doc = export_cpus(
            &[
                &[
                    (&SCHED_SWITCH, 0, &[0, 3, 1, 7]),
                    (&SYSCALL_ENTER, 10, &[4, 0, 0, 0, 0]),
                    (&SCHED_SWITCH, 20, &[3, 0, 7, 1]),
                ],
                // Another context keeps CPU 1 busy long after.
                &[
                    (&SCHED_SWITCH, 5, &[0, 4, 2, 8]),
                    (&SYSCALL_ENTER, 1_000_000, &[1, 0, 0, 0, 0]),
                ],
            ],
            0,
        );
        let call = doc["resourceSpans"]
            .as_array()
            .expect("an array")
            .iter()
            .flat_map(|r| r["scopeSpans"][0]["spans"].as_array().expect("an array"))
            .find(|s| s["name"] == "syscall 0x4")
            .expect("the call");
        // 1000 ticks per microsecond: tick 20, the switch that put context 7 out.
        assert_eq!(call["endTimeUnixNano"], "20");
        assert_eq!(
            attribute(&call["attributes"], "ferric.unfinished"),
            Some(&serde_json::json!({ "boolValue": true }))
        );
    }

    #[test]
    fn a_switch_only_capture_is_not_empty() {
        let doc = export(
            &[
                (&SCHED_SWITCH, 0, &[0, 3, 1, 7]),
                (&SCHED_SWITCH, 10, &[3, 0, 7, 1]),
            ],
            0,
        );
        let names: Vec<_> = doc["resourceSpans"]
            .as_array()
            .expect("an array")
            .iter()
            .flat_map(|r| r["scopeSpans"][0]["spans"].as_array().expect("an array"))
            .map(|s| s["name"].clone())
            .collect();
        assert_eq!(names, ["sched.switch", "sched.switch"]);
    }

    #[test]
    fn an_empty_capture_has_no_resources() {
        let doc = export(&[], 0);
        assert_eq!(doc["resourceSpans"], serde_json::json!([]));
    }
}
