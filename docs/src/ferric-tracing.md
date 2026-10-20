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
| `kernel.trace_mm` | `true` | Compiles in `mm.page_fault` |
| `kernel.trace_irq` | `true` | Compiles in `irq.deliver` and `irq.ack` |
| `kernel.trace_iommu` | `true` | Compiles in `iommu.fault` (an `error`-level event, so every `kernel.trace_max_level` keeps it) |
| `kernel.trace_max_level` | `"info"` | Leaves out tracepoints more verbose than this level (`error` < `warn` < `info` < `debug` < `trace`) |
| `kernel.trace_boot` | `""` | Events to turn on at boot, written to the kernel `config.toml` as `trace.boot` |
| `kernel.trace_initfs_enable` | `[]` | Selections bootstrap turns on before `init` starts, from the initfs `trace.toml` |
| `kernel.trace_initfs_disable` | `[]` | Selections bootstrap turns off after those, from the same file |
| `kernel.trace_crash` | `false` | Reserves the crash region: a panic leaves its message and the newest records for the next boot ([Crash dump](#crash-dump)) |
| `kernel.trace_crash_base` | `"0x4000000"` | The crash region's physical address, written to the kernel `config.toml` as `trace.crash_base` |
| `kernel.trace_crash_test` | `false` | Testing only: writing `panic` to `kernel.trace/crash` panics the kernel and resets the machine after the dump |
| `kernel.log_persist` | `false` | Keeps the kernel log in reserved RAM too, so the next boot serves its end as `/scheme/sys/log_previous` ([Persistent kernel log](#persistent-kernel-log)); needs no `kernel.trace` |
| `kernel.log_persist_base` | `"0x4010000"` | The persistent log region's physical address, written to the kernel `config.toml` as `log.persist_base` |

The options below `kernel.trace` take effect only with it. The subsystem
options map to the kernel Cargo features `trace_syscall`, `trace_sched`,
`trace_scheme`, `trace_mm`, `trace_irq` and `trace_iommu`, and
`kernel.trace_max_level` to `trace_level_*`.
`ferric-config` rejects `kernel.trace` together with `kernel.profiling`, because
the profiler takes a CPU out of the system and that CPU would never answer the
interrupt that live patching waits for.

A selection is a comma- or space-separated list. Each item is `all`, a
subsystem (`syscall`, `sched`, `scheme`, `mm`, `irq`, `iommu`) or an event name (`sched.switch`). Besides
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
including the kernel's side of scheme requests, in the context switch, and in
the page fault and interrupt handlers (every IDT entry is an interrupt gate).
Each CPU's ring therefore has a single producer. A record that would be
written with interrupts on is counted as lost instead.

The events are:

| Event | Arguments |
| --- | --- |
| `syscall.enter` | `nr`, `a`, `b`, `c`, `d` (the system call number and first four arguments) |
| `syscall.exit` | `nr`, `ret` (the result as the kernel returns it: an error is `-errno`). A call that ends its context (`exit`, or a pending `SIGKILL`) has none |
| `sched.switch` | `prev_pid`, `next_pid`, `prev_id`, `next_id` |
| `scheme.request` | `scheme`, `tag`, `opcode`, `caller`, `arg0`, `arg2` |
| `scheme.response` | `scheme`, `tag`, `ret`, `responder` |
| `scheme.cancel` | `scheme`, `tag` |
| `mm.page_fault` | `addr`, `ip`, `flags`, `outcome` |
| `irq.deliver` | `irq`, `cpu`, `source`, `count`, `handles` |
| `irq.ack` | `irq`, `cpu`, `count` |
| `iommu.fault` | `unit`, `format`, `lo`, `hi`, `count` |

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

The codes in the last four events have one definition, next to the events in
`crates/trace/src/kernel.rs`, which the kernel encodes with and decoders read.

- `mm.page_fault` is recorded once per page fault, when the handler
  (`crates/kernel/src/memory/mod.rs`) has decided, before an unhandled fault
  becomes a signal or a panic. Faults the kernel takes on purpose to probe
  memory (the shadow-stack pool check) are not recorded.
  - `addr` is the faulting address and `ip` the instruction pointer.
  - `flags` is a `PageFaultFlags`: `present` (a protection fault, not a missing
    page), `write`, `user` (the access came from user mode), `fetch` and
    `reserved` (a reserved bit in a page table entry).
  - `outcome` is a `PageFaultOutcome`: 0 `mapped` (demand paging or copy on
    write; the access is retried), 1 `copy failed` (the kernel faulted copying
    to or from user memory, and the call fails with `EFAULT`), 2 `unhandled`.
- `irq.deliver` is recorded when the irq scheme (`crates/kernel/src/scheme/irq.rs`)
  counts an interrupt and wakes the `irq:` handles that get it.
  - `cpu` is the CPU it is counted for: the one that took it with
    `kernel.irq_percpu`, otherwise the BSP.
  - `source` is an `IrqSource`: 0 `line` (PIC or I/O APIC) or 1 `vector` (a
    vector reserved on the CPU, which MSI and MSI-X messages target).
  - `count` is that CPU's count of the IRQ with this interrupt, the value a
    driver writes back to acknowledge it, and `handles` how many handles woke.
  Interrupts the kernel handles itself (the timer, IPIs) are not recorded.
- `irq.ack` is recorded when the kernel accepts a driver's acknowledgement:
  the IRQ, the handle's CPU and the count. An `irq.ack` with the IRQ, CPU and
  count of an `irq.deliver` ends that interrupt's handling. An acknowledgement
  of a stale count changes nothing and is not recorded.
- `iommu.fault` is recorded, along with the kernel's log line, for each report
  an IOMMU writes: a VT-d fault recording register (a blocked DMA or interrupt
  request) or an AMD-Vi event log entry (any event, including the unit's own
  command and table errors). Conditions without a report of their own are only
  logged: VT-d invalidation queue errors and fault overflow, and a full AMD-Vi
  event log.
  - `unit` is the unit's register base.
  - `format` is an `IommuFormat`: 1 for a VT-d fault record, 2 for an AMD-Vi
    event log entry.
  - `lo` and `hi` are the report's two 64-bit words as the hardware wrote
    them, so nothing is lost. Decoders read them with `ferric-vtd`'s
    `FaultRecord::decode` or `ferric-amdvi`'s `Event`, as the kernel does.
  - `count` is how many identical reports in a row the record stands for
    (AMD-Vi runs are folded; VT-d records are one each).
  It is an `error`-level event: every maximum level keeps it.

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
run-time enable. When a ring is full, new records are dropped and counted, or,
with `kernel.trace_crash`, the oldest are overwritten and counted (see
[Crash dump](#crash-dump)).
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
| `events` | One line per compiled-in event: `<id> <name> <subsystem> <level> <on\|off>` | `enable <selection>` or `disable <selection>` patches the tracepoints live; `filter <event> <filter\|none>` sets or removes an argument filter ([Argument filters](#argument-filters)) |
| `stats` | One line per CPU: `cpu <n> pending <words> lost <records>` | — |
| `clock` | One line: `clock <tsc\|kvmclock\|hpet> period_fs <femtoseconds\|unknown>` ([Timestamps](#timestamps)) | — |
| `filters` | One line per filtered event, the command that sets it: `filter <event> <filter>` | — |
| `cpu/<n>` | Drains whole records from CPU n's ring | `commit <end>` consumes a mapped read up to `end` |
| `crash` | The previous boot's crash dump, from any offset (`kernel.trace_crash`; it exists only when there is one) | `panic` panics the kernel, with `kernel.trace_crash_test` only |

`events`, `stats`, `clock` and `filters` are read from a snapshot taken at the first read of each
open. A `cpu/<n>` read follows these rules:

- It consumes records only after they have been copied to the caller, so a
  failed copy loses nothing.
- It returns 0 bytes only when the ring is empty.
- It fails with `EINVAL` when the buffer cannot hold the next record.
  `abi::READ_MIN` bytes always hold one.
- It fails with `EAGAIN` while another reader is reading the same CPU.

A write naming no known event gives `ENOENT`. A write naming only events that
are not compiled in gives `EOPNOTSUPP`.

### Argument filters

`filter <event> <filter>` keeps only the records of `<event>` whose arguments
pass `<filter>`; other records are not written and not counted as lost. A
filter is up to four conditions joined by `&&`, each `<arg> <op> <value>`:

```sh
echo "filter syscall.enter nr == 3 && a < 0x1000" > /scheme/kernel.trace/events
echo "filter sched.switch next_pid == 42" > /scheme/kernel.trace/events
echo "filter syscall.enter none" > /scheme/kernel.trace/events
```

- `<arg>` is one of the event's argument names, as the event table above
  lists them.
- `<op>` is `==`, `!=`, `<`, `<=`, `>`, `>=`, or `&` (any of the value's bits
  set).
- `<value>` is a decimal or `0x` hexadecimal 64-bit number.

A filter is independent of whether the event is on: it is kept while the event
is off and applies when it is turned on. Setting a filter replaces the event's
previous one. A filter on an unknown event gives `ENOENT`, on an event that is
not compiled in `EOPNOTSUPP`, and one that does not parse (an unknown argument
or operator, a bad number, more than four conditions) `EINVAL`. The kernel logs
each filter it sets, and `filters` lists them in the same syntax, with values
in hexadecimal.

Each event's filter sits in a sequence-locked slot of atomics
(`ferric_trace::filter::Slot`). A tracepoint checks it without a lock: an event
without a filter costs one relaxed load more, and a record that races with a
filter being replaced is kept rather than dropped, so a filter only ever
removes records. A tracepoint rereads a slot at most eight times before it
gives up and keeps the record. The `filters` listing reads the slots while
holding the lock that serializes setting them, so it never meets a replacement
in progress and lists every filter that is set. If a slot still cannot be read,
the listing says so in a comment line, `# filter <event> is being replaced`,
rather than leaving the filter out.

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
  decode. A panic inside the dump does not start another.
- **Flight-recorder rings.** With `kernel.trace_crash`, a full ring overwrites
  its oldest records instead of dropping the new one, so the dump holds the
  last records before the panic even when nobody reads the rings. The producer
  moves `tail` past the oldest whole records (a compare-and-swap, since the
  reader stores `tail` too) and counts them in `lost` before it reuses their
  words. A reader checks `tail` again after each copy, and keeps the part of
  its copy from the new `tail` on, so it never returns a torn record and is not
  starved by a producer that keeps overwriting. The dump's copy checks the same
  way: records overwritten while it copies are dropped (or, if the producer
  overtook the whole copy, it copies again, up to four times), and the `lost`
  count is stored after the records, so it counts them. In this mode `lost` is an upper
  bound: a record copied just before it was overwritten is delivered and
  counted. Without `kernel.trace_crash`, a full ring drops new records as
  before.
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
  A read or commit that fails with `EAGAIN`, because another reader is
  draining the same CPU, is retried after a short wait.
- It takes the records out of each ring through its read-only mapping
  ([Mapping a ring](#mapping-a-ring), the `ferric-trace-map` crate): it copies
  whole records out with `View`, writes them to `<dir>/cpu<n>`, and only then
  writes `commit <end>`, so the kernel reuses no word before it is saved. A
  ring that cannot be mapped (a kernel before the mapping existed) is read
  instead. `--via map` insists on the mapping, `--via read` always reads, and
  the default, `--via auto`, maps where it can. The command prints `(mapped)`
  or `(read)` after each CPU's counts.
- It reads `stats` again into `<dir>/stats.end`.
- It copies the `clock` file into `<dir>/clock`, when the kernel has one.
- Any other error, or a CPU that stays busy, stops the capture with a message
  naming the file.

Either way each record taken from a ring lands in exactly one file, and
nothing drained is ever deleted:

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

Each ring holds 64 KiB, and a full ring drops new records (or overwrites the
oldest, with `kernel.trace_crash`). For each CPU the
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
since the first record, the event and its named arguments. The page fault,
`irq.deliver` and IOMMU fault lines end with the codes in words after ` | `,
such as `write user, mapped`, or the decoded IOMMU report
(`tools/ferric-trace/src/describe.rs`).

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
- **Interrupts.** Each interrupt is an async slice on the track of the CPU
  that took it, from `irq.deliver` to the `irq.ack` with the same IRQ, CPU and
  count: how long the driver took to handle it. A delivery that woke no handle
  is an instant.
- **Faults.** A page fault is an instant on the faulting context's track, with
  the access and outcome in words. An IOMMU fault is a global instant, a line
  across every track, with the decoded report.
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
## Sampling profiles

`kernel.profiling` is upstream's sampling profiler, separate from the
tracepoints (and, for now, exclusive with them). It sets one CPU aside, which
sends an NMI to every other CPU on a timer; each NMI saves the interrupted
instruction pointer, the TSC, the context's debug id and the frame-pointer
chain to that CPU's buffer, `/scheme/debug/profiling-<n>`. Writing `1` or `0` to
`/scheme/debug/ctl-profiling` starts or stops sampling. Ferric Oxide adds a read
of `ctl-profiling`, which gives `kernel_text 0x<address>`: where the kernel's
text was loaded, so kernel addresses can be symbolized under KASLR. It also
adds an owner: writing `x` takes the profiler for that open file and stops
sampling, other files' writes then fail with `EBUSY`, and closing the owner's
file stops sampling and frees it. The kernel writes a sample to a buffer whole
or not at all. The sample format and that line are in `ferric_trace::profile`.

```sh
ferric-trace profile prof/ -- ion -c 'ls -R /usr > /dev/null'   # as root
ferric-trace profile prof/ --seconds 5                          # or for a while
```

`ferric-trace profile` takes the profiler (a second recorder fails with
"the profiler is in use"), throws away whatever an earlier run left in the
buffers, turns sampling on, runs the command (or waits), drains every buffer
every 100 ms so none fills, turns sampling off and saves `kernel_text`, one
`samples<n>` file per CPU and a `names` file with the kernel's name for each
sampled context. Like a capture, the directory is marked `INCOMPLETE` until it
is complete (its parents are created as needed), and sampling is switched off
on every way out, errors included; if recording a command fails, the command is
killed and waited for. Loading a profile refuses one whose `samples<n>` files
skip a CPU, and decoding reports words that are not a whole sample (from a
kernel without the whole-sample writes) as one error and carries on after
them.
User stacks are only walked through frame pointers, so build the image with
`cpu.frame_pointers` ([#149](https://github.com/rexlunae/ferric-oxide/pull/149)) for useful user frames.
With `kernel.sys_proc` it also reads `/scheme/sys/proc` as it drains and saves
an `images` file: the `image` lines of every sampled context.

Those lines come from the loaders. Exec (`redox-rt`) records the program and
its interpreter, and `ld.so` each library it maps: the load bias and path
(`ferric_procinfo::ImageRecord`, written to the address space's `images`
handle in the proc scheme). The kernel keeps up to 64 per address space,
copies them on fork and starts empty on exec, so user addresses can be
symbolized with ASLR on. A program that lives less than one drain interval may
be missed, and its frames then show as `[unknown]`. `ld.so` takes a library's
record back when a library is unloaded (in the library's drop, so a failed
`dlopen` cleans up too; the address space's `images-unload` handle), but the
recorder keeps every image it saw, so samples taken before the unload still
resolve. Each snapshot is an epoch, and each sighting of an image an occurrence
with the epochs it was seen in (`image-spans`); an image that comes back after
missing from a snapshot is a new occurrence. The `batches` file gives the
sample files' lengths after each drain, which dates every sample with the epoch
of the snapshot that followed it. The symbolizer resolves a sample against the
occurrence live at its epoch, so old samples keep their library when another
(or the same one again) is later mapped at the same address, and falls back to
the newest image that holds the address.
The recorder reads the file before sampling, after the first drain, after
every fifth (about every 500 ms) and after the last: one read took about
1.6 ms on the CI image, 1.6% of the time when it was read at every drain, so
an image that lives less than half a second may be missed. `ferric-trace
profile` prints what the reads cost (`image snapshots: … ppm of the … from the
first to the last`).

```sh
ferric-trace flamegraph prof/ --kernel kernel.all --root build/x86_64/ferric-smoke/sysroot \
    --folded prof.folded -o prof.svg
```

`ferric-trace flamegraph` (on the host or the target) symbolizes each stack
from ELF symbol tables. Kernel addresses use the kernel ELF, with the KASLR
slide taken from `kernel_text`. User addresses use the image that holds them,
read from `--root` (unstripped binaries, such as the cookbook's build trees,
give names; stripped ones show `[<file>]`). The paths come from the target and
are not trusted: a path with `..`, or one that leaves `--root` through a
symbolic link, is not read, and its frames show `[<file>]`. It writes the folded stacks
(`root;frame;frame count`, the format other flame graph tools read) and its own
self-contained SVG (`tools/ferric-trace/src/svg.rs`): root at the bottom,
frames sorted by name, the name, count and share in each frame's tooltip.
`profile-image.yml` builds an image with `kernel.profiling`, `kernel.sys_proc`
and `cpu.frame_pointers`, boots it once, and profiles a short fuzz run.

## Validation

- **Host tests.** `cargo test --locked -p ferric-trace --all-features` covers:
  - the compile-time catalog checks, that every subsystem has events, the
    levels of the new events, and the page fault, interrupt source and IOMMU
    format codes;
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
  arguments, and duplicate CPUs), checks the text lines and the descriptions
  of page faults, deliveries and VT-d and AMD-Vi fault reports, checks the
  JSON mapping (slices on the right context across CPUs, records from before
  a CPU's first switch moved to the outgoing context with its CPU slice, async
  request slices matched by tag through cancels, interrupt slices from a
  delivery to its acknowledgement on another CPU, instants for stale
  acknowledgements, deliveries nobody waited for, page faults and global
  IOMMU faults, instants for unmatched records, finite timestamps at the
  frequency limits), refuses bad
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
  8. maps every CPU's ring, checks that writable, unreadable, partial and
     private mappings and an `mprotect` to writable are refused, finds the
     `write` to a closed descriptor by reading the mappings, and checks that
     `commit` refuses a position past `head` or inside a record and accepts
     the end of what was read;
  9. with `mm` on, writes to each page of a fresh anonymous mapping, unmaps
     it and `read`s into it, which fails with `EFAULT`: each page must have a
     `mm.page_fault` from user mode, a write, ending `mapped`, and the read one
     from the kernel ending `copy failed` (`trace-fault`).
  10. sets `filter syscall.enter nr == <write> && a == <closed fd>`, checks the
     `filters` listing, and repeats the marker calls with `syscall` on: only
     the write keeps its `syscall.enter`, while `syscall.exit` (no filter)
     records every call; then removes the filter and checks that unknown events
     and arguments are refused (`trace-filter`).
  11. reads the `clock` file and, with `syscall` on, makes two marker writes
      200 ms apart: when the clock's tick length is known (the KVM clock, the
      HPET), the time between their timestamps must be within a factor of two
      of the wall time; for the TSC it prints the frequency they imply
      (`trace-clock`).

  It prints `FERRIC_TEST_PASS trace-<step>` for each step, or
  `FERRIC_TEST_SKIP trace` without the scheme (and `FERRIC_TEST_SKIP
  trace-fault` on a kernel without `mm.page_fault`).
- **IOMMU faults.** `ferric-iommu-test`, which makes QEMU's `edu` device DMA
  where its domain does not allow it, turns `iommu.fault` on before its tests.
  Before each fault it announces it drains the rings, keeping the
  `iommu.fault` records, since `sched.switch` fills them from boot. At the end
  it checks that every announced fault has a record: on VT-d each DMA fault by
  page and access and the forged interrupt by its remapping entry, decoded with
  `ferric-vtd`; on AMD-Vi an I/O page fault event, decoded with
  `ferric-amdvi`, since QEMU leaves the address out of those. Each announced
  fault needs a record of its own. AMD-Vi events of two faults are identical,
  and the kernel merges identical events drained in one pass, so the test
  waits for the previous fault's record to come back from the rings before it
  causes the next fault. The kernel drains the event log from its event
  interrupt and after every command batch, but under QEMU the events have been
  seen to wait for the next command, so while it waits the test maps a page
  into its domain and unmaps it, which sends one. It prints
  `FERRIC_TEST_PASS iommu-trace`, which `check-iommu-log.sh` requires with
  either IOMMU, and a skip without one. It also turns the `irq` events on
  around the interrupt of `iommu-msi`, which it acknowledges itself as a driver
  would, and requires an `irq.deliver` that woke a handle and the `irq.ack`
  with the same IRQ, CPU and count (`FERRIC_TEST_PASS trace-irq`, in every
  boot mode, since that interrupt needs no IOMMU). No driver of the smoke image
  serves for this: `nvmed` does not acknowledge its MSI-X interrupts on the
  handle, and `e1000d` acknowledges only received packets. It runs before `pcid-spawner` starts
  the next driver, so the rings it drains are not yet anyone else's. The smoke script then runs
  `ferric-trace capture --via map` (through the mapping, which fails if a
  ring does not map) and `ferric-trace text` on the result, and prints
  `FERRIC_TEST_PASS trace-capture` if both succeed. When `/scheme` lists no
  `kernel.trace` it prints `FERRIC_TEST_SKIP trace-capture` instead, as on the
  `trace-overhead` job's baseline image, which requires that skip.
- **Overhead.** `ferric-trace-bench` and the `trace-overhead` job (see
  [Overhead budget](#overhead-budget)).
- **Image workflow.** `image.yml` builds the smoke image with `trace = true`,
  `trace_boot = "sched"`, `trace_initfs_enable = ["scheme.cancel"]` and
  `trace_crash = true`. It requires all sixteen step markers, the kernel's
  log of the filter, its `trace: clock` line, the `trace-capture` marker, the boot log line, the kernel's log of bootstrap's
  `enable scheme.cancel`, the crash region's log line, and
  `FERRIC_TEST_SKIP trace-crash` (a clean boot has no dump).
- **Crash dump.** `cargo test -p ferric-trace` writes dumps into a region and
  reads them back: the newest records that fit, a region too small for them,
  refusal of an incomplete dump, of a bad version and of every flipped bit,
  sections that run past the payload, and message truncation on a character
  boundary; and `Ring::copy_latest` with wrap-around, a corrupt record, and a
  flight-recorder producer that overwrites records during the copy (it drops
  the overwritten ones, or copies again).
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
- **Persistent log.** `cargo test -p ferric-trace` appends to a region and
  reads it back: order, wrap-around, a write longer than the region, re-arming,
  short copies, headers that do not fit, and a reset in the middle of a write
  (before the ring is full, while it wraps, and in a write longer than the
  ring). It also tests the early-boot buffer. Every image in `image.yml` has
  `log_persist = true`. The boot job requires the region's `boot 0` line and no
  previous log. In the `crash` job, the second boot must log `boot 1` and the
  previous boot's log, and `ferric-trace-test crash` checks that
  `/scheme/sys/log_previous` holds the test panic's `KERNEL PANIC` lines, and
  that this boot's `/scheme/sys/log` (and the previous boot's log, unless it
  wrapped) holds `Redox OS starting`, which the kernel logs before its heap
  exists (`FERRIC_TEST_PASS log-previous`). It then runs itself again as a
  user other than root, which must get `EPERM` opening the file
  (`FERRIC_TEST_PASS log-previous-root-only`).

## Persistent kernel log

With `kernel.log_persist`, everything the kernel logs (its own messages, the
panic handler's, and what user space writes through the `debug` scheme) also
goes to a second reserved region, so after a panic, a watchdog reset or a
plain reboot the next boot can show how the previous one ended. It does not
need tracing.

- **The region.** 64 KiB at `kernel.log_persist_base` (`0x4010000` by
  default, right after the crash region's default), reserved the same way as
  the crash region (the kernel's `reserved` module) and refused the same way:
  a region that is not wholly free RAM, or that overlaps the crash region,
  turns the feature off with a warning.
- **The format** (`ferric_trace::klog`): a header of eight words (magic
  `FOKLOG01`, version 1, the data length, the bytes written this boot, the
  boot's sequence number and the count the write in progress will end at),
  then a byte ring. A write stores the count it will end at before its bytes
  and the written count after them. After a reset in the middle of a write,
  the reader drops that write and the oldest bytes it may have overwritten,
  so the text never mixes old and new bytes. Once the ring is full the newest
  bytes replace the oldest, so the kept text can start in the middle of a
  line.
- **This boot.** `Log::write`, under the in-memory log's lock, appends every
  byte to the region. Before the heap exists there is no in-memory log, so the
  kernel keeps what it logs (the newest 16 KiB) in a static buffer,
  `ferric_trace::klog::EarlyLog`, and `log::init` moves it into the new
  in-memory log. `kmain` then arms the region, copying the in-memory log so
  far, so the region holds the whole boot, early lines included, until it
  wraps. With the feature, `/scheme/sys/log` holds the early lines too.
- **Who can read it.** Only root can open `/scheme/sys/log_previous`
  (`EPERM` otherwise). Upstream lets anyone read `/scheme/sys/log`, but the
  previous boot's log holds a whole boot, panics included.
- **The next boot.** Before arming, `kmain` copies what the previous boot left
  to the heap and logs `log: the previous boot's log (<n> bytes) is in
  /scheme/sys/log_previous`. That file serves the copy until the next reboot
  (empty after a cold boot). The boot number counts warm reboots since the last
  cold one: `log: persistent log at <base> (64 KiB), boot <n>`.

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

## Per-process introspection

With `kernel.sys_proc` (Kernel menu, off by default; kernel feature
`sys_proc`), the `sys` scheme serves `/scheme/sys/proc`: a snapshot of every
context, read in one go. Only root may open it (`EPERM` otherwise), because it
shows the address-space layout of every process. A capability will replace
the uid check with Phase 4.

Each context is a block of lines, in the format of the `ferric-procinfo`
crate (`crates/procinfo`), which the kernel writes with and tools parse with:

```text
context 12 pid 9 euid 0 egid 0 status runnable cpu 1 running yes active_ns 5210000 sched_ns 61000 name /usr/bin/ferric-proc-test
memory private 9670656 shared 65536 kernel 81920
fd 0 scheme pipe number 3
fd 3 scheme sys number 7
fd 4 scheme user:7 number 1
grant 0x400000 pages 12 provider allocated prot r-x
image 0x0 path /usr/bin/ferric-proc-test
```

- `context`: the context's debug id (the id `sched.switch` records carry, so
  a trace and this file name the same context), process id, effective user
  and group, state (`runnable`, `sleeping`, `blocked`, `dead`), the CPU it is
  on and whether it is running there, and its CPU time in nanoseconds
  (`active_ns` running, `sched_ns` scheduled). A running context's times
  include its current timeslice, so two reads in a row show them growing.
  Each CPU's idle context (`[kmain]`) is never charged. The name comes last and runs to the end of the line; control characters in it
  are printed as `?`, so a name cannot forge a line.
- `memory`: bytes of grants the address space owns (`private`) and shares
  with other address spaces or schemes (`shared`), and the kernel memory the
  context uses itself (kernel stack and FPU state). Borrowed device memory is
  in neither count.
- `fd`: each open descriptor with the scheme it is open on and the scheme's
  handle number. Kernel schemes are named. A user-space scheme has no name in
  the kernel, so it shows as `user:<scheme id>`; descriptors open on the same
  user scheme share the id. The scheme manager's descriptors show as `mgr`,
  except the scheme creation capability, which is `cap`.
- `grant`: each mapping of the address space, with its base address, size in
  pages, where the memory comes from (`allocated`, `allocated_shared`,
  `phys`, `external`, `fmap`) and its protection, as user space may use it:
  a `PROT_NONE` guard page (`kernel.prot_none`) is `---`, not readable.
  Threads of one process share an address space and list the same grants.
- `image`: each ELF image its loaders recorded (see "Sampling profiles"): the
  load bias (0 for an executable that is not position-independent) and the
  path, which runs to the end of the line.

The kernel builds the file in two passes, so it takes no lock out of order.
First it copies each context, and its descriptors' scheme ids, under that
context's lock. Then it reads each address space and the scheme table with
the context locks released. A process that changes between the two passes is
shown partly before and partly after the change.

`ferric_procinfo::processes` groups the file into one record per context.
`ferric-proc-test` (in the smoke image, with `sys_proc` on in CI) checks its
own block (name, CPU time, a file it holds open, an 8 MiB allocation), that
its CPU time grows between two reads, that a thread stack's guard page shows
as `---`, that no `mgr` descriptor names the capability, and that user 1000
is refused the file.

## Not implemented yet

- Tracepoints for other subsystems (memory mappings, IOMMU map and unmap,
  futexes, process lifecycle), and for the interrupts the kernel handles
  itself (the timer, IPIs).
- Live viewing on the target and Perfetto's protobuf format.
- Architectures other than x86_64.
- Per-descriptor detail in `/scheme/sys/proc` from the fork's own schemes
  (ring sizes, IOMMU domain mappings), and flamegraphs from the profiler with
  user frame pointers.
