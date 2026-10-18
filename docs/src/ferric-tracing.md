# Kernel tracing

`kernel.trace` adds typed kernel tracepoints. Records go to per-CPU rings,
and a kernel scheme turns events on and off and drains the rings. It is off by
default. Without it, no tracepoint is compiled in and the kernel matches the
upstream build. The event catalog, record format, ring and scheme ABI are in
`crates/trace/` (`ferric-trace`, `no_std`, no `unsafe`). The kernel side is in
`crates/kernel/src/trace/`, `crates/kernel/src/arch/x86_64/static_key.rs` and
`crates/kernel/src/scheme/trace.rs`.

## Enabling it

Tracing has three levels of control: what is built, what is on at boot, and
what is on at run time.

| Setting (`ferric.toml`) | Default | Effect |
| --- | --- | --- |
| `kernel.trace` | `false` | Builds the tracing code and the `kernel.trace` scheme (x86_64 only) |
| `kernel.trace_syscall` | `true` | Compiles in `syscall.enter` and `syscall.exit` |
| `kernel.trace_sched` | `true` | Compiles in `sched.switch` |
| `kernel.trace_scheme` | `true` | Compiles in `scheme.request`, `scheme.response` and `scheme.cancel` |
| `kernel.trace_max_level` | `"info"` | Leaves out tracepoints more verbose than this level (`error` < `warn` < `info` < `debug` < `trace`) |
| `kernel.trace_boot` | `""` | Events to turn on at boot, written to the kernel `config.toml` as `trace.boot` |

The last four options take effect only with `kernel.trace`. They map to the
kernel Cargo features `trace_syscall`, `trace_sched`, `trace_scheme` and
`trace_level_*`.
`ferric-config` rejects `kernel.trace` together with `kernel.profiling`, because
the profiler takes a CPU out of the system and that CPU would never answer the
interrupt that live patching waits for.

A selection is a comma- or space-separated list. Each item is `all`, a
subsystem (`syscall`, `sched`, `scheme`) or an event name (`sched.switch`). Besides
`trace.boot`, the boot environment can set `FERRIC_TRACE=<selection>`, which
the bootloader's environment editor can change. Both are applied on the
bootstrap processor before the other CPUs start, and the kernel logs
`trace: <event> on from boot (<source>)` for each event that is on.

```toml
[kernel]
trace = true
trace_boot = "sched"
```

## How a tracepoint works

`trace_event!(EVENT, args...)` marks a tracepoint. If the event's subsystem
or level is not compiled in, the macro expands to nothing and its arguments
are never evaluated. A compiled-in tracepoint is a static key. It is a 5-byte
NOP plus an entry in the `.ferric_trace_keys` section, and turning the event on
rewrites the NOP as a jump to the code that writes the record.

- **At boot**, before the other CPUs run, the bytes are overwritten directly.
- **At run time**, the kernel uses the breakpoint protocol for cross-modifying
  code (Intel SDM vol. 3A). It writes `int3`, then the tail of the
  instruction, then its first byte, and after each step every CPU
  acknowledges a `TraceSync` IPI on vector `0x46`. An `int3` hit on a site
  mid-patch skips the site.

The kernel writes the bytes with the same text-patching routine the CPU
feature alternatives use.

Tracepoints sit only where interrupts are off: in system call handling,
including the kernel's side of scheme requests, and in the context switch. Each CPU's ring therefore has a single producer.

The first events are:

| Event | Arguments |
| --- | --- |
| `syscall.enter` | `nr`, `a`, `b`, `c`, `d` (the system call number and first four arguments) |
| `syscall.exit` | `nr`, `ret` (the result as the kernel returns it: an error is `-errno`) |
| `sched.switch` | `prev_pid`, `next_pid`, `prev_id`, `next_id` |
| `scheme.request` | `scheme`, `tag`, `opcode`, `caller`, `arg0`, `arg2` |
| `scheme.response` | `scheme`, `tag`, `ret`, `responder` |
| `scheme.cancel` | `scheme`, `tag` |

The scheme events cover requests to user-space schemes
(`crates/kernel/src/scheme/user.rs`).

- `scheme.request` is recorded when the kernel queues a request.
  - `tag` is the request's SQE tag.
  - `opcode` is a `schemev2::Opcode`.
  - `caller` is the caller's pid.
  - `arg0` and `arg2` are the request's first and third arguments. For reads
    and writes they are the scheme's file number and the byte count.
- `scheme.response` is recorded when the kernel accepts the scheme's CQE for
  a request that is still waiting. A CQE the kernel rejects, for an unknown or
  already answered tag, is not recorded.
  - `ret` is the result the caller's system call gets: `-errno` on failure,
    and 0 for a response that carries file descriptors. It is the scheme's
    value after the kernel's normalization: `ECANCELED` becomes `EINTR` for a
    request being cancelled, and an `EINTR` the kernel did not ask for becomes
    `EIO`.
  - `responder` is the answering process's pid.
- `scheme.cancel` is recorded when the kernel asks the scheme to cancel an
  interrupted caller's request. It does not end the request: the scheme still
  answers it, usually with `EINTR`, and that answer is a `scheme.response`.

A tag is unique among one scheme's requests in flight and is then reused. A
request is therefore matched with the next response that has the same scheme
id and tag. A cancel in between is a mark on the request, not its end.

The pids let you follow one I/O through several schemes. For example, an
application's read reaches RedoxFS as a request with the application as
`caller`. While serving it, RedoxFS makes its own requests to the disk driver,
with RedoxFS as `caller`, before it answers as `responder`.

These events do not cover one-way messages (`close`, `detach`) or the kernel's
own mmap requests.

## Records and rings

A record is a sequence of little-endian 64-bit words:

| Word | Contents |
| --- | --- |
| 0 | Length in words (bits 7:0), argument count (15:8), event id (31:16), reserved zero (63:32) |
| 1 | Timestamp: the TSC |
| 2.. | One word per argument |

Event ids and names come from `ferric_trace::kernel::CATALOG`. Decoders use
that same catalog, and `ferric_trace::record::Records` decodes a buffer.

Each CPU has a 64 KiB ring, created when the CPUs are counted or on the first
run-time enable. When a ring is full, new records are dropped and counted.
Timestamps assume an invariant TSC that is synchronized across CPUs.

## The `kernel.trace` scheme

Only root (uid 0) may open `/scheme/kernel.trace`, because traces expose
timing and kernel behaviour. A capability is planned to replace this check.
The paths are defined in `ferric_trace::abi`.

| Path | Read | Write |
| --- | --- | --- |
| `events` | One line per compiled-in event: `<id> <name> <subsystem> <level> <on\|off>` | `enable <selection>` or `disable <selection>` patches the tracepoints live |
| `stats` | One line per CPU: `cpu <n> pending <words> lost <records>` | — |
| `cpu/<n>` | Drains whole records from CPU n's ring | — |

`events` and `stats` are read from a snapshot taken at the first read of each
open. A `cpu/<n>` read follows these rules:

- It consumes records only after they have been copied to the caller, so a
  failed copy loses nothing.
- It returns 0 bytes only when the ring is empty.
- It fails with `EINVAL` when the buffer cannot hold the next record.
  `abi::READ_MIN` bytes always hold one.
- It fails with `EAGAIN` while another reader is reading the same CPU.

A write naming no known event gives `ENOENT`. A write naming only events that
are not compiled in gives `EOPNOTSUPP`.

## Reading a capture on the host

To capture a trace on the target, run as root:

```sh
cat /scheme/kernel.trace/stats > stats.before
echo "enable syscall scheme" > /scheme/kernel.trace/events
# ... run the workload ...
echo "disable syscall scheme" > /scheme/kernel.trace/events
ferric-trace capture capture/
```

`ferric-trace capture <dir>` (the `ferric-trace` recipe; the smoke image has
it) works as follows:

- It reads `stats` for the list of CPUs and saves it as `<dir>/stats.start`.
- It drains each CPU's ring into `<dir>/cpu<n>`, stopping once it has the
  bytes `stats.start` listed as pending for that CPU, or earlier if a read
  returns 0 bytes. The capture is a snapshot of what was pending when it
  started. Draining until the ring is empty would never end while tracing is
  on, because the capture's own writes are traced and refill the ring as fast
  as it is read.
  A read that fails with `EAGAIN`, because another reader is draining the same
  CPU, is retried after a short wait.
- It reads `stats` again into `<dir>/stats.end`.
- Any other error, or a CPU that stays busy, stops the capture with a message
  naming the file.

A read drains a ring, so each record lands in exactly one file and nothing
drained is ever deleted:

- `<dir>` is created by the command, with any missing parents, and must not
  exist yet, so an earlier capture is never overwritten. It must not overlap the scheme path.
- An `INCOMPLETE` file is written into it first and removed only after
  `stats.end` is saved.
- If the capture fails or is killed, everything drained so far stays in
  `<dir>` with the marker, and the command says the capture is partial.
  `text` and `perfetto` refuse such a directory unless they are given
  `--partial`, because CPUs may be missing or cut short. With `--partial`, a
  record cut off at the end of a file is dropped and the whole records before
  it are kept.

`--scheme <path>` reads another scheme path.

Each ring holds 64 KiB, and a full ring drops new records. For each CPU the
command prints the records lost before the capture started and those lost
while it ran. The first count runs from boot, so compare it with
`stats.before`. If either shows new losses, the capture is missing records, and
slices may be cut short or left unmatched. For a long or busy workload, run
`ferric-trace capture` more than once while it runs, into separate
directories, or capture a shorter window.

Copy the directory to the host and use the same tool (`tools/ferric-trace/`)
on it:

```sh
cargo run -p ferric-trace-cli -- text capture/
cargo run -p ferric-trace-cli -- perfetto --tsc-mhz 3000 -o trace.json capture/
```

`text` prints one line per record in timestamp order: the CPU, the cycles
since the first record, the event and its named arguments.

`perfetto` writes Chrome trace event JSON. Open it at <https://ui.perfetto.dev>
or in `chrome://tracing`.

- **CPU tracks.** With `sched.switch` captured, each CPU track shows which
  context ran when, including the context that ran from the CPU's first
  record up to its first captured switch.
- **Context tracks.** Each context has a track (a process per kernel pid, a
  thread per context). It shows that context's system calls as slices, even
  when the context moved to another CPU while it was blocked. Records from
  before a CPU's first captured switch belong to the context that switch takes
  off the CPU, and are shown on its track.
- **Scheme requests.** Each request is an async slice on the caller's track,
  from `scheme.request` to the `scheme.response` with the same scheme and tag.
  The slice is named with the scheme id and opcode, and the end carries the
  responder's pid. A `scheme.cancel` is an instant on the slice, not its end.
- **Unmatched records.** A record whose start was not captured is an instant.

`--tsc-mhz` converts TSC cycles to time. Pass the traced machine's TSC
frequency; the default, 1000, shows one cycle as one nanosecond. A value below
1 MHz, or not a finite number, is refused: real TSCs run at hundreds of MHz or
more, and a tinier divisor would overflow timestamps to infinity. The format and
the mapping are described in `tools/ferric-trace/src/perfetto.rs`.

## Validation

- **Host tests.** `cargo test --locked -p ferric-trace --all-features` covers:
  - the compile-time catalog checks;
  - record encoding and decoding, including truncated and corrupt input;
  - ring wrap-around, loss counting, too-small buffers and uncommitted reads;
  - a two-thread test with a million records.
- **Host tool.** `cargo test --locked -p ferric-trace-cli` decodes captures
  (including bad and unknown records, records with too few or too many
  arguments, and duplicate CPUs), checks the text lines, checks the JSON
  mapping (slices on the right context across CPUs, records from before a
  CPU's first switch moved to the outgoing context with its CPU slice, async
  request slices matched by tag through cancels, instants for unmatched
  records, finite timestamps at the frequency limits), refuses bad
  `--tsc-mhz` values, runs the binary on a capture directory, and runs
  `capture` against a stand-in scheme directory (CPU list from `stats`,
  `EAGAIN` and `EINTR` retries, a CPU that stays busy, other errors, start and
  end snapshots and losses during the capture, refusing an existing or
  overlapping output, keeping what was drained with the `INCOMPLETE` marker
  when a capture fails, and loaders refusing such a capture without
  `--partial`).
- **Kernel builds.** `kernel-lints.yml` builds kernel Rustdoc with the trace
  features.
- **Smoke test.** `ferric-trace-test`, in `config/ferric-smoke.toml`, does the
  following:
  1. checks that `sched.switch` is on from boot;
  2. enables `syscall` and `scheme`;
  3. makes a `write` to a closed descriptor, a `read` of `stats`, a `write` to
     the null scheme and a `yield`;
  4. disables them;
  5. drains and decodes every CPU's ring;
  6. checks the enter/exit pairs, the results, the timestamp order, that
     `sched.switch` records are present, that the null-scheme write has a
     request (the first one on its CPU after the program's own
     `syscall.enter` for that write) and a matching response (same scheme and tag, the byte count as
     result, another process as responder), and that `disable` stops the
     stream.

  It prints `FERRIC_TEST_PASS trace-<step>` for each step, or
  `FERRIC_TEST_SKIP trace` without the scheme. The smoke script then runs
  `ferric-trace capture` and `ferric-trace text` on the result, and prints
  `FERRIC_TEST_PASS trace-capture` if both succeed.
- **Image workflow.** `image.yml` builds the smoke image with `trace = true`
  and `trace_boot = "sched"`. It requires all ten step markers, the
  `trace-capture` marker and the boot log line.

## Not implemented yet

- Tracepoints for other subsystems (page faults, IRQs, IOMMU, futexes,
  process lifecycle).
- Read-only mapping of the rings.
- An initfs trace configuration.
- Live viewing on the target, OpenTelemetry export and Perfetto's protobuf format.
- The CI overhead budget.
- Architectures other than x86_64.
