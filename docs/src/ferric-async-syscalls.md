# Batched asynchronous system calls (design)

This chapter designs **batched, asynchronous system calls**: a thread queues
system calls in shared memory without entering the kernel, the kernel services
them whenever it is already running on that thread's behalf, and the results
come back through the same memory. In Rust, each queued call is a `Future`. A
program yields the CPU to the kernel only at an `.await` that its executor
cannot otherwise satisfy.

**Status: planned.** Nothing on this page is implemented beyond what
[Submission/completion rings](ferric-io.md#submissioncompletion-rings)
already describes. The page extends the existing `kernel.ring` scheme and the
planned kernel async runtime ([Kernel async executor](ferric-async.md),
[#182], [#183]); it does not add a second queueing mechanism. Opcode numbers,
header words and verb numbers below are proposals until the
implementing pull request lands them in `crates/ring`.

## Goals and non-goals

Goals:

- **No mandatory transition.** Submitting is a few stores to shared memory.
  It costs no system call, no `syscall` instruction and no context switch.
- **Opportunistic service.** When the thread enters the kernel for any other
  reason (another system call, a timer interrupt, a page fault, a block), the
  kernel may take its queued calls, run the ones that cannot block, and post
  their results, all with no extra transition.
- **Completion without a transfer of control.** A call that has to wait (a
  user-scheme request, a futex, a timeout) completes when its event happens.
  For example, the file system daemon answers, and the completion entry
  appears in the caller's queue while the caller keeps running.
- **`async`/`await` as the control-transfer model.** Awaiting a future never
  enters the kernel while other work is ready. The executor makes the one
  explicit transfer, `Enter` with `min_complete`, only when every task is
  waiting.
- **Same semantics as the synchronous call.** A queued call does what the
  system call does, with the same permission checks, user-memory validation
  and errors. Every scheme works unchanged.
- **Optional and additive.** Behind feature toggles. Without them the
  system behaves exactly as today; the synchronous ABI stays forever.

Non-goals:

- Making every system call asynchronous. Calls that replace the thread or
  process image (`exec`) and the ones that manage the ring itself are refused.
- Linux `io_uring` binary compatibility. A Linux personality (Phase L) may
  translate `io_uring` entries into these operations, but the layouts differ
  (see [Linux `io_uring`](#linux-io_uring)).
- Real-time guarantees. Queued calls are best effort; reservations are
  [#190]'s job.

## What exists today

| Piece | Where | State |
| --- | --- | --- |
| Ring memory, 64-byte SQE, 32-byte CQE, chains, need-wakeup | `crates/ring` (`ferric-ring`) | Implemented, host-tested |
| `kernel.ring` scheme: `Setup`, `Enter`, fixed files, `ASYNC` chains on a worker | `crates/kernel/src/scheme/ring.rs`, `ring/worker.rs` | Implemented, `kernel.rings` |
| Opcodes | `Nop`, `Read`, `Write`, `Fsize`, `Fstat`, `Fsync` | Implemented |
| `SqPoll` and `Umwait` notify modes | encodings only | `EOPNOTSUPP` |
| Nonblocking scheme completion, executor-backed ring | [#182], [#183] | Planned (Phase S5) |
| POSIX `aio_*` in relibc | `crates/relibc/src/header/_aio` | Stubs that panic (`unimplemented!()`) |

Today a ring only progresses inside `Enter` or on the ring's worker, so a
program still makes at least one system call per batch. This design keeps that
path as the guaranteed fallback and adds three things:

1. a wider set of operations,
2. kernel service points that need no `Enter`,
3. a userland runtime that turns entries into futures.

## Architecture

```text
 thread T (user)                                  kernel
 ───────────────                                  ──────
 async fn ... {                         ┌──────────────────────────────┐
   let n = file.read(buf).await; ──┐    │ service points on T:         │
 }                                 │    │  (1) syscall entry           │
                                   ▼    │  (2) return to user after    │
 ferric-ring-rt::Ring ── SQE ──► [ SQ ] ─┤      IRQ / fault             │
   slab of in-flight ops                │  (3) T blocks or Enter       │
   reaps CQ on every poll ◄── CQE ─[ CQ ]◄┤  (4) worker / executor     │
   park(): Enter(min_complete=1)        │  (5) completion callbacks:   │
                                        │      user-scheme response,   │
                                        │      futex wake, timer       │
                                        └──────────────────────────────┘
```

A ring is **bound** to one thread (the `Bind` verb below). Binding is what
lets the kernel find the ring cheaply: it stores a pointer on the thread's
`Context`. An unbound ring keeps today's behaviour exactly, serviced only by
`Enter` and its worker.

## Shared ABI (`ferric-ring` version 2)

`crates/ring` stays the single definition of the format; the kernel, relibc,
bootstrap and the new runtime all depend on it by path. The ring memory layout,
the SQE and CQE sizes and every v1 encoding stay unchanged. Version 2 is purely
additive.

### Header words

The header page has unused words; v2 assigns:

| Word | Name | Written by | Meaning |
| --- | --- | --- | --- |
| 1 | `VERSION` | kernel | `2` on a kernel that implements this design |
| 5 | `FEATURES` | kernel, at setup | Bit set: `SERVICE_ON_ENTRY`, `SERVICE_ON_IRQ_EXIT`, `NONBLOCKING_FORWARD`, `BARRIER`, `TIMEOUT`, ... |
| 6 | `BOUND_TID` | kernel | The bound thread's ID, or 0 |
| 26 | `SERVICED` | kernel | Count of SQEs taken outside `Enter`, for tests and `sys:` views |

The application reads `FEATURES`; it never assumes a service point exists.
All new words follow the v1 rule: one writer per cache line, and the kernel
never trusts a value it reads back from shared memory.

### Verbs

| Verb | Encoding | Effect |
| --- | --- | --- |
| `Bind` (5) | `SYS_CALL`, metadata `[5, flags]` | Bind the ring to the calling thread. `flags`: `ON_ENTRY`, `ON_IRQ_EXIT`, `HANDOFF_ON_BLOCK`. A thread has at most one bound ring (`EBUSY`) |
| `Unbind` (6) | `SYS_CALL`, metadata `[6]` | Remove the binding; queued SQEs stay for `Enter` |
| `Probe` (7) | `SYS_CALL`, metadata `[7]`, payload out | Write a bitmap of supported opcodes and SQE flags |

A v1 kernel refuses unknown verbs with `EINVAL` (`Request::decode` returns
`DecodeError::Verb`), so a v2 library detects an old kernel without guessing
and falls back to `Enter`-only service.

### Opcodes

Opcodes keep the v1 rule: when the operation is a scheme request, the opcode
is the `schemev2::Opcode` number. Kernel-only operations take numbers from
`0x80` up, so the two ranges never collide.

| Opcode | Number | Fields used | Synchronous equivalent |
| --- | --- | --- | --- |
| `Nop`, `Read`, `Write`, `Fsize`, `Fstat`, `Fsync` | v1 | v1 | unchanged |
| `Close` | 3 | `fd` | `close` |
| `Dup` | 4 | `fd`, `addr`/`len` (name) | `dup` (`SYS_DUP_INTO` with a target slot in `ext[0]`) |
| `Fcntl` | 10 | `fd`, `op_flags` = cmd, `offset` = arg | `fcntl` |
| `Fpath` | 13 | `fd`, `addr`, `len` | `fpath` |
| `Ftruncate` | 18 | `fd`, `offset` | `ftruncate` |
| `Getdents` | 26 | `fd`, `addr`, `len`, `offset` | `getdents` |
| `Call` | 28 | `fd`, `addr`/`len` (payload), `op_flags` = `CallFlags`, `ext[0]`/`ext[1]` = metadata ptr/len | `SYS_CALL`. The process manager, `proc:` and most of POSIX go through this |
| `OpenAt` | 29 | `fd` (dir), `addr`/`len` (path), `op_flags` (flags), `ext[0]` (fcntl flags) | `openat`; the result is a descriptor, or a fixed slot with `FIXED_FILE` in `ext[1]` |
| `UnlinkAt` | 32 | `fd`, `addr`/`len`, `op_flags` | `unlinkat` |
| `FutexWake` | `0x80` | `addr`, `len` = count | `futex(FUTEX_WAKE)` |
| `FutexWait` | `0x81` | `addr`, `offset` = expected, `ext[0]` = timeout ns | `futex(FUTEX_WAIT)` |
| `Timeout` | `0x82` | `offset` = deadline ns (`CLOCK_MONOTONIC`), `op_flags` = absolute/relative | `nanosleep`, as a completion |
| `Cancel` | `0x83` | `addr` = target `user_data` | none: cancels a queued or waiting entry |
| `Mprotect` | `0x84` | `addr`, `len`, `op_flags` | `mprotect` |
| `ClockGettime` | `0x85` | `op_flags` = clock, `addr` | `clock_gettime` (mostly for chains) |

Refused, permanently or until designed: `exec`, thread or process creation
and exit, `Bind`/`Enter` on any ring, signal-mask changes, and `RawSyscall`
(below). An unsupported opcode completes with `-EOPNOTSUPP` in its CQE. It is
not a ring error, so one unknown entry does not stop the queue.

`ext[0..3]` are the three v1 `reserved` words, renamed. Every opcode that
does not define them must still find them zero (`EINVAL` otherwise), so they
remain available for later fields.

### Flags

| Flag | Bit | Meaning |
| --- | --- | --- |
| `LINK`, `FIXED_FILE`, `BUF_GRANT`, `ASYNC` | 0–3 | v1, unchanged |
| `BARRIER` | 4 | Do not start this entry until every earlier entry of the ring has completed |
| `INLINE_ONLY` | 5 | Complete with `-EAGAIN` rather than wait or go to a worker. For latency-sensitive polling |
| `NO_CQE_ON_SUCCESS` | 6 | Skip the CQE when the result is non-negative. The CQ room is still reserved, so back-pressure is unchanged |

A new CQE flag `DEFERRED` (bit 1) marks an entry that did not complete at the
point it was taken, which is useful for tracing and tuning.

`ioprio` stays zero until [#190] defines lanes; it never reorders a ring.

### `RawSyscall`: an explicitly unstable escape hatch

Redox treats relibc, not the system call numbers, as the stable interface, and
`SYS_*` numbers have changed upstream. A raw "system call number plus
arguments" opcode would freeze them through the ring. It is therefore **not**
part of the stable ABI. Opcode `0xFF` is reserved for it, accepted only on
rings set up with the `UNSTABLE` setup flag on a kernel built with
`kernel.rings_unstable`, and documented as free to change in any release.

## Kernel changes

### Binding and the cheap check

`Context` gains `sysring: Option<Arc<RingBinding>>` (`cfg(feature = "rings")`).
`RingBinding` holds the ring's `Arc`, the kernel's private SQ head, and a
direct reference to the SQ tail word in the ring's frames, which the kernel
already reaches through its own mapping (`Frames::word`).

The check at a service point is one relaxed load of `SQ_TAIL` compared with the
private head:

- the thread has no binding: one predicted-not-taken branch on a
  `Context` field already in cache;
- the thread is bound and its queue is empty: one cache-line load.

Both must be measured. The design budget for an idle check is under 5 ns, a
small fraction of a system-call round trip; step A4 measures both on the
reference host.

### Service points

Each point is a separate `Bind` flag and `FEATURES` bit, and the first two are
separate catalog options, so each can be measured and turned off on its own.

1. **System call entry** (`ON_ENTRY`): in `syscall::syscall` before dispatch.
   Entries published before the call are taken first, which gives an
   ordering guarantee: *with `SERVICE_ON_ENTRY`, every entry the thread
   published before a synchronous system call is taken before that call
   runs.* That makes mixing async and sync calls on the same file behave as
   program order. Taking is not the same as completing: a deferred entry may
   still finish later, exactly as it would on another thread.
2. **Return to user after an interrupt or fault** (`ON_IRQ_EXIT`): at the
   point where `signal_handler` already runs (after `context::switch::tick`
   and the fault paths), only when the interrupted frame was user mode and
   preemption is enabled. This point runs **inline-safe operations only**
   (below) under a budget, so a timer tick cannot turn into a long
   kernel-mode run.
3. **The thread blocks** (`HANDOFF_ON_BLOCK`): when the bound thread goes to
   sleep in a synchronous call or a page fault, pending entries go to the
   ring's backend, the worker today and the S5 executor once [#183] lands.
   They then progress while the thread is off CPU. Preemption does not hand
   off, because the thread runs again soon and will drain at point 1 or 2.
4. **`Enter`**: unchanged. It is the guaranteed point and the only one that
   can wait.
5. **Completion callbacks**: not a service point for new entries, but the
   place where deferred entries finish (next section).

The kernel runs at most one service pass per context at a time
(`Context::servicing`). An entry such as `Call` runs through the normal
dispatcher, and the guard keeps it from draining the ring again from inside
that dispatch. `Enter` does not run point 1 on its own ring.

**Budgets.** Point 1 takes up to `ENTRY_BUDGET` entries (proposed 64). Point
2 takes up to `IRQ_BUDGET` entries (proposed 16) and stops at a time budget
(proposed 20 µs, read from the continuous clock). Whatever is left waits for
the next point. Budgets are kernel constants first; they become tunables only
if measurement shows the need.

### Inline, forwarded and deferred operations

When the kernel takes an entry, it resolves files and checks the entry exactly
as `Enter` does today (`resolve_files`, one copy of the SQE, `nospec` clamping
of table indices). It then runs the entry in one of three ways:

| Class | Examples | How it runs |
| --- | --- | --- |
| **Inline** | `Nop`, `Fsize`/`Fstat` on kernel schemes, pipe `Read`/`Write` that can make progress now, `FutexWake`, `Mprotect`, `ClockGettime`, `Close` of a kernel-scheme file, `kernel.pcache` hits | Runs to completion in the current context and posts its CQE |
| **Forwarded** | Any request to a **user** scheme (RedoxFS, `proc`, network, drivers) | The kernel queues the scheme request and records a *ring continuation* instead of blocking the context. The CQE is posted when the daemon responds (`UserInner::respond`) |
| **Deferred** | `FutexWait`, `Timeout`, a pipe `Read` with no data, anything not yet converted | Registers a waiter (futex queue, timeout registry, pipe wait queue) whose wake posts the CQE. Operations without a nonblocking path go to the worker or executor backend |

Forwarding is what delivers "completes without a transfer of control".
`UserInner::call_inner` today blocks the caller (`context.block("UserInner::call")`)
until `respond` wakes it. The forwarded path splits that function in two:

- **Submit half:** it allocates the tag, queues the `Sqe` in the scheme's
  `todo`, and records `Waiter::Ring { ring, user_data, files, addr_space }`.
- **Completion half:** `respond` sees a ring waiter, finishes the result
  exactly as the synchronous path would (copy-out, descriptor installation for
  `OpenAt`/`Dup`, grant release), posts the CQE and triggers `EVENT_READ`.
  This runs in the **daemon's** context, so the caller is never scheduled to
  receive its own result.

This is [#182]'s nonblocking scheme completion. The design depends on it, and
the existing blocking path stays for synchronous callers.

Buffers of forwarded entries are user memory of the submitter. They stay valid
because the waiter holds the address space (`AddrSpaceWrapper`), as the
worker already does. The scheme's view of them (grants made by
`call_generic`) is the same as for a synchronous call.

### Ordering, links and barriers

- Entries are **taken** in SQ order. Completions may arrive in any order unless
  the entries are linked.
- `LINK` chains keep v1 rules. A chain runs at the first service point that
  can take it whole. If an entry in it must be forwarded or deferred, the
  rest of the chain continues from that entry's completion callback: the
  continuation runs the next entry. This is how one read-then-write chain
  crosses RedoxFS and a pipe without returning to the application.
- `BARRIER` holds the entry, and everything after it, until the in-flight count
  (`notify::InFlight`) drains. The library uses it before credential or
  descriptor-table changes (below).
- An `ASYNC` chain still goes straight to the backend.

### Cancellation

- `Cancel` (opcode `0x83`) and closing the ring cancel an entry:
  - queued: completes `-ECANCELED`;
  - deferred: removed from its wait queue, then `-ECANCELED`;
  - forwarded: the kernel sends the scheme `Opcode::Cancel` for the tag, as a
    signal does today, and the CQE carries whatever the scheme answers
    (`-ECANCELED` or the real result if it raced).
- Each entry reaches **exactly one terminal CQE**, whatever the race. The
  existing `InFlight` and CQ reservation invariants enforce this, and
  host-side model tests must cover it.
- Dropping a Rust future does not cancel the kernel operation by itself (see
  [Buffer ownership](#buffer-ownership-and-cancellation-safety)).

### Credentials, address space and TOCTOU

The SQE is copied once when it is taken. User memory it names is accessed when
the operation runs, exactly as a synchronous system call accesses it.
Credentials and the descriptor table are those of the bound thread **when the
entry is taken**. For forwarded and deferred entries they are captured with the
waiter, as the worker captures PID and credentials today.

So a `setuid` issued between publishing an entry and its being taken does
change the identity it runs with. The library therefore issues a `BARRIER`,
draining the ring with `Enter`, before relibc changes credentials, `chdir`s or
`close_range`s, and on a v1 kernel it simply drains. This rule is part of the
ABI: *an entry acts as if issued at the moment the kernel takes it.*

### Process and thread lifecycle

- **`fork`.** Bindings are per thread and are not inherited. The ring mapping
  must not be shared with a child, or the child's stores would corrupt the
  parent's queue. Ring mappings get a new grant property, "not inherited":
  the child sees the range unmapped. The ring descriptor is still
  inherited, as any descriptor is, but the child cannot `Bind` it while the
  parent's binding exists (`EBUSY`), and `Enter` from the child is refused
  with `EPERM`, as `kernel.pcache` queues do for other processes.
- **`exec`.** It unbinds. Forwarded and deferred entries keep the old address
  space alive through their waiter until they complete. Their CQEs land in
  a ring nobody maps. If the descriptor was `O_CLOEXEC`, closing the ring
  cancels them.
- **Thread exit.** It unbinds; pending entries go to the backend (as with
  `HANDOFF_ON_BLOCK`). Closing the last descriptor cancels everything, as
  today.
- **Signals.** Service points run before signal delivery on the same return
  path. A signal ends an `Enter` wait with `EINTR`, as today. The userland
  side of a ring is **not async-signal-safe**: a handler on the same thread
  must not submit (`ferric-ring-rt` enforces this with a re-entrancy flag and
  returns `WouldBlock`). relibc handlers keep using synchronous calls, which
  stay correct because point 1 preserves order.

### Accounting, limits and fairness

- Inline work at points 1–3 runs in the bound thread's context, so the
  scheduler charges it to that thread (`sched-core` measured accounting). It
  cannot be used to get kernel time for free.
- Worker and executor time is charged to the submitting process, a gap the
  existing worker already has and [#183] closes.
- Kernel memory per in-flight forwarded or deferred entry (waiter, resolved
  files) is bounded by the CQ reservation, at most `MAX_CQ_ENTRIES` per ring,
  and by a per-process cap on bound rings and in-flight entries, enforced
  with `EAGAIN` at take time. Ring pages count against the process's pinned
  memory, as registered buffers do.
- `IRQ_BUDGET` and the time budget bound how long a tick can keep a CPU in
  kernel mode on the user's behalf.

### Security review points

The ring is attacker-controlled memory read by the kernel at points 1–3, which
are reachable from any user program. The review must cover these:

- Every SQE field is validated from a private copy (already the v1 rule).
  Opcode dispatch uses `nospec` for table lookups; index words read from the
  ring are clamped the same way.
- No lock is held while taking from the SQ at point 2. If the ring lock is
  contended (another `Enter`), the point skips rather than waits.
- A corrupt ring at a service point must not fail the unrelated system call or
  interrupt return that hit it. The ring is marked broken (`EINVAL` on its
  next `Enter`, a tracepoint), and service at that point is skipped.
- Forwarded completions run in the daemon's context. They may touch the
  submitter's address space only through the existing usercopy paths, which
  re-check the mapping.
- `Bind` binds only the caller's own thread. There is no cross-process binding.

### Tracing

New tracepoints in the `ring` group, following [Kernel tracing](ferric-tracing.md):

- `ring.take { ring, user_data, opcode, point }`, where `point` ∈ {enter,
  syscall_entry, irq_exit, block_handoff, worker, executor};
- `ring.defer { ring, user_data, class }`;
- `ring.post { ring, user_data, result, flags }`.

`ferric-trace latency` can then report time from take to post per opcode and
point. The `ferric-trace` tests must check that reading `kernel.trace` through
a ring does not feed back on itself (compare [#388]).

### Feature toggles

| Option | Default | Needs | Gives |
| --- | --- | --- | --- |
| `kernel.rings` | `false` | — | Existing scheme, plus the v2 opcodes, `Probe`, `Bind` (service at `Enter` only) |
| `kernel.rings_on_entry` | `false` | `kernel.rings` | Service point 1 |
| `kernel.rings_on_irq_exit` | `false` | `kernel.rings_on_entry` | Service point 2 |
| `kernel.rings_forward` | `false` | `kernel.rings`, [#182] | Forwarded user-scheme requests; without it they go to the backend |
| `kernel.rings_unstable` | `false` | `kernel.rings` | `RawSyscall`, for experiments only |
| `relibc.ring_aio` | `false` | — | POSIX `aio_*` on rings (falls back to threads without `kernel.rings`) |

Without a `ferric.toml` none is on, and the build behaves like upstream. Each
option goes in `features.toml` with help text when it is implemented, not before.

## Userland

### `ferric-ring-rt`: the Rust runtime (new crate, `crates/ring-rt`)

The crate is `no_std` with `alloc`, `#![deny(missing_docs)]` and
`#![deny(unsafe_op_in_unsafe_fn)]`. It depends on `ferric-ring` for the format
and has a small `sys` module that holds every `unsafe` block: the `mmap` of the
ring, and handing buffer pointers to the kernel. It builds for the host
against a mock kernel, so `cargo test` covers it.

```rust
/// Per-thread ring; !Send because it is bound to its thread.
pub struct Ring { /* Producer<Sqe>, Consumer<Cqe>, ops: Slab<OpState>, features */ }

enum OpState {
    Waiting(Option<Waker>),
    Done(Cqe),
    /// The future was dropped; the ring keeps the buffers until the CQE.
    Orphaned(Box<dyn Any>),
}

impl Ring {
    pub fn current() -> Result<Rc<Ring>, RingError>;      // thread-local, lazily set up and bound
    pub fn submit<O: Operation>(&self, op: O) -> Submit<'_, O>; // no system call
    pub fn reap(&self) -> usize;                          // drain the CQ, wake wakers
    pub fn park(&self, timeout: Option<Duration>) -> Result<(), RingError>; // the one Enter
}
```

The key rules:

- **Submitting never enters the kernel** when the ring is bound with
  `ON_ENTRY` or `ON_IRQ_EXIT`. If the SQ is full, `submit` first reaps;
  if it is still full, the future stays pending with its waker queued for
  SQ space. `submit` never spins on the kernel.
- **Polling a future reaps the CQ first**, so a completion the kernel posted
  opportunistically is seen with no system call, then registers its waker.
- **`park` is the only transfer.** An executor calls it when it has no
  runnable task. It uses the need-wakeup protocol (`notify::prepare_sleep`)
  and `Enter { to_submit: pending, min_complete: 1 }`. On a v1 kernel, or
  when service points are off, `park` also runs before the executor sleeps
  and whenever the SQ holds entries the kernel has not taken: the explicit
  transfer moves to the latest safe moment instead of disappearing.
- **Typed errors.** `RingError` is a `thiserror` enum
  (`Setup`, `Map`, `Unsupported { opcode }`, `Corrupt`, `Reentered`, `Io(Errno)`).
  Operation results are `Result<T, Errno>`.

### Buffer ownership and cancellation safety

A future that borrows a buffer (`read(&mut buf)`) is unsound with a completion
queue. If the future is dropped while the kernel or a daemon still writes into
the buffer, the memory may be reused. The runtime therefore uses **owned
buffers**:

```rust
pub trait IoBuf: Unpin + 'static { fn ptr(&self) -> *const u8; fn len(&self) -> usize; }
pub trait IoBufMut: IoBuf { fn ptr_mut(&mut self) -> *mut u8; unsafe fn set_init(&mut self, n: usize); }

impl File {
    pub fn read<B: IoBufMut>(&self, buf: B, offset: Offset) -> Read<B>; // Output = (Result<usize, Errno>, B)
    pub fn write<B: IoBuf>(&self, buf: B, offset: Offset) -> Write<B>;
}
```

Dropping an incomplete future moves its buffer into `OpState::Orphaned` and,
unless the caller opts out, submits `Cancel`. The buffer is freed when the
terminal CQE arrives. `Vec<u8>`, `Box<[u8]>` and registered-buffer slices
(`crates/regbuf`, `BUF_GRANT` once grants are supported) implement the traits.
A borrowed-buffer API is offered only as `unsafe fn ..._unchecked`, for callers
that can guarantee the future is never dropped early (relibc's `aio_*`, whose
`aiocb` contract already makes the caller keep the buffer alive).

### Executors

- `ferric_ring_rt::block_on(future)` and `LocalExecutor` (single-threaded,
  `spawn_local`): about 300 lines. Run-queue empty → `ring.park(next_timer)`.
  Timers are `Timeout` entries, not a separate timer thread.
- **Integrating other executors.** The ring descriptor reports `EVENT_READ`
  while completions are waiting (already true), so a reactor built on Redox's
  `event:` scheme (mio's Redox backend, `smol`'s `async-io`) can register it
  like a socket. Its readiness callback calls `reap()`. Such reactors get
  correctness at once and the zero-syscall fast path as far as their own park
  logic allows.
- **Multithreaded executors** use one ring per worker thread: rings stay
  single-producer and single-consumer, and the binding stays per thread. A
  future that migrates threads keeps its op slot in its original ring. The
  CQE wakes it through its `Waker`, which is `Send`.

### Synchronous fallback

If `/scheme/kernel.ring` does not exist (upstream Redox, or a kernel without
`kernel.rings`), `Ring::current()` returns a fallback ring. Each submission
runs the synchronous system call at once and completes immediately. The API
and semantics are identical; only batching is lost. Programs written against
`ferric-ring-rt` therefore run unmodified everywhere.

### relibc and redox-rt

- **No change to synchronous wrappers is needed for correctness.** The
  kernel's point 1 keeps program order between queued entries and later
  synchronous calls on the same thread.
- relibc issues the `BARRIER`/drain described under credentials before
  `setuid`/`setgid`/`setgroups`, `chdir`/`fchdir`, `close_range` and `exec`,
  only when the thread has a bound ring (one TLS check).
- **POSIX AIO** (`relibc.ring_aio`) replaces today's panicking `aio_*` stubs:
  - `aio_read`/`aio_write`/`aio_fsync` submit to the calling thread's ring;
  - `aio_suspend` parks;
  - `aio_cancel` submits `Cancel`;
  - `aio_error`/`aio_return` read the op slot;
  - `lio_listio(LIO_WAIT)` submits one batch and parks.

  Without `kernel.rings` they fall back to a helper thread. Replacing the
  `unimplemented!()` stubs with `ENOSYS` is an independent fix that should
  land first.
- **C API**, `<ferric/ring.h>`, written for this project (not derived from
  `liburing`):
  - `ferric_ring_get(void)` returns the thread's ring;
  - `ferric_ring_prep_{read,write,call,openat,close,timeout,futex_wait,...}`
    fill an SQE;
  - `ferric_ring_submit` (no system call when bound);
  - `ferric_ring_peek_cqe`, `ferric_ring_wait_cqe` (parks) and
    `ferric_ring_cqe_seen`.

  `struct ferric_ring` is opaque. The functions get versioned symbols, and the
  SQE/CQE structs are generated from `ferric-ring` by cbindgen, so there is
  one definition.
- **redox-rt** does not use rings internally at first. Its process manager
  calls (`Call` on the `proc` descriptor) become queueable once forwarding
  exists, which is what `posix_spawn`-heavy workloads will benefit from.

## API stability

There are three stability surfaces, each with its own policy.

| Surface | Who depends on it | Policy |
| --- | --- | --- |
| Ring ABI: memory layout, SQE/CQE encoding, opcodes, flags, verbs, header words | Kernel ↔ every ring user, including statically linked binaries | **Stable once released, additive only.** |
| `ferric-ring-rt` Rust API | Rust programs | **SemVer.** |
| `<ferric/ring.h>` and POSIX AIO | C programs via relibc | **Stable C ABI** with versioned symbols, as the rest of relibc. |

**Ring ABI rules:**

- Numbers are never reused or renumbered.
- `VERSION` only grows.
- Unknown opcodes complete with `-EOPNOTSUPP`; unknown flag bits and non-zero
  reserved fields are `EINVAL` per entry, and unknown verbs are `EINVAL` per
  call. So old kernels refuse new requests cleanly, and new kernels keep
  serving old binaries.
- A larger entry, if ever needed, is a setup flag (`SQE128`), never a change
  to the 64-byte entry.

**`ferric-ring-rt` rules:**

- `Operation` and `OpState` stay sealed.
- Public enums are `#[non_exhaustive]`.
- Buffer traits only grow.

**Other implications:**

- **The ring ABI becomes more stable than the system call numbers.** Redox
  makes no promise about `SYS_*` numbers; relibc absorbs changes. Ring opcodes
  are typed and named by operation, so the kernel can renumber or restructure
  system calls freely: only the opcode-to-operation mapping in
  `kernel/src/scheme/ring.rs` changes. This is why `RawSyscall` is outside
  the stable set, and why each new opcode needs review as a permanent
  commitment.
- **Behaviour versus timing.** Turning a service point on or off changes only
  *when* an entry is taken, never *what* it does, with one documented
  exception: the point-1 ordering guarantee. Programs that rely on it check
  `FEATURES & SERVICE_ON_ENTRY`; `ferric-ring-rt` exposes it as
  `Ring::orders_with_sync_calls()`. Without it, the runtime drains before
  synchronous calls it makes itself, and C users must call
  `ferric_ring_submit_and_wait` first.
- **Graduation.** A new opcode starts behind `kernel.rings_unstable` with a
  number in `0xE0`–`0xFE` that may change. After a release with tests and a
  consumer, it gets its permanent number. The unstable number stays refused
  rather than reused for one further release.
- **Deprecation.** A stable opcode can be deprecated: documented, with a
  tracepoint counting uses. It is removed only together with a `VERSION` bump
  that the library checks, and never within one major release of the system.
- **Upstream Redox.** The ring ABI is Ferric-specific. Binaries using it run on
  Redox only through the synchronous fallback. Nothing here changes the
  upstream system call ABI, so Redox binaries keep running on Ferric.

### Linux `io_uring`

Phase L may expose Linux's `io_uring` system calls to Linux binaries. The
semantics here (SQ/CQ, links, `user_data`, need-wakeup) are close enough that a
translation layer can map common `io_uring` opcodes onto these operations. The
implementation must be written from the published `io_uring(7)`,
`io_uring_setup(2)` and `io_uring_enter(2)` manual pages, never from Linux
source, as AGENTS.md requires. Ferric's own layout stays independent: Linux
compatibility is a translation, not a constraint on this ABI.

## Implementation plan

Each step is a reviewable pull request with tests, docs and the status-table
update. Steps 1–3 do not depend on Phase S.

| Step | Delivers | Depends on | Validation |
| --- | --- | --- | --- |
| A0 | This design | — | Book checks |
| A1 | relibc `aio_*` stubs return `ENOSYS` instead of panicking | — | relibc test |
| A2 | `ferric-ring` v2: new opcodes, flags, `ext` words, `Probe`/`Bind` encodings, `FEATURES`; kernel runs the new opcodes through `Enter` and the worker | — | Host tests in `crates/ring`; `ferric-ring-test` cases for every opcode, refusal and errno |
| A3 | `ferric-ring-rt`: futures, owned buffers, orphaning, `LocalExecutor`, fallback ring | A2 | Host tests against a mock kernel built on `ferric_ring::chain` (drop-before-complete, cancel races, SQ-full, re-entrancy); target tests in `ferric-ring-test` |
| A4 | Binding and service point 1 (`kernel.rings_on_entry`), `ring.*` tracepoints | A2 | Ordering test (queued write then sync read); syscall-entry overhead with and without binding (`ferric-trace-bench`); a batch of N `Nop`s completes with zero `Enter` calls |
| A5 | Service point 2 (`kernel.rings_on_irq_exit`) with budgets | A4, S4 audit ([#188]) | Interrupt-latency p99 within 5% of baseline under a saturating submitter; budget enforcement test |
| A6 | Forwarded user-scheme requests and deferred futex/timeout/pipe waiters (`kernel.rings_forward`) | [#182] | RedoxFS read completes while the submitter spins in user mode with no system calls; cancellation races; exactly-once terminal CQE; daemon-exit handling |
| A7 | `HANDOFF_ON_BLOCK` onto the executor backend | [#183] | Waiting-ring workload at 1/16/64/256 rings, as in [#183] |
| A8 | relibc `<ferric/ring.h>`, `relibc.ring_aio`, credential barriers | A3, A4 | POSIX AIO conformance subset; `fork`/`exec`/signal tests |
| A9 | `SqPoll` and `Umwait` notify modes (Phase 2.3) | A4 | Existing plan |

**Benchmarks** go in `ferric-ring-test` and `ferric-iobench`. Each PR reports
before and after numbers, as CONTRIBUTING.md asks:

- `nop` × N batched against N synchronous calls;
- `pread` 4 KiB at QD1 and QD32 (`--ioengine=ring`) against `pread`;
- a `posix_spawn` loop against the queued `proc` calls.

## Open questions

- Should point 2 also run on page-fault return? It is cheap to add, but faults
  are often hot in loops, so measure first.
- Per-process instead of per-thread binding would let any thread's entry
  drain any ring. That gives better latency for thread pools but needs an MPSC
  SQ. It is deferred until per-thread rings are measured.
- `NO_CQE_ON_SUCCESS` with linked chains: is the failing link's CQE enough
  for the library to recover the state of the whole chain?

[#182]: https://github.com/rexlunae/ferric-oxide/issues/182
[#183]: https://github.com/rexlunae/ferric-oxide/issues/183
[#188]: https://github.com/rexlunae/ferric-oxide/issues/188
[#190]: https://github.com/rexlunae/ferric-oxide/issues/190
[#388]: https://github.com/rexlunae/ferric-oxide/pull/388
