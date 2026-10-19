# Ferric Oxide roadmap

The full plan, with the design and exit criteria for every phase, is the
[Roadmap chapter](docs/src/ferric-roadmap.md) of the book. This file tracks
progress against it. For what the system does today, read
[Implemented System](docs/src/ferric-oxide.md). "Merged" lists pull requests
that landed; it does not mean the phase is done. A phase is done only when its
exit criteria are met.

Phases run in dependency order, several in parallel. The two release milestones
are **M1 "Server/VM usable"** (Phases 0–4, P through P5, T, V and guest
virtualization) and **M2 "Workstation usable"** (M1 plus Phases 5–6, with the
wasm userland as default if its benchmark gate passes).

## Status

Last updated 19 October 2026.

| Phase | Status | Merged | Open or next |
| --- | --- | --- | --- |
| **0 Foundation**: monorepo, feature config, image CI, benchmarks | In progress | Monorepo subtrees and layout [#6](https://github.com/rexlunae/ferric-oxide/pull/6)–[#12](https://github.com/rexlunae/ferric-oxide/pull/12), [#14](https://github.com/rexlunae/ferric-oxide/pull/14), [#29](https://github.com/rexlunae/ferric-oxide/pull/29); feature configuration [#2](https://github.com/rexlunae/ferric-oxide/pull/2), [#70](https://github.com/rexlunae/ferric-oxide/pull/70), [#71](https://github.com/rexlunae/ferric-oxide/pull/71); image build and QEMU boot CI [#3](https://github.com/rexlunae/ferric-oxide/pull/3), [#4](https://github.com/rexlunae/ferric-oxide/pull/4), [#13](https://github.com/rexlunae/ferric-oxide/pull/13), [#24](https://github.com/rexlunae/ferric-oxide/pull/24), [#87](https://github.com/rexlunae/ferric-oxide/pull/87), [#88](https://github.com/rexlunae/ferric-oxide/pull/88); system documentation and Rustdoc [#66](https://github.com/rexlunae/ferric-oxide/pull/66), [#69](https://github.com/rexlunae/ferric-oxide/pull/69) | [#98](https://github.com/rexlunae/ferric-oxide/pull/98) (CI cache). Not started: benchmark harness against Linux with per-PR numbers, reference-hardware runs. P1–P3 are part of this phase's exit |
| **P Packages** (`fox`) | In progress | P1a [#52](https://github.com/rexlunae/ferric-oxide/pull/52), P1b [#82](https://github.com/rexlunae/ferric-oxide/pull/82), P2a [#89](https://github.com/rexlunae/ferric-oxide/pull/89), P2b [#91](https://github.com/rexlunae/ferric-oxide/pull/91) | P3 registry: [#94](https://github.com/rexlunae/ferric-oxide/pull/94), [#96](https://github.com/rexlunae/ferric-oxide/pull/96), [#97](https://github.com/rexlunae/ferric-oxide/pull/97), [#99](https://github.com/rexlunae/ferric-oxide/pull/99), [#101](https://github.com/rexlunae/ferric-oxide/pull/101). Not started: P4 variants, P5 hardening |
| **T Kernel tracing** | In progress | T1 [#81](https://github.com/rexlunae/ferric-oxide/pull/81), T2a [#84](https://github.com/rexlunae/ferric-oxide/pull/84), T2b [#85](https://github.com/rexlunae/ferric-oxide/pull/85) (host tool, Perfetto export), T2c [#86](https://github.com/rexlunae/ferric-oxide/pull/86) (overhead budget in CI), T3a [#129](https://github.com/rexlunae/ferric-oxide/pull/129) (read-only ring mapping), T3b [#130](https://github.com/rexlunae/ferric-oxide/pull/130) (initfs `trace.toml`), T5a [#131](https://github.com/rexlunae/ferric-oxide/pull/131) (crash dump in reserved memory) | T3d [#137](https://github.com/rexlunae/ferric-oxide/pull/137) (timestamp clock: invariant TSC, else KVM clock, else HPET). Not started: T3c argument filters, T3e OpenTelemetry export; fault, IRQ and IOMMU tracepoints, persistent kernel log, capability-gated access, programmable probes |
| **1 Hardening** | Mostly done | x86-64-v3 baseline and AVX-512 state [#4](https://github.com/rexlunae/ferric-oxide/pull/4), [#13](https://github.com/rexlunae/ferric-oxide/pull/13); Spectre v1 [#17](https://github.com/rexlunae/ferric-oxide/pull/17); lint and `unsafe` ratchet [#19](https://github.com/rexlunae/ferric-oxide/pull/19); KASLR [#20](https://github.com/rexlunae/ferric-oxide/pull/20), [#30](https://github.com/rexlunae/ferric-oxide/pull/30); user ASLR [#21](https://github.com/rexlunae/ferric-oxide/pull/21), [#22](https://github.com/rexlunae/ferric-oxide/pull/22); guard pages [#23](https://github.com/rexlunae/ferric-oxide/pull/23), [#26](https://github.com/rexlunae/ferric-oxide/pull/26); KPTI and PCID [#32](https://github.com/rexlunae/ferric-oxide/pull/32), [#37](https://github.com/rexlunae/ferric-oxide/pull/37); syscall fuzzer and the bugs it found [#27](https://github.com/rexlunae/ferric-oxide/pull/27), [#28](https://github.com/rexlunae/ferric-oxide/pull/28), [#31](https://github.com/rexlunae/ferric-oxide/pull/31), [#33](https://github.com/rexlunae/ferric-oxide/pull/33)–[#35](https://github.com/rexlunae/ferric-oxide/pull/35); relibc entropy and signal-entry fixes [#18](https://github.com/rexlunae/ferric-oxide/pull/18), [#25](https://github.com/rexlunae/ferric-oxide/pull/25); CET IBT [#36](https://github.com/rexlunae/ferric-oxide/pull/36); shadow-stack groundwork [#43](https://github.com/rexlunae/ferric-oxide/pull/43), [#45](https://github.com/rexlunae/ferric-oxide/pull/45) | [#47](https://github.com/rexlunae/ferric-oxide/pull/47) (enable supervisor shadow stacks; waits on a hardware test). Not started: user-mode CET, scheme-protocol fuzzing |
| **2 I/O** | In progress | 2.1 IOMMU (VT-d, AMD-Vi, interrupt remapping, per-driver domains) [#38](https://github.com/rexlunae/ferric-oxide/pull/38)–[#42](https://github.com/rexlunae/ferric-oxide/pull/42), [#44](https://github.com/rexlunae/ferric-oxide/pull/44), [#46](https://github.com/rexlunae/ferric-oxide/pull/46), [#48](https://github.com/rexlunae/ferric-oxide/pull/48), [#61](https://github.com/rexlunae/ferric-oxide/pull/61)–[#63](https://github.com/rexlunae/ferric-oxide/pull/63), [#72](https://github.com/rexlunae/ferric-oxide/pull/72); 2.2 registered buffers [#49](https://github.com/rexlunae/ferric-oxide/pull/49)–[#51](https://github.com/rexlunae/ferric-oxide/pull/51), [#83](https://github.com/rexlunae/ferric-oxide/pull/83) | 2.3 rings: [#90](https://github.com/rexlunae/ferric-oxide/pull/90), [#92](https://github.com/rexlunae/ferric-oxide/pull/92), [#93](https://github.com/rexlunae/ferric-oxide/pull/93). 2.4 per-core queues and interrupt steering: per-(CPU, vector) delivery [#104](https://github.com/rexlunae/ferric-oxide/pull/104), MSI-X vectors on chosen CPUs [#105](https://github.com/rexlunae/ferric-oxide/pull/105), vector steering [#125](https://github.com/rexlunae/ferric-oxide/pull/125), per-core NVMe queue pairs [#127](https://github.com/rexlunae/ferric-oxide/pull/127), concurrent disk requests with close after in-flight I/O [#133](https://github.com/rexlunae/ferric-oxide/pull/133) (on QEMU, spilling to per-CPU queues does not beat one queue), a paired, confirmed same-boot A/B gate [#139](https://github.com/rexlunae/ferric-oxide/pull/139); NIC queues and a balancer to come. 2.5 kernel-bypass networking started: the queue ABI crate `ferric-netq` [#108](https://github.com/rexlunae/ferric-oxide/pull/108), its rings [#136](https://github.com/rexlunae/ferric-oxide/pull/136), copy mode in `driver-network` [#140](https://github.com/rexlunae/ferric-oxide/pull/140) (`drivers.net_bypass`), its on-target test `ferric-netq-test` [#141](https://github.com/rexlunae/ferric-oxide/pull/141), port ownership, TX validation and a TX benchmark against netstack [#142](https://github.com/rexlunae/ferric-oxide/pull/142). Not started: 2.6 shared page cache, colocated driver tier, the fio exit benchmark |
| **V Virtualization and enclaves** | Not started | | Needs 2.2–2.3 and Phase T |
| **3 Storage and filesystems** | Not started | | |
| **4 Capability model** | Not started | | |
| **5 Real hardware** | Not started | | |
| **6 Graphics and desktop** | Not started | | |
| **7 Server** | Not started | | |
| **8 Releases** (M1, M2) | Not started | | |
| **W WebAssembly userland** | Not started | | |

Numbers are pull requests in this repository.

## Updating this file

When you merge work that belongs to a roadmap phase, add its pull request to
the phase's row in the same pull request or right after it, move items from
"Open or next" to "Merged", update the status, and change the date above. Mark
a phase done only when its exit criteria in the
[Roadmap chapter](docs/src/ferric-roadmap.md) are met. Changes to the plan itself
go in that chapter.
