# Kernel async executor: design

Issue [#178] defines the kernel async runtime: futures run inside bounded,
appropriately scheduled kernel workers, on top of the tickless scheduler
and its shared deadline service ([#186], [#187]), with a separate admitted
real-time class ([#189], [#190]). This page records the architecture
decisions, the reuse audit, the task state machine and the invariants that
host tests must hold. It describes planned work: nothing here is running in
the kernel yet, and no performance claim is made. The host-testable core
([#180], `crates/exec-core`) implements the state machine beside this page.

## Scope and goals

Today, each ring starts a kernel context and runs its chains sequentially
through blocking kernel operations; a scheme call or wait condition suspends
the context. The async design replaces waiting one-context-per-ring with a
bounded pool of kernel workers that poll futures. An executor worker waits
only with an explicit sleep handshake, never by poll loop; the scheduler
wakes it when a completion, timer or remote wake arrives. Goals, from [#177]:

- Fewer per-ring worker stacks and less scheduling jitter.
- A shared, measured deadline service (no per-executor heartbeat).
- Explicit CPU-budget and latency contracts for supported scheme chains.
- Best-effort EEVDF fairness and a separate admitted real-time class, never
  mixed: EEVDF virtual deadlines express fairness; only reservation-owned
  deadlines are real-time contracts ([#189]).

What this design does **not** do: no new public ABI, no change to the ring
FIFO/linked-chain contract, no default-on behavior change, no task
migration or work stealing (initially), no per-executor polling to discover
work, no hard real-time claim without a schedulability argument.

## Reuse audit

| In-tree code | What it offers | Verdict |
| --- | --- | --- |
| `userspace/base/drivers/executor` | Task spawning, ready queues, completion futures, event handling, `Hardware` trait for NVMe SQ/CQ | **Do not import.** It is `async`/await + `std::future::Future`-based, tied to user-space events, thread-local `LocalExecutor`, intrusive linked lists and driver hardware abstractions. Reuse its *ideas*: waker-vtable pattern, local ready queues with cross-thread notification, join/cancel handles. |
| `userspace/base/drivers/storage/nvmed/percore.rs` | Per-CPU workers on one executor | Model per-CPU worker ownership after this; the kernel version is pinned workers, not thread-locals. |
| `crates/kernel/src/scheme/ring/worker.rs` | Resolved file descriptions, caller credentials/address space, CQ reservation, cancel on close | Consumed as-is by the ring pilot ([#183]); the async backend reuses its request preparation, not its blocking waits. |
| `crates/kernel/src/scheme/user.rs`, `WaitCondition` | Blocking scheme calls and waits | The future path adds a separate waiter representation ([#182]); blocking callers keep the legacy path and its direct-switch optimizations. |
| `crates/kernel/src/context/switch.rs` | EEVDF scheduling and wakeup infrastructure | Scheduler stays EEVDF; the extraction of accounting into a host-testable module is [#187], beside `exec-core` in its own module. |
| `crates/ring`, `crates/iobench`, `crates/blkqueue` (A/B), `recipes/tests/ferric-ring-test`, `tools/ferric-trace` | Ring client, fio-style histograms, paired A/B, ring tests, trace tooling | Reused unchanged by the benchmarks ([#179], [#185]) and the pilot. |

Decision: one new small MIT crate, `crates/exec-core`, `no_std` with an
optional `alloc` feature, safe Rust only, host-tested, with the kernel's
futures and timer registration bound to it by [#181]. Scheduler policy
accounting (EEVDF) lives in a separate host-testable module from [#187];
`exec-core` does not know about weights or reservations. No dependency is
added for the core itself; allocatable containers come from `alloc` when the
`alloc` feature is on.

## Task identity and state machine

A *task* is one unit of async work on an executor: a pollable head returning
a step. It is not a kernel context and has no stack.

- **Identity.** `TaskId = index + generation`. Storage is a fixed pool; an
  index is reused with an incremented generation, so stale ids (a Waker held
  past completion, a late response) fail harmlessly on the new occupant.
- **States.**

```text
            spawn
Pending ──────────► Ready ──► Running ◄──► Ready     (yield, or wake while running)
   │                  │            │
   │ cancel           │ cancel     │ WaitDeadline ──► Sleeping(deadline) ──► Ready (clock)
   ▼                  ▼            ▼                │
Cancelled ◄───────────┴────────────┴────────────────┘
Running ──► Completed ──► (reaped: id + generation released)      Wait ──► Ready (wake)
```

  Every transition happens under the executor's own lock except
  `wake`/`next_deadline-triggered` moves, which only set bits and push into
  the bounded wake channel described below.
- **Wake coalescing.** A task is enqueued at most once. A `wake` on an
  already-queued or running task only sets its wake bit; the running task
  sees the bit at its next check and, if it had decided to wait, stays
  queued/ready instead of sleeping. A wake after `Completed` or on a stale
  generation is dropped. A wake *before* the task reaches `Wait` is not lost:
  the bit is set, and the task's first wait immediately re-reads it.
- **Cancellation.** `cancel(id)` is a request, not a join termination:
  - `Pending`/`Ready`: the task moves to `Cancelled` and never runs.
  - `Running`: sets the task's cancel bit; the task's poll head observes it
    at its next check and returns a cancellation result. A single unbounded
    poll cannot be interrupted from inside the core.
  - `Waiting`/`Sleeping`: moves to `Cancelled` (races with `wake` resolve by
    whichever transition runs first; both settle to exactly one outcome).
  - Dropping a future does **not** cancel external I/O. The core's
    `drop(id)` only releases the join interest; the operation's external
    resources are owned by the caller's cancellation protocol until the
    scheme/device acknowledges ([#182]). The core keeps the task's storage
    and result until ack-driven cleanup.
- **Exactly-once results.** A task completes exactly once. `join` returns
  the result when reaped; the completion is stored until the joiner takes
  it, then the slot is released (generation bump).

## Ready queues, admission and polls

- **Per-executor ready queue**, FIFO, owned by the executor. Remote (and
  IRQ-context) wakeups do not touch the queue: they set a bit on the task's
  pinned slot and push its index into the executor's bounded wake channel
  (a fixed preallocated ring, capacity = pool size; an index is pushed only
  on a 0→1 bit transition, so the ring cannot overflow). The executor drains
  the channel while polling, clears bits and enqueues the woken tasks. The
  channel and bits use only atomic operations: an IRQ-safe wake path never
  allocates, locks the executor or runs user/destructor code ([#180]).
- **Admission.** `spawn` is fallible. Live tasks are bounded by pool
  capacity; queued work by the pool's free half (documented in the crate).
  `Admission` errors are typed; a full pool or full queue never panics and
  never silently drops a wake.
- **Bounded polls.** `poll(budget)` runs at most `budget` ready tasks and
  returns; between tasks it drains the wake channel and dues sleepers. A
  batch budget does not interrupt a single unbounded poll: a task that
  never yields runs on.
- **Fairness.** Tasks re-enqueue at the tail on yield; the wake channel is
  drained before each batch so a waiting task woken during the batch runs
  in a later batch, not immediately (bounded poll work per batch).

## Sleep and wake handshake

A task that has nothing to do returns `Wait` or `WaitDeadline`. Before it
goes to sleep the executor re-checks the wake bit once more (the
*sleep/readiness handshake*): a wake that arrived between the task's check
and the sleep state change is visible to the sleep transition and the task
stays queued. The same handshake covers timer expiry: `WaitDeadline` tasks
are kept in a deadline heap, the executor exposes `next_deadline()` for the
shared rearm protocol ([#187]), and due tasks move to `Ready` when the
executor is entered with an advanced clock — the core never polls a clock
by itself, so there is no periodic polling and no per-executor heartbeat.
Timer registration/cancellation participates in the shared rearm protocol
through the bound deadline service in [#181]; the core's `Clock` is a trait
so host tests use a fake clock and the kernel later passes the validated
monotonic clock from [#186].

## Worker model

- Initially one kernel worker per executor, then pinned per-CPU workers
  ([#181]), scheduled as ordinary EEVDF contexts. Worker count is bounded
  by configuration and online CPUs, never by ring count.
- A worker polls its executor's tasks; when the executor is empty it sleeps
  with the handshake above. Local completion, timer expiry and remote
  wakeups all take the same wake path, so they cannot arrive between the
  empty-queue check and sleep and strand work.
- IRQ paths only ever call the wake channel and bits; they never allocate,
  free task storage or run destructors. Task execution and cleanup happens
  on the worker.
- Latency limits: until [#188] enables safe kernel-worker preemption, the
  kernel cannot interrupt an executor's single unbounded poll with a timer;
  a limited cooperative demonstration (bounded polls, documented latency)
  may precede it. A budget that must hold regardless of a poll's length is
  a [#188]/[#190] deliverable, not an executor check between polls.

## Request-scoped ownership

Each future's request carries its own identity and resources: caller pid /
credentials, address-space access, resolved descriptors, buffer mappings and
borrowed syscall head/tail storage are scoped to the *request*, not to the
worker context, so suspend/resume never exposes one caller's identity or
buffers to another task. Preparation that can page-fault or block is not
done inside a poll; it is done before submission or on a bounded fallback
path ([#182]), and unsupported operations stay on the legacy blocking path.
Every accepted operation reaches exactly one terminal state; teardown has an
explicit owner even when a future is dropped.

## Feature catalog and coexistence

- New catalog option `kernel.async` (Cargo feature `kernel_async`),
  **default off**; without a `ferric.toml` the kernel behaves exactly as
  upstream. The pilot ring backend ([#183]) gets its own catalog option
  defaulting to legacy.
- Backend choice is fixed at ring creation. Same-boot A/B of paired
  backends is not assumed safe; the benchmark matrix ([#185]) uses
  interleaved boots where pairing is unsafe, and marks psync/thread numbers
  as a separate baseline.
- Tracing of worker counts, live/queued tasks, polls, local/remote wakes,
  timers and wake-to-poll latency rides the existing `kernel.trace` events
  ([#181]); diagnostics are compiled out when their feature is off.

## Tickless and real-time contract

- Executor tasks consume readiness and deadlines from the shared deadline
  service ([#187]) via the binding in [#181]. An executor never starts work
  on its own and never adds a periodic timer.
- Best-effort futures work without any real-time metadata. Extensible
  per-task scheduling metadata (if needed) is defined once with typed
  authority and ownership; a caller-supplied deadline is *not* authorization
  for real-time execution. RT lanes are separate admitted executors whose
  workers are bound to reservations ([#189], [#190]); a long best-effort
  poll cannot delay an urgent future queued behind it.
- The core accepts a stable executor home/placement descriptor from
  [#191]/[#192] without tying the state machine to Intel labels or firmware
  parsing; an epoch/capacity hint does not authorize migrating a pinned or
  non-`Send` future.

## Failure semantics

All allocation, spawn, admission and clock failures are typed errors. There
is no panic on a full queue or on wake loss (wake loss is prevented). A task
that panics is a kernel bug and is handled by the kernel's panic path, not
by the executor. Resource accounting: worker stacks, task slots, completion
objects, wait queues, temporary mappings and peak memory are counted and
validated to return to baseline after acknowledged teardown ([#183],
[#184]).

## Testing seams

- Deterministic single-threaded tests of every state transition and race
  pairing (wake×poll, sleep×notify, cancel×complete, timer expiry×removal,
  stale generation, duplicate events, admission saturation) with a fake
  clock.
- Threaded stress tests of the wake channel (many wakers, one executor) with
  a seedable harness where feasible; Miri-compatible tests where supported
  (the core is safe Rust).
- On-target: interrupt/timer storms, cross-CPU wake bursts, repeated
  create/close cycles, exactly-once CQEs and resource-baseline checks
  ([#184]).
- Benchmarks: the executor microbenchmarks, idle/compiled-in-overhead and
  waiting-ring scale runs of [#179]/[#185]; microbench speed alone is not an
  adoption gate.

## Deferrals

Explicitly out of the first implementation: task migration and work
stealing, cross-CPU future delegation, global EDF, unused-budget reclaim,
automatic page migration, cluster-wide placement, per-executor heartbeats.

[#177]: https://github.com/rexlunae/ferric-oxide/issues/177
[#178]: https://github.com/rexlunae/ferric-oxide/issues/178
[#179]: https://github.com/rexlunae/ferric-oxide/issues/179
[#180]: https://github.com/rexlunae/ferric-oxide/issues/180
[#181]: https://github.com/rexlunae/ferric-oxide/issues/181
[#182]: https://github.com/rexlunae/ferric-oxide/issues/182
[#183]: https://github.com/rexlunae/ferric-oxide/issues/183
[#184]: https://github.com/rexlunae/ferric-oxide/issues/184
[#185]: https://github.com/rexlunae/ferric-oxide/issues/185
[#186]: https://github.com/rexlunae/ferric-oxide/issues/186
[#187]: https://github.com/rexlunae/ferric-oxide/issues/187
[#188]: https://github.com/rexlunae/ferric-oxide/issues/188
[#189]: https://github.com/rexlunae/ferric-oxide/issues/189
[#190]: https://github.com/rexlunae/ferric-oxide/issues/190
[#191]: https://github.com/rexlunae/ferric-oxide/issues/191
[#192]: https://github.com/rexlunae/ferric-oxide/issues/192