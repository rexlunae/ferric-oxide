# Scheduling

The kernel uses measured-time accounting from the host-tested, allocation-free
`crates/sched-core` crate. Runnable selection and architecture switching remain
in `crates/kernel/src/context/switch.rs`.

## The current scheduler: EEVDF

The kernel uses EEVDF, *earliest eligible virtual deadline first*, the same
family of algorithm Linux adopted in 6.6. Each runnable context (thread)
carries:

- a **weight**, taken from its priority through a table of 40 nice levels
  (`Weight::for_priority`, each step about 1.25 times the next);
- a **virtual time**, which advances as the context runs, more slowly for
  heavier weights;
- a **virtual deadline**, its virtual time plus its time slice divided by its
  weight.

The scheduler picks the eligible runnable context with the earliest virtual
deadline. `FairState` charges measured monotonic execution time, including the
last interval before a context blocks. Its remaining slice is real time;
weighted virtual time and deadlines use saturating `u128` arithmetic. Division
residue survives successive observations at the same weight, so short intervals
do not disappear through rounding. A waking context starts no earlier than the
run queue's virtual clock, so sleeping does not build up credit.

Every switch has an explicit reason. Only the `sched_yield` syscall forfeits
unused service; short wake, timer and blocking intervals do not count as voluntary
yields. Direct scheme execution consumes the original client's timeslice, and an
explicit yield on a borrowed slice goes through owner selection. CPU migration
preserves virtual lag and remaining real service.

The requested base slice is **20.25 ms**, independent of interrupt cadence. The
periodic path still calls `switch::tick` and reconsiders scheduling after three
timer interrupts. The x86 PIT interval is about **4.06 ms** (`pit::RATE`), not the
former accounting constant's 2.25 ms. One-shot hardware remains optional and is
not yet the scheduler event source. These intervals are not dispatch bounds:
kernel interrupt masking and preemption remain part of the Phase S audit.

`FairClock` charges each non-idle interval against the runnable population that
preceded incoming wakes and blocked-owner removal. The clock owns a separate
monotonic observation timestamp under the run-queue lock. Local weight changes
and remote steals settle the preceding population first; both queues observe one
migration timestamp before weight moves. This never resets the running owner's
separate elapsed execution charge. Idle queues do not charge virtual service,
even if contended work remains queued. A stolen context contributes
to the destination's active weight even when it is selected immediately, without
being inserted into that CPU's runnable tree. Live priority changes still use
legacy queue-weight bookkeeping; coordinated reweight events remain part of
[#187](https://github.com/rexlunae/ferric-oxide/issues/187).

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
across these switches (the per-CPU context-timer registry in `context/switch.rs`),
so a blocked scheme handler still wakes on time.

### Watching the scheduler

With [kernel tracing](./ferric-tracing.md) enabled, the `sched.switch`,
`sched.context_*` and `sched.futex_*` tracepoints record every switch, context
lifecycle event and futex wait and wake, and `ferric-trace` can turn a capture
into a Perfetto timeline.

## Planned: tickless, asynchronous and real-time scheduling

Issue [#177](https://github.com/rexlunae/ferric-oxide/issues/177) proposes the
remaining migration. Measured accounting is active; the continuous-clock and
one-shot hardware groundwork, ordered time-scheme queue and portable executor
core also exist. Context waits and time-scheme/async timers still lack unified
rearming, and the executor is not bound to kernel workers. The remaining plan
has these parts, each tracked in its own issue:

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
