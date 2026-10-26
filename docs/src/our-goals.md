# Our Goals

Ferric Oxide aims to be a complete, general-purpose operating system written
in Rust that is fast enough to choose on its merits. It keeps the safety and
isolation of a microkernel and works to remove the performance cost that has
historically come with one.

The [Roadmap](./ferric-roadmap.md) turns these goals into phases with
testable exit criteria; [`ROADMAP.md`](https://github.com/rexlunae/ferric-oxide/blob/main/ROADMAP.md)
tracks progress. This page explains what the goals are and why.

## Performance you can measure

Performance is the reason the fork exists. Kernel, scheduler, file system and
network changes are judged by benchmarks, and regressions are bugs. The
targets are stated against Linux on the same hardware: for example, the
Phase 2 exit asks for NVMe 4K random reads at queue depth 32 to reach at least
80% of Linux io_uring with IOMMU enforcement on.

Today the project has the measurement tools (kernel tracing with an overhead
budget enforced in CI, `ferric-iobench` for fio-style disk jobs, network queue
benchmarks), but the comparison against Linux on reference hardware has not
been run yet. No performance claim is made without numbers.

## A microkernel without the IPC tax

A microkernel moves drivers and file systems into user space, where a crash
or exploit is contained. The usual cost is that every I/O request travels
through the kernel several times and its data is copied between processes.
Ferric Oxide's approach is to keep file descriptors as the **control plane**
(they name a resource and carry the permission to use it) while moving the
**data plane** onto shared memory:

- **IOMMU domains** (implemented, off by default) so a user-space driver can
  DMA only into memory it was given, which is what makes unprivileged drivers
  actually safe.
- **Registered buffers** (implemented, off by default) that an application
  registers once and grants, revocably, to the services handling its requests,
  so a device can DMA straight into the application's memory.
- **Submission and completion rings** (implemented for kernel-brokered
  operations, off by default) so the steady-state I/O path needs few or no
  system calls.
- **Per-core queues and interrupt steering** for NVMe, **kernel-bypass
  network queues** and a **shared page cache** (partly implemented; see
  [Implemented System](./ferric-oxide.md#implementation-status)).
- A **kernel-colocated driver tier** for a few hot drivers, isolated by safe
  Rust and protection keys and fenced by the IOMMU (planned).

The design is in [Registered Buffers, IOMMUs and Rings](./ferric-io.md) and
Phase 2 of the roadmap.

## Modern hardware only

Ferric Oxide targets machines made in roughly the last decade. The default
x86 user-space baseline is **x86-64-v3** (AVX2, BMI1/2, FMA), with an optional
**x86-64-v4** (AVX-512) build. Boot is UEFI only. ARM64 (ARMv8.0 and later) is
the second architecture. i586, BIOS boot and older CPUs are not supported, and
RISC-V is frozen until someone needs it.

Requiring modern hardware lets the kernel rely on features such as NX, SMEP,
SMAP, PCID, XSAVE, x2APIC and an IOMMU instead of carrying fallbacks, and use
CET, protection keys and AVX-512 where they exist. See
[Hardware Support](./hardware-support.md).

## Security in depth

Rust removes whole classes of memory errors, but not all of them, and not in
`unsafe` code or hardware. Ferric Oxide therefore also uses conventional
mitigations: KASLR, user ASLR, guard pages, KPTI where the CPU needs it,
Spectre v1 hardening, and optional CET indirect-branch tracking and shadow
stacks in the kernel. These are implemented; see
[Memory and CPU Protection](./ferric-memory.md).

The next step is finishing the **capability model** (planned, Phase 4):
replacing the remaining "is this root?" checks with explicit capability file
descriptors, starting every system service sandboxed with a declared
manifest, and a measured boot chain with TPM-sealed disk unlock.

## Fit into the systems people already run

- **Linux disks:** read ext4, LVM2 and LUKS volumes (implemented, read-only),
  then write ext4 and XFS (planned), all written clean-room from published
  format specifications so the project stays MIT licensed.
- **Linux software:** run C and C++ programs through relibc, and run code that
  needs a different OS's security model unmodified inside lightweight VMs
  ("enclaves", planned in Phase V; the hardware formats and VM interface are
  prepared).
- **Rust software:** the Rust standard library and major crates already
  support the `redox` targets, which Ferric Oxide keeps.

## Packages you can verify

`fox` replaces Redox's `pkg` as the target package manager. Packages are built
from git sources with Cargo, prebuilt binaries are protected by hashes and
signed build attestations, registries are signed git repositories, and every
package carries a CycloneDX SBOM. Local archives, signed git sources,
registries, advisories and package sets are implemented; an official registry,
dependency resolution and rollback are not yet. See
[Package Tooling: fox](./ferric-packaging.md).

## Observable by default

A system you can't see into is a system you can't tune. Kernel tracing
(implemented, off by default) provides typed tracepoints with build-time,
boot-time and run-time switches, Perfetto and OpenTelemetry export, crash
dumps that survive a warm reboot and flamegraphs from sampling profiles. See
[Kernel Tracing](./ferric-tracing.md).

## A WebAssembly userland (goal)

The long-term goal is for most user programs to ship as WebAssembly (wasm32
by default, wasm64 for programs that need more than 4 GiB), validated and
compiled for the exact CPU by a trusted compiler service, with native code as
an explicit exception list. This would give software-isolated processes, one
package for every CPU level and architecture, and capabilities by
construction. It is the most research-heavy part of the plan and is gated on
benchmarks. Only the import ABI and code-cache formats exist so far; see
[WebAssembly Interfaces](./ferric-wasm.md).

## Configurable, not forked per use case

Anything a user might want on or off (a driver tier, tracing, a CPU level, a
file system) is an option in `features.toml`, chosen in `ferric.toml` with
`make menuconfig` or the `ferric-config` CLI. One tree serves the server,
workstation and test images.

## Non-goals

- **Supporting old hardware.** Code paths for pre-v3 x86 CPUs, i586 and BIOS
  boot are not extended. Their remains in imported sources are kept only to
  ease upstream merges.
- **Becoming Linux.** Ferric Oxide runs Linux programs through libraries,
  ports and VMs. It does not implement the Linux kernel ABI.
- **GPL code in the tree.** All code in this repository is MIT licensed, and
  dependencies must use MIT-compatible licenses. Linux-compatible features are
  written from specifications, not ported from GPL sources.
- **Unmeasured complexity.** A faster design that can't show its numbers
  doesn't land. Several roadmap items (the colocated driver tier, the
  WebAssembly userland) are explicitly conditional on passing a benchmark
  gate.
