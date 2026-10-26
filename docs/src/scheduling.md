# Scheduling

This page describes the scheduler on `main` and the changes planned for it.
The code is in `crates/kernel/src/context/switch.rs`. (The module's opening
comment still calls it a round-robin scheduler; that comment predates the
current algorithm.)

## The current scheduler: EEVDF

The kernel uses EEVDF, *earliest eligible virtual deadline first*, the same
family of algorithm Linux adopted in 6.6. Each runnable context (thread)
carries:

- a **weight**, taken from its priority through a table of 40 nice levels
  (`SCHED_PRIO_TO_WEIGHT`, each step about 1.25 times the next);
- a **virtual time**, which advances as the context runs, more slowly for
  heavier weights;
- a **virtual deadline**, its virtual time plus its time slice divided by its
  weight.

The scheduler picks the runnable context with the earliest virtual deadline.
A context that wakes up starts no earlier than the run queue's current virtual
time, so sleeping does not build up credit. A context that gives up the CPU
well before its slice ends is charged for the unused part, so yielding cannot
be used to jump the queue.

The base time slice is 9 ticks (about 20 ms) at the default weight. A timer
interrupt calls `switch::tick` every tick (about 2.25 ms), and the scheduler
runs every third tick (about 6.75 ms) or whenever a context blocks.

### Run queues and CPUs

Each CPU has its own run queue. A context that wakes up goes back to the CPU it
last ran on, or to another CPU its affinity allows. A CPU whose queue is empty
steals up to two contexts from a CPU with at least two more queued than it.
Periodic balancing between busy CPUs exists in the code but is disabled,
because it caused a livelock in WebKit.

### Direct switches

When a process makes a request to a user-space scheme, the kernel can switch
straight to the scheme's daemon, and back to the caller when the daemon
replies, without going through the run queue. Timed waits keep their deadlines
across these switches (`context/timeout.rs`), so a blocked scheme handler
still wakes on time.

### Watching the scheduler

With [kernel tracing](./ferric-tracing.md) enabled, the `sched.switch`,
`sched.context_*` and `sched.futex_*` tracepoints record every switch, context
lifecycle event and futex wait and wake, and `ferric-trace` can turn a capture
into a Perfetto timeline.

## Planned: tickless, asynchronous and real-time scheduling

Issue [#177](https://github.com/rexlunae/ferric-oxide/issues/177) proposes the
next scheduler. None of it is implemented yet. The plan has these parts, each
tracked in its own issue:

- **Tickless operation.** Replace the periodic tick with one-shot timers
  programmed for the next deadline, so an idle CPU stays idle and a busy one is
  interrupted only when a decision is due (#186, #187).
- **An asynchronous kernel core.** A host-testable `no_std` executor, bound
  to the tickless scheduler, so scheme requests and `kernel.ring` operations can
  complete without holding a blocked kernel stack per request (#180 to #183).
- **Real-time reservations.** Bounded kernel preemption, interrupt and lock
  latency first (#188), then admitted per-CPU deadline reservations, so a
  real-time task gets a guaranteed share of a CPU and cannot exceed it (#189),
  carried through async work and scheme IPC (#190).
- **Topology-aware placement.** Placement that knows about SMT siblings,
  NUMA nodes and core classes such as performance and efficiency cores (#191,
  #192).
- **Measurement first.** An architecture document (#178), baseline benchmarks
  (#179), fault-injection tests (#184), and a benchmark-based decision on
  rolling each part out (#185).

[Roadmap](./ferric-roadmap.md) has the wider context.

## Further reading

- [Scheduling algorithms on the OSDev wiki](https://wiki.osdev.org/Scheduling_Algorithms)
- [EEVDF in the Linux kernel documentation](https://docs.kernel.org/scheduler/sched-eevdf.html)
