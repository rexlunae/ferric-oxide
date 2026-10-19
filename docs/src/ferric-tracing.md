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
| `kernel.trace_initfs_enable` | `[]` | Selections bootstrap turns on before `init` starts, from the initfs `trace.toml` |
| `kernel.trace_initfs_disable` | `[]` | Selections bootstrap turns off after those, from the same file |
| `kernel.trace_crash` | `false` | Reserves the crash region: a panic leaves its message and the newest records for the next boot ([Crash dump](#crash-dump)) |
| `kernel.trace_crash_base` | `"0x4000000"` | The crash region's physical address, written to the kernel `config.toml` as `trace.crash_base` |
| `kernel.trace_crash_test` | `false` | Testing only: writing `panic` to `kernel.trace/crash` panics the kernel and resets the machine after the dump |

The options below `kernel.trace` take effect only with it. The next four map
to the kernel Cargo features `trace_syscall`, `trace_sched`, `trace_scheme`
and `trace_level_*`.
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

### The initfs trace configuration

`kernel.trace_initfs_enable` and `kernel.trace_initfs_disable` are applied
from user space instead, by bootstrap, before it starts `init`. Changing them
rebuilds only the base package's initfs, not the kernel. System calls made
before bootstrap applies them are not traced.

`ferric-config` writes the selections to `trace.toml` in its output directory
and checks them as the kernel and bootstrap will, so a mistake fails when the
configuration is generated rather than being logged at boot:

- every name must be an event or subsystem of the catalog;
- every selection must include an event the configured kernel compiles in
  (`kernel.trace_<subsystem>` and `kernel.trace_max_level`). A selection that
  also names events left out is kept, as the kernel applies the rest of it;
- each command must fit the kernel's 4096-byte limit, and the file must fit
  bootstrap's 64 KiB.

It also exports `FERRIC_INITFS_TRACE`,
and the base package copies the file into the initfs as `lib/trace.toml`.
The format is `ferric_trace::config`:

```toml
enable = ["syscall", "scheme.request"]
disable = ["syscall.exit"]
```

Both keys are optional lists of selections, and any other key is an error.
Bootstrap writes `enable <selection>` for each `enable` entry and then
`disable <selection>` for each `disable` entry to `/scheme/kernel.trace/events`,
as a root shell could. The kernel logs each change, for example
`trace: enable syscall (2 event(s) changed)`. A missing file does nothing.
Bootstrap logs a warning and boots on when the file does not parse, when a
command is refused, or when the kernel has no `kernel.trace` scheme.

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
| `syscall.exit` | `nr`, `ret` (the result as the kernel returns it: an error is `-errno`). A call that ends its context (`exit`, or a pending `SIGKILL`) has none |
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
| 1 | Timestamp, from the clock the kernel picked at boot (see [Timestamps](#timestamps)) |
| 2.. | One word per argument |

Event ids and names come from `ferric_trace::kernel::CATALOG`. Decoders use
that same catalog, and `ferric_trace::record::Records` decodes a buffer.

Each CPU has a 64 KiB ring, created when the CPUs are counted or on the first
run-time enable. When a ring is full, new records are dropped and counted.
The ring sits in kernel-allocated pages in the shared layout of
`ferric_trace::ring`: a 4 KiB header page (magic `FOTRACE1`, layout version,
data length, and `head`, `tail` and `lost` on cache lines of their own), then
the data pages.

### Timestamps

Records on different CPUs are merged by timestamp, so the clock must agree
across CPUs. The kernel picks one clock for the whole boot, before the first
ring exists (`crates/kernel/src/trace/clock.rs`, with the rule in
`ferric_trace::clock::choose`):

1. **`tsc`**, when CPUID leaf `0x8000_0007` reports an invariant TSC (EDX bit
   8; Intel SDM Vol. 3B, 18.17.1, and AMD APM Vol. 3, `Fn8000_0007_EDX`
   `TscInvariant`). One `RDTSC` per record.
2. **`kvmclock`**, when the TSC is not invariant but KVM offers a stable
   paravirtual clock, in nanoseconds. Hosts usually hide the invariant TSC bit
   from guests because it does not survive migration, so this is the common
   clock in a VM.
3. **`hpet`**, when neither is there but the HPET's main counter is 64 bits
   wide and running. It is one counter for all CPUs, read through MMIO, so it
   is the slowest.
4. Otherwise **`tsc`** anyway, with a warning in the log that timestamps of
   different CPUs may disagree.

The kernel logs its choice (`trace: clock kvmclock (the TSC is not invariant;
the KVM clock is stable)`), and the scheme's `clock` file says it as `clock
<name> period_fs <femtoseconds|unknown>`. The tick length is unknown for the
TSC, because the kernel does not calibrate it.

## The `kernel.trace` scheme

Only root (uid 0) may open `/scheme/kernel.trace`, because traces expose
timing and kernel behaviour. A capability is planned to replace this check.
The paths are defined in `ferric_trace::abi`.

| Path | Read | Write |
| --- | --- | --- |
| `events` | One line per compiled-in event: `<id> <name> <subsystem> <level> <on\|off>` | `enable <selection>` or `disable <selection>` patches the tracepoints live |
| `stats` | One line per CPU: `cpu <n> pending <words> lost <records>` | — |
| `clock` | One line: `clock <tsc\|kvmclock\|hpet> period_fs <femtoseconds\|unknown>` ([Timestamps](#timestamps)) | — |
| `cpu/<n>` | Drains whole records from CPU n's ring | `commit <end>` consumes a mapped read up to `end` |
| `crash` | The previous boot's crash dump, from any offset (`kernel.trace_crash`; it exists only when there is one) | `panic` panics the kernel, with `kernel.trace_crash_test` only |

`events`, `stats` and `clock` are read from a snapshot taken at the first read of each
open. A `cpu/<n>` read follows these rules:

- It consumes records only after they have been copied to the caller, so a
  failed copy loses nothing.
- It returns 0 bytes only when the ring is empty.
- It fails with `EINVAL` when the buffer cannot hold the next record.
  `abi::READ_MIN` bytes always hold one.
- It fails with `EAGAIN` while another reader is reading the same CPU.

A write naming no known event gives `ENOENT`. A write naming only events that
are not compiled in gives `EOPNOTSUPP`.

### Mapping a ring

A reader can map a CPU's ring instead of reading it, and read records in place
without a system call per batch:

- `mmap` of a `cpu/<n>` descriptor must ask for `abi::CPU_MAP_BYTES` from
  offset 0 with `PROT_READ` and `MAP_SHARED`. The kernel refuses `PROT_WRITE`
  (`EACCES`), a mapping without `PROT_READ`, a partial or a private mapping
  (`EINVAL`) and a fixed address (`EOPNOTSUPP`). `mprotect` cannot make the
  mapping writable later.
- `ferric_trace::ring::View` checks the header and copies whole records from
  the mapping. Each copy reports its end position.
- Writing `commit <end>` to the descriptor lets the kernel reuse the words up
  to `end`. The kernel accepts only the end of a record between its own `tail`
  and `head`, and returns `EINVAL` otherwise. Like a read, a commit returns
  `EAGAIN` while another reader is using that CPU.

The kernel is the only writer of a mapped ring. It never takes a value from
the mapping that it did not store itself: the ring's size is kept in the
kernel, every position is masked to the data before use, and `commit`'s `end`
is checked against the kernel's own counters. The reader sees `head` and
`tail` change under it, so `View` copies a record before decoding it.

A `read` of the same CPU can consume records while a mapped reader is copying
them, and the kernel can then reuse those words. After each copy, `View`
checks `tail` again and starts over if it moved past the copy, so a copy never
mixes two records. Two readers of one CPU can still both see a record. A
collector that needs each record exactly once should be the CPU's only
reader.

## Crash dump

With `kernel.trace_crash`, a kernel panic leaves its message and the newest
records of every CPU's ring in memory that the next boot finds again, in the
manner of Linux's pstore/ramoops:

- **The region.** 64 KiB of RAM at a fixed physical address
  (`kernel.trace_crash_base`, 64 MiB by default). During memory setup the
  kernel takes it out of the free memory, if all of it is free RAM in the boot
  memory map, and maps it with the boot-time identity-mapped areas; otherwise
  it logs a warning and keeps no dumps. Firmware does not clear RAM on a warm
  reset (QEMU's `system_reset`, a reboot through the 8042 or ACPI), and boot
  loaders allocate from the top of memory, so the same bytes are there on the
  next boot. A cold boot finds random bits, which never pass as a dump.
- **The dump.** The panic handler's first act is to write the panic message
  (up to 512 bytes) and, for each CPU, its ring's `lost` count and the newest
  whole records that fit in an equal share of the room left, oldest first. It
  copies records with atomic loads and consumes nothing, since the other CPUs
  are still running; a record a producer was writing at that moment can come
  out torn, and readers keep a CPU's records up to the first that does not
  decode. A panic inside the dump does not start another. The records are the
  ones still in the ring: a ring that nobody reads fills up and then drops new
  records (its `lost` count says how many), so the dump only holds the last
  records before the panic while a reader keeps the rings from filling, as
  `ferric-trace capture` does. A flight-recorder mode that overwrites the
  oldest records instead is not implemented yet.
- **The format** (`ferric_trace::crash`): a header of eight words (magic
  `FOCRASH1`, version 1, state, payload length, checksum, and the timestamp
  clock as `1 +` its kind and its tick length in femtoseconds, both `0` in
  dumps from kernels that did not record it, whose timestamps are TSC cycles),
  then sections, each
  a word `kind | cpu << 8 | bytes << 32` and its data: the message, or a CPU's
  `lost` count and records. The state is set to complete last, and the checksum
  (64-bit FNV-1a over every other word) is checked before anything is read, so
  a dump cut short by a reset or a second panic is not used.
- **The next boot.** `kmain` copies a complete dump to the heap, logs
  `trace: the previous boot panicked on CPU <n>: <message>`, and arms the region
  for this boot. `/scheme/kernel.trace/crash` then serves the copy (root only,
  like the rest of the scheme) until the next reboot.

On the target or on the host:

```sh
ferric-trace crash                         # reads /scheme/kernel.trace/crash
ferric-trace crash dump.bin --out crash/   # also saves the records as a capture
ferric-trace perfetto crash/ -o crash.json
```

`ferric-trace crash` prints the message, the clock line, each CPU's record
count and bytes, `lost` count and torn bytes, and the records as text. `--out`
saves the capture, with a `clock` file when the dump records its clock, the way
`ferric-trace capture` does, marked `INCOMPLETE` until every
file is written.

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
- It copies the `clock` file into `<dir>/clock`, when the kernel has one.
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

A capture whose `clock` names the KVM clock or the HPET carries its own tick
length, and `perfetto` converts with that, however slow the clock (an HPET tick
can be longer than a microsecond). Two inputs with different clocks are
refused, and so is an input without a `clock` file (from a kernel that does not
say, so its timestamps are TSC cycles) together with one whose clock is not
the TSC. For TSC timestamps, and for captures without a `clock` file,
`--tsc-mhz` converts cycles to time. Pass the traced machine's TSC frequency; the default, 1000, shows one cycle as one nanosecond. A value below
1 MHz, or not a finite number, is refused: real TSCs run at hundreds of MHz or
more, and a tinier divisor would overflow timestamps to infinity. The format and
the mapping are described in `tools/ferric-trace/src/perfetto.rs`.

### OpenTelemetry

`otlp` writes the capture as OpenTelemetry traces, in the OTLP/JSON encoding
of an `ExportTraceServiceRequest` (OTLP 1.x, "JSON Protobuf Encoding"):

```sh
cargo run -p ferric-trace-cli -- otlp --start-unix-ns 1760000000000000000 -o otlp.json capture/
curl -H 'Content-Type: application/json' --data-binary @otlp.json http://collector:4318/v1/traces
```

An OTLP/HTTP collector, or a backend such as Jaeger that takes OTLP, accepts
the file on `/v1/traces`; the OpenTelemetry Collector's `otlpjsonfile`
receiver also reads it from disk.

- **Spans.** Each system call is a span, and each scheme request a client span.
  Records before a CPU's first `sched.switch` belong to the context that switch
  puts out, so a call that moved CPUs still pairs. A request made during a
  system call is the system call's child and shares its trace; one still open
  when its call ends (its response was lost) is no later call's parent. Every
  span without a parent starts a trace of its own.
- **Resources.** The spans of each process are under a resource whose
  `service.name` is `pid <n>` (with `process.pid` and `service.namespace`
  `ferric-oxide`). `thread.id` is the context and `cpu.logical_number` the CPU.
- **Arguments** of the opening and closing records are attributes named
  `<event>.<argument>`, as hexadecimal strings. A cancel, and any other record
  made while a span was open on its context, is a span event; a record outside
  every span is a span of zero length. A `sched.switch` is a record of the
  context it puts out, so a call that blocks shows its switches.
- **Status.** A system call or request whose result is an error number has
  status `ERROR` with the error's text. A span still open at the end of the
  capture ends at its context's last record (on any CPU) and has
  `ferric.unfinished` set.

A capture does not record the wall time, so `--start-unix-ns` gives the Unix
time of its first record in nanoseconds; the default, 0, makes every time the
time since the first record. Later times use the capture's clock and
`--tsc-mhz` as `perfetto` does. Trace ids start with a hash of every record
and the start time, so two captures sent to one backend keep separate traces. The
mapping is described in `tools/ferric-trace/src/otlp.rs`.

## Validation

- **Host tests.** `cargo test --locked -p ferric-trace --all-features` covers:
  - the compile-time catalog checks;
  - record encoding and decoding, including truncated and corrupt input;
  - ring wrap-around, loss counting, too-small buffers and uncommitted reads;
  - the shared layout's header, `View` reads that store nothing, header
    checks, and `commit_to` refusing positions past `head` or inside a
    record;
  - a two-thread test with a million records;
  - `trace.toml` parsing, refusal of unknown keys and wrong types, catalog,
    compiled-in and size checks of the selections, command order, and a TOML
    round trip.
- **Configuration.** `cargo test --locked -p ferric-config` checks the
  generated `trace.toml` and `FERRIC_INITFS_TRACE`, that nothing is generated
  without the options or without `kernel.trace`, and that a misspelt event,
  a selection with no compiled-in event, an oversize command and an oversize
  file fail generation.
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
  1. reads the initfs `trace.toml` (CI's image turns on `scheme.cancel` there),
     checks that the events it leaves on are on, and turns off again the ones
     the later steps test;
  2. checks that `sched.switch` is on from boot;
  3. enables `syscall` and `scheme`;
  4. makes a `write` to a closed descriptor, a `read` of `stats`, a `write` to
     the null scheme and a `yield`;
  5. disables them;
  6. drains and decodes every CPU's ring;
  7. checks the enter/exit pairs, the results, the timestamp order, that
     `sched.switch` records are present, that the null-scheme write has a
     request (the first one on its CPU after the program's own
     `syscall.enter` for that write) and a matching response (same scheme and tag, the byte count as
     result, another process as responder), and that `disable` stops the
     stream;
  7. maps every CPU's ring, checks that writable, unreadable, partial and
     private mappings and an `mprotect` to writable are refused, finds the
     `write` to a closed descriptor by reading the mappings, and checks that
     `commit` refuses a position past `head` or inside a record and accepts
     the end of what was read;
  8. reads the `clock` file and, with `syscall` on, makes two marker writes
     200 ms apart: when the clock's tick length is known (the KVM clock, the
     HPET), the time between their timestamps must be within a factor of two
     of the wall time; for the TSC it prints the frequency they imply
     (`trace-clock`).

  It prints `FERRIC_TEST_PASS trace-<step>` for each step, or
  `FERRIC_TEST_SKIP trace` without the scheme. The smoke script then runs
  `ferric-trace capture` and `ferric-trace text` on the result, and prints
  `FERRIC_TEST_PASS trace-capture` if both succeed. When `/scheme` lists no
  `kernel.trace` it prints `FERRIC_TEST_SKIP trace-capture` instead, as on the
  `trace-overhead` job's baseline image, which requires that skip.
- **Overhead.** `ferric-trace-bench` and the `trace-overhead` job (see
  [Overhead budget](#overhead-budget)).
- **Image workflow.** `image.yml` builds the smoke image with `trace = true`,
  `trace_boot = "sched"`, `trace_initfs_enable = ["scheme.cancel"]` and
  `trace_crash = true`. It requires all thirteen step markers, the kernel's
  `trace: clock` line, the `trace-capture` marker, the boot log line, the kernel's log of bootstrap's
  `enable scheme.cancel`, the crash region's log line, and
  `FERRIC_TEST_SKIP trace-crash` (a clean boot has no dump).
- **Crash dump.** `cargo test -p ferric-trace` writes dumps into a region and
  reads them back: the newest records that fit, a region too small for them,
  refusal of an incomplete dump, of a bad version and of every flipped bit,
  sections that run past the payload, and message truncation on a character
  boundary; and `Ring::copy_latest` with wrap-around and a corrupt record.
  `cargo test -p ferric-trace-cli` reads a dump with a torn record into a
  capture and saves it. On the target, the `crash` job boots a third image,
  whose kernel also has `kernel.trace_crash_test`, with `BOOT_REBOOT=1`
  (`boot-smoke.sh` without `-no-reboot`). On the first boot
  `ferric-trace-test crash` turns on the `syscall` events, makes a marker
  `write`, prints `FERRIC_TEST_PASS trace-crash-armed` and writes `panic` to
  `kernel.trace/crash`; the kernel dumps and resets. On the second boot it
  checks that the dump parses, that its message is the test's panic, that it
  records the clock this boot's `clock` file names, and that its records hold the marker `write` and the `write` of `panic`
  (`FERRIC_TEST_PASS trace-crash-read`). The job also requires the kernel's
  lines for the dump, the reset and the dump found, and exactly two boots.

## Overhead budget

Tracing must stay cheap enough to leave compiled in. The roadmap budget, which
the `trace-overhead` job in `image.yml` enforces on the workloads below, is:

| Comparison | Workload | Budget |
| --- | --- | --- |
| Compiled in, every event off, against compiled out | `syscall` | at most 1% slower |
| Every compiled-in event on (`enable all`), against all off | `io` | at most 5% slower |

Every workload and comparison is reported; the table says which ones fail the
job. The two kernels of the first comparison are built separately, so it also
measures where the linker put the hot code, and which runner the job got.
On the null system call that is small and repeatable: +0.53% and +0.54% in
two runs. There a disabled tracepoint is the largest share of the work, so any
cost it has shows first. On `io`, which runs through far more kernel code, the
same comparison went from -4.25% to +1.12% between those two runs with no
tracepoint changed, so judging it would judge the layout. The second comparison runs on one kernel, so `io` is
judged there.

`ferric-trace-bench`, in `config/ferric-smoke.toml`, times two workloads in
rounds:

- `io`: a 4 KiB `pwrite` and `pread` of a file on the root file system. Each
  call is a request to RedoxFS through its scheme, so a round crosses system
  call entry and exit, the scheme request path and context switches.
- `syscall`: a `write` to a descriptor that is not open, the cheapest system
  call that reaches the kernel's dispatch. Its tracepoints are a large share of
  its time, so it is where a compiled-in, disabled tracepoint is judged. With
  every event on it is reported but not budgeted: a record per entry and exit
  is far more than 5% of a call that does nothing else.

On a kernel without `kernel.trace` every round runs as state `out`. With it,
the program runs a block of back-to-back rounds with all events off (`off`),
exactly as the `out` rounds run. Then it alternates rounds with all off
(`pairoff`) and with `enable all` (`on`), draining the rings after each `on`
round. Toggling tracepoints (live patching and its IPIs) and draining disturb
the caches of the round that follows. So `off` is compared with `out`, and `on`
with `pairoff`, and the two are never compared across. The program prints
`FERRIC_BENCH <workload> <state> min_ns=<n> median_ns=<n> rounds=<n>` for each
workload and state, and `FERRIC_BENCH lost records=<n>` for records dropped
during the `on` rounds.

The image workflow builds a second smoke image whose kernel differs only in
leaving out `kernel.trace`. The `trace-overhead` job boots it and the traced
image ten times each, interleaved (a boot takes about 20 seconds).
`scripts/ferric-ci/check-trace-overhead.sh` takes each boot's fastest round of
each workload and state, and compares the medians of those over the boots.
The fastest round keeps scheduling noise within a boot out. The median over
boots handles the variation between boots: kernel image KASLR moves the code
and runner load changes, and the first runs showed a spread of about 3%,
three times the budget.

Every boot's bench and trace result lines go into a few workflow annotations
per image (GitHub keeps ten notices per step and 4 KiB of each). A boot that
fails, or lacks `FERRIC_TEST_PASS trace-bench`, fails the job, with an
annotation that puts its test results and crash lines ahead of the serial
tail, so a failing run can be read without its logs. `OFF_BUDGET_PCT` and
`ON_BUDGET_PCT` set the budgets, which are checked against the unrounded
medians. The script fails, naming the logs, when a boot lacks a result or a
traced boot lacks its `lost records` line. It warns when records were dropped,
since a dropped record costs less than a written one, and prints every result
as a notice on the run.

## Not implemented yet

- Tracepoints for other subsystems (page faults, IRQs, IOMMU, futexes,
  process lifecycle).
- A reader that maps the rings in the `ferric-trace` tool (it still reads them).
- Live viewing on the target and Perfetto's protobuf format.
- A flight-recorder ring mode, so a crash dump keeps the last records even
  when nobody reads the rings.
- Architectures other than x86_64.
