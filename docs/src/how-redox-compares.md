# How Ferric Oxide Compares to Other Operating Systems

Ferric Oxide shares a lot with other operating systems, and most of its ideas
have been tried somewhere before. What is unusual is the combination: a Rust
microkernel with a shared-memory I/O data plane, aimed at modern hardware.

## System calls

The user-space API is Unix-like. Programs call `open`, `pipe`, `lseek`,
`read`, `write`, `mmap`, `execv` and the other familiar POSIX functions
through relibc, the C library. Much of the machinery behind these functions
(typically the [man(2)](https://en.wikipedia.org/wiki/Man_page#Manual_sections)
functions) lives in user space, in relibc and its `redox-rt` runtime, rather
than in the kernel. The kernel's own system-call interface is smaller, is not
Linux-compatible, and is not yet stable. Ferric Oxide does not implement the
Linux kernel ABI; Linux programs run after recompilation, or (planned) in
enclaves.

## "Everything is a file"

In a model largely inspired by Plan 9, [resources](./resources.md) can be
file-like or socket-like, giving one unified API. Resources are named by
paths, and a resource managed by a particular service can be addressed with a
scheme-rooted path such as `/scheme/kernel.regbuf/region`. This is explained
in [Schemes and Resources](./schemes-resources.md).

## The kernel

The kernel is a microkernel whose architecture was inspired by MINIX and
seL4. It is about 60,000 lines of Rust, compared with tens of millions of
lines of C in Linux. Most system services are provided in user space, either
in libraries or as daemons.

A smaller kernel is easier to review and test, and a bug outside it does far
less damage. The code that would be inside a monolithic kernel still exists;
it has moved into user-space daemons, where it follows the principle of least
authority. Each component:

- is isolated in memory as a separate process, so its failure does not crash
  other components and its bugs cannot spread directly;
- has restricted communication with other components;
- runs without administrator privileges, or (as the capability model is
  finished, Phase 4) with only the specific capabilities it needs.

## I/O compared with Linux

Linux gets much of its I/O performance from keeping the whole path in one
address space and from interfaces like io_uring, AF_XDP and its page cache.
Ferric Oxide borrows those interfaces' ideas while keeping drivers and file
systems in user space:

| Linux | Ferric Oxide | Status |
| --- | --- | --- |
| io_uring submission and completion queues | `kernel.ring` rings shared between an application and the kernel | Implemented for kernel-brokered operations, off by default |
| io_uring registered buffers | Registered-buffer regions and revocable grants that a driver can DMA into | Implemented, off by default; used by `nvmed` |
| IOMMU DMA API | One VT-d or AMD-Vi DMA domain per user-space driver | Implemented, off by default |
| blk-mq per-CPU queues, IRQ affinity | Per-CPU NVMe queue pairs and interrupt-vector steering | Implemented, off by default; no balancer |
| AF_XDP | `ferric-netq` queue sockets | Copy mode implemented; zero-copy planned |
| Page cache | `kernel.pcache` read path for RedoxFS | Implemented for reads, off by default; writes and mappings planned |
| ftrace, perf | `kernel.trace` tracepoints, profiles and flamegraphs | Implemented, off by default |

The design is in [Registered Buffers, IOMMUs and Rings](./ferric-io.md), and
the targets it is measured against are in Phase 2 of the
[Roadmap](./ferric-roadmap.md).

## Compared with Redox OS

Ferric Oxide is a fork of Redox and shares its kernel design, schemes, relibc,
RedoxFS and user space. The main differences are its modern-hardware-only
baseline, the I/O and tracing work above, kernel hardening (KASLR, KPTI, CET),
Linux volume and file system support, the `fox` package manager, a
Cargo-first build with TOML feature configuration, and its acceptance of
AI-assisted contributions. See [Why Ferric Oxide?](./why-a-new-os.md).
