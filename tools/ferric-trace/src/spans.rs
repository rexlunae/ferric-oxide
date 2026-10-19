//! A capture as spans: the system calls and scheme requests it holds, each on the context that
//! made it, with the records in between attached to them.
//!
//! [`Spans::build`] walks a capture's records in timestamp order and pairs them:
//!
//! - **Contexts.** A record belongs to the context `sched.switch` last put on its CPU
//!   ([`Context::Known`]). Records from before a CPU's first switch belong to that CPU's
//!   [`Context::Unknown`] until the switch names the context it switches out; they then move
//!   to that context, with its open spans, so a call begun there still ends when the context
//!   finishes it on another CPU. These are the rules the Perfetto export
//!   ([`crate::perfetto`]) follows.
//! - **System calls.** `syscall.enter` opens a [`SpanKind::Syscall`] on its context, and the
//!   `syscall.exit` with the same number on the same context closes it, wherever the context
//!   runs by then.
//! - **Scheme requests.** `scheme.request` opens a [`SpanKind::Request`], and the
//!   `scheme.response` with the same scheme id and tag closes it. A `scheme.cancel` does not
//!   close the request (the scheme still answers it): it is one of the span's
//!   [`events`](Span::events).
//! - **Parents.** A span opened while its context has another open span is that span's child:
//!   a scheme request made during a system call is the system call's child.
//! - **Everything else.** Any other record, and an exit or response nothing opened, is an event
//!   of its context's innermost open span, or a [`SpanKind::Instant`] of its own when the
//!   context has none open. `sched.switch` itself only moves contexts and is not kept.
//!
//! A span still open at the end of the capture has no [`end`](Span::end).

use std::collections::HashMap;

use ferric_trace::kernel::{
    SCHED_SWITCH, SCHEME_CANCEL, SCHEME_REQUEST, SCHEME_RESPONSE, SYSCALL_ENTER, SYSCALL_EXIT,
};

use crate::capture::{Capture, Entry};

/// The context a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    /// A context named by `sched.switch`.
    Known {
        /// Its process id.
        pid: u64,
        /// Its context debug id.
        id: u64,
    },
    /// Whatever ran on `cpu` before the CPU's first `sched.switch`, when that switch was not
    /// captured.
    Unknown {
        /// The CPU.
        cpu: u32,
    },
}

/// What a span stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpanKind {
    /// A system call, from `syscall.enter` to `syscall.exit`.
    Syscall {
        /// The system call number.
        nr: u64,
    },
    /// A scheme request, from `scheme.request` to `scheme.response`.
    Request {
        /// The scheme id.
        scheme: u64,
        /// The request's tag.
        tag: u64,
        /// The request's opcode, raw.
        opcode: u64,
    },
    /// A record outside every span: it starts and ends at once.
    Instant,
}

/// One span of a capture. Records are indices into [`Capture::entries`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// What the span is.
    pub kind: SpanKind,
    /// The context it belongs to.
    pub context: Context,
    /// The record that opened it.
    pub start: usize,
    /// The record that closed it: `None` if it was still open at the end of the capture, and
    /// [`start`](Span::start) for an [`SpanKind::Instant`].
    pub end: Option<usize>,
    /// The span it was opened in, an index into [`Spans::spans`].
    pub parent: Option<usize>,
    /// The records that happened during it on its context, and cancels of a request.
    pub events: Vec<usize>,
}

/// The spans of a capture, in the order they open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spans {
    /// The spans; a parent always comes before its children.
    pub spans: Vec<Span>,
}

/// The pairing state.
#[derive(Default)]
struct Builder {
    spans: Vec<Span>,
    /// The context `sched.switch` last put on each CPU.
    running: HashMap<u32, Context>,
    /// Each context's open spans, innermost last.
    open: HashMap<Context, Vec<usize>>,
    /// Open scheme requests by (scheme, tag).
    requests: HashMap<(u64, u64), usize>,
}

impl Spans {
    /// Pairs the records of `capture` into spans.
    pub fn build(capture: &Capture) -> Self {
        let mut builder = Builder::default();
        for (index, entry) in capture.entries().iter().enumerate() {
            builder.entry(index, entry);
        }
        Self {
            spans: builder.spans,
        }
    }
}

impl Builder {
    fn context(&self, entry: &Entry) -> Context {
        self.running
            .get(&entry.cpu)
            .copied()
            .unwrap_or(Context::Unknown { cpu: entry.cpu })
    }

    fn entry(&mut self, index: usize, entry: &Entry) {
        let id = entry.event.id();
        if id == SCHED_SWITCH.id() {
            self.switch(entry);
        } else if id == SYSCALL_ENTER.id() {
            self.open(index, entry, SpanKind::Syscall { nr: entry.arg(0) });
        } else if id == SYSCALL_EXIT.id() {
            self.syscall_exit(index, entry);
        } else if id == SCHEME_REQUEST.id() {
            let kind = SpanKind::Request {
                scheme: entry.arg(0),
                tag: entry.arg(1),
                opcode: entry.arg(2),
            };
            let span = self.open(index, entry, kind);
            // A tag still open was never answered in the capture; its span stays open.
            self.requests.insert((entry.arg(0), entry.arg(1)), span);
        } else if id == SCHEME_RESPONSE.id() {
            self.response(index, entry);
        } else if id == SCHEME_CANCEL.id() {
            match self.requests.get(&(entry.arg(0), entry.arg(1))) {
                Some(&span) => self.spans[span].events.push(index),
                None => self.event(index, entry),
            }
        } else {
            self.event(index, entry);
        }
    }

    fn switch(&mut self, entry: &Entry) {
        if !self.running.contains_key(&entry.cpu) {
            let prev = Context::Known {
                pid: entry.arg(0),
                id: entry.arg(2),
            };
            self.reattribute(entry.cpu, prev);
        }
        let next = Context::Known {
            pid: entry.arg(1),
            id: entry.arg(3),
        };
        self.running.insert(entry.cpu, next);
    }

    /// Moves everything on CPU `cpu`'s unknown context to `context`.
    fn reattribute(&mut self, cpu: u32, context: Context) {
        let unknown = Context::Unknown { cpu };
        for span in &mut self.spans {
            if span.context == unknown {
                span.context = context;
            }
        }
        if let Some(open) = self.open.remove(&unknown) {
            // Spans the context opened elsewhere before it came here are older, so these go
            // inside them.
            self.open.entry(context).or_default().extend(open);
        }
    }

    fn open(&mut self, index: usize, entry: &Entry, kind: SpanKind) -> usize {
        let context = self.context(entry);
        let stack = self.open.entry(context).or_default();
        let span = self.spans.len();
        self.spans.push(Span {
            kind,
            context,
            start: index,
            end: None,
            parent: stack.last().copied(),
            events: Vec::new(),
        });
        stack.push(span);
        span
    }

    fn close(&mut self, span: usize, index: usize) {
        self.spans[span].end = Some(index);
        let context = self.spans[span].context;
        if let Some(stack) = self.open.get_mut(&context) {
            stack.retain(|&s| s != span);
        }
    }

    fn syscall_exit(&mut self, index: usize, entry: &Entry) {
        let context = self.context(entry);
        let innermost = self.open.get(&context).and_then(|stack| {
            stack
                .iter()
                .rev()
                .copied()
                .find(|&s| matches!(self.spans[s].kind, SpanKind::Syscall { .. }))
        });
        match innermost {
            Some(span) if self.spans[span].kind == (SpanKind::Syscall { nr: entry.arg(0) }) => {
                self.close(span, index);
            }
            _ => self.event(index, entry),
        }
    }

    fn response(&mut self, index: usize, entry: &Entry) {
        match self.requests.remove(&(entry.arg(0), entry.arg(1))) {
            Some(span) => self.close(span, index),
            None => self.event(index, entry),
        }
    }

    /// Attaches record `index` to its context's innermost open span, or makes it an instant.
    fn event(&mut self, index: usize, entry: &Entry) {
        let context = self.context(entry);
        match self.open.get(&context).and_then(|stack| stack.last()) {
            Some(&span) => self.spans[span].events.push(index),
            None => self.spans.push(Span {
                kind: SpanKind::Instant,
                context,
                start: index,
                end: Some(index),
                parent: None,
                events: Vec::new(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CpuCapture, tests::bytes};

    fn capture(cpus: &[Vec<u8>]) -> Capture {
        Capture::from_cpus(
            cpus.iter()
                .enumerate()
                .map(|(cpu, b)| {
                    CpuCapture::decode(u32::try_from(cpu).expect("few CPUs"), b).expect("valid")
                })
                .collect(),
        )
    }

    const CTX: Context = Context::Known { pid: 3, id: 7 };

    #[test]
    fn a_syscall_ends_on_the_cpu_its_context_moved_to() {
        let spans = Spans::build(&capture(&[
            bytes(&[
                (&SCHED_SWITCH, 100, &[0, 3, 1, 7]),
                (&SYSCALL_ENTER, 110, &[4, 9, 0, 16, 0]),
                (&SCHED_SWITCH, 120, &[3, 0, 7, 1]),
            ]),
            bytes(&[
                (&SCHED_SWITCH, 130, &[0, 3, 2, 7]),
                (&SYSCALL_EXIT, 140, &[4, 16]),
            ]),
        ]));
        assert_eq!(
            spans.spans,
            [Span {
                kind: SpanKind::Syscall { nr: 4 },
                context: CTX,
                start: 1,
                end: Some(4),
                parent: None,
                events: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_request_in_a_syscall_is_its_child() {
        let spans = Spans::build(&capture(&[bytes(&[
            (&SCHED_SWITCH, 100, &[0, 3, 1, 7]),
            (&SYSCALL_ENTER, 110, &[4, 9, 0, 16, 0]),
            (&SCHEME_REQUEST, 120, &[2, 5, 6, 3, 1, 16]),
            (&SCHEME_CANCEL, 125, &[2, 5]),
            // An exit of another call is an event of the innermost span.
            (&SYSCALL_EXIT, 127, &[9, 0]),
            (&SCHEME_RESPONSE, 130, &[2, 5, 16, 12]),
            (&SYSCALL_EXIT, 140, &[4, 16]),
        ])]));
        let [call, request] = &spans.spans[..] else {
            panic!("two spans: {spans:?}");
        };
        assert_eq!((call.start, call.end, call.parent), (1, Some(6), None));
        assert_eq!(
            request.kind,
            SpanKind::Request {
                scheme: 2,
                tag: 5,
                opcode: 6
            }
        );
        assert_eq!((request.start, request.end), (2, Some(5)));
        assert_eq!(request.parent, Some(0));
        // The cancel and the stray exit both happened while the request was the innermost span.
        assert_eq!(request.events, [3, 4]);
    }

    #[test]
    fn records_before_the_first_switch_move_to_the_outgoing_context() {
        let spans = Spans::build(&capture(&[bytes(&[
            (&SYSCALL_ENTER, 100, &[4, 9, 0, 16, 0]),
            (&SCHED_SWITCH, 120, &[3, 0, 7, 1]),
            (&SCHED_SWITCH, 130, &[0, 3, 1, 7]),
            (&SYSCALL_EXIT, 140, &[4, 16]),
        ])]));
        assert_eq!(spans.spans.len(), 1);
        assert_eq!(spans.spans[0].context, CTX);
        assert_eq!(spans.spans[0].end, Some(3));
    }

    #[test]
    fn unmatched_records_are_instants_or_events() {
        let spans = Spans::build(&capture(&[bytes(&[
            // An exit and a response nothing opened, on an unknown context.
            (&SYSCALL_EXIT, 100, &[5, 0]),
            (&SCHEME_RESPONSE, 110, &[2, 6, 0, 12]),
            // An exit with another number while a call is open is an event of the call.
            (&SYSCALL_ENTER, 120, &[4, 9, 0, 16, 0]),
            (&SYSCALL_EXIT, 130, &[5, 0]),
        ])]));
        let kinds: Vec<_> = spans
            .spans
            .iter()
            .map(|s| (s.kind, s.start, s.end))
            .collect();
        assert_eq!(
            kinds,
            [
                (SpanKind::Instant, 0, Some(0)),
                (SpanKind::Instant, 1, Some(1)),
                (SpanKind::Syscall { nr: 4 }, 2, None),
            ]
        );
        assert_eq!(spans.spans[2].events, [3]);
        assert_eq!(spans.spans[0].context, Context::Unknown { cpu: 0 });
    }

    #[test]
    fn a_reused_tag_opens_a_new_request() {
        let spans = Spans::build(&capture(&[bytes(&[
            (&SCHEME_REQUEST, 100, &[2, 5, 6, 3, 1, 16]),
            (&SCHEME_RESPONSE, 110, &[2, 5, 16, 12]),
            (&SCHEME_REQUEST, 120, &[2, 5, 7, 3, 1, 16]),
        ])]));
        let ends: Vec<_> = spans.spans.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(ends, [(0, Some(1)), (2, None)]);
        assert_eq!(spans.spans[1].parent, None);
    }
}
