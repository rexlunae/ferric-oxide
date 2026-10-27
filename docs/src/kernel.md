# The Kernel

The Ferric Oxide kernel lives in `crates/kernel/`. It started as the Redox
kernel and is still a microkernel: it manages address spaces, contexts
(threads), scheduling, interrupts and the routing of requests between
processes and the schemes that serve them. File systems, drivers, the network
stack and most other services run as user-space daemons.

## System calls

System calls have an ABI close to an ordinary function call. On x86-64 a
program executes `syscall`, which switches from ring 3 to ring 0; the kernel
handles the call and returns with `sysretq`, as if the instruction had been a
`call`. The call numbers and structures are defined in the `redox_syscall`
crate, which the kernel builds from `crates/kernel/syscall/`. Programs normally
reach them through relibc and `redox-rt` rather than directly.

Most system calls operate on file descriptors. A call on a descriptor owned by
a user-space scheme becomes a message to that scheme's daemon, as described in
[Communication](./communication.md).

## Kernel schemes

Some schemes are implemented in the kernel itself, in `crates/kernel/src/scheme/`.
They include the inherited ones (`event`, `irq`, `memory`, `pipe`, `proc`,
`sys`, `time`, `acpi`, `debug`, `serio` and the user-scheme bridge) and several
that Ferric Oxide added, each behind a feature option and off by default:

| Scheme | Purpose | Option |
| --- | --- | --- |
| `kernel.iommu` | DMA domains on Intel VT-d and AMD-Vi | `kernel.iommu` |
| `kernel.regbuf` | Registered buffers and grants | `kernel.regbuf` |
| `kernel.ring` | Shared submission and completion rings | `kernel.rings` |
| `kernel.pcache` | Page cache read path for file systems | `kernel.page_cache` |
| `kernel.trace` | Tracepoints, trace rings and profiles | `kernel.trace` |

The [I/O chapter](./ferric-io.md) and [Kernel tracing](./ferric-tracing.md)
describe them in detail. Options are set in `ferric.toml`; see
[Configuration Settings](./configuration-settings.md).

## What changed from Redox

The main kernel changes so far are:

- An EEVDF scheduler with per-CPU run queues; see [Scheduling](./scheduling.md).
- Hardening: kernel and physical-map KASLR, KPTI with PCID, user ASLR and
  optional CET indirect-branch tracking and shadow stacks; see
  [Memory and CPU protection](./ferric-memory.md).
- IOMMU support, registered buffers, rings, per-CPU interrupt delivery and a
  page cache; see [Registered buffers, IOMMUs and rings](./ferric-io.md).
- Typed tracepoints with a crash dump and a persistent kernel log; see
  [Kernel tracing](./ferric-tracing.md).

Kernel virtualization support is prepared as host-tested crates but not wired
into the kernel yet; see [Virtualization](./ferric-virt.md).
