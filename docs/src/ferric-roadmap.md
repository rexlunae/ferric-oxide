# Roadmap

This chapter is the plan for Ferric Oxide: where the system is going, in what
order, and how each step is judged finished. It was adopted in October 2026.
For what each phase has delivered so far, with links to the pull requests, see
the status table in [`ROADMAP.md`](https://github.com/rexlunae/ferric-oxide/blob/main/ROADMAP.md)
at the repository root. For what is implemented today, see
[Implemented System](ferric-oxide.md); the roadmap describes intent, and a phase
appearing here does not mean any of it exists.

The phases are in dependency order, and each one ends with exit criteria you
can test. There are no dates. Several phases run in parallel once Phase 0 is
done; the [dependency summary](#dependency-summary) shows how they connect.

## Guiding decisions

1. **Modern hardware only.** The default x86 target is **x86-64-v3** (AVX2, BMI1/2, FMA, MOVBE). An optional **x86-64-v4** build adds AVX-512. v3 is the right floor because Intel client chips from Alder Lake on ship without AVX-512, so a v4 floor would exclude most Intel laptops. AMD Zen 4/5 and Intel Xeon support v4. Hot libraries use runtime dispatch for AVX-512 (crypto, memcpy, checksums, compositor blits) so that v3 images can still use it when it's there. Besides the ISA level, the target platform assumes NX, SMEP, SMAP, UMIP, PCID/INVPCID, FSGSBASE, XSAVE/XSAVES, x2APIC, an IOMMU (VT-d or AMD-Vi), UEFI and a TPM 2.0, so the kernel is free to require them. Which of these a build actually checks or enforces today is in [Memory and CPU protection](ferric-memory.md) and [Build and configuration](ferric-build.md); TPM use arrives with the Phase 4 boot chain. It can *use* CET (shadow stacks and IBT), PKU/PKS and WAITPKG where they exist. Support for i586, BIOS boot and pre-v3 CPUs gets dropped. aarch64 (ARMv8.2+) stays as the second architecture. riscv64 is frozen until someone needs it.
2. **File descriptors stay the control plane. Shared memory becomes the data plane.** That settles the bottleneck concern without losing the capability model (see Phase 2).
3. **Independent of upstream.** The connection to Redox is historical: nothing syncs from Redox, and a Redox change is ported by hand when it is wanted (decided 26 October 2026). Internal names stay, because the Rust targets, tools and imported code depend on them. Optional work still goes behind feature flags.
4. **Measure everything.** No performance claim lands without a benchmark in CI.
5. **Userland is WebAssembly by default (goal).** User programs ship as **wasm32**, with **wasm64** reserved for programs that need more than 4 GiB of address space. Both are validated and compiled natively per CPU by a kernel-trusted compiler, with native code as an explicit exception list. This is gated on benchmarks (Phase W).

## Phase 0: Foundation

- Import kernel, base, relibc, redoxfs, bootloader and book into this repository as git subtrees, keeping their history.
- GitHub Actions builds the image and boots it in QEMU/KVM with a smoke test, using the existing `acid` and `os-test` suites.
- **Feature configuration system:** `ferric.toml` (the selected configuration) plus `features.toml` (the feature catalog), edited by hand, by agents, or through `make menuconfig` (the TUI). Every optional item in this roadmap (KPTI, kernel-colocated drivers, AVX-512 paths, sandbox defaults) lands as a toggle there, and CI tests a set of named profiles.
- **Benchmark harness against Linux on the same hardware:** syscall and IPC round trip, context switch, fio 4K random read at QD1 and QD32, sequential read, TCP throughput and latency (iperf3), nginx or a Rust HTTP server req/s, boot-to-shell time, and a kernel compile. Results go to a dashboard with each commit tracked over time.
- **Reference hardware:** QEMU/KVM, one AMD Zen 4/5 desktop (v4), one recent Intel laptop (v3, Wi-Fi, iGPU) and one ARM board.

**Exit:** a single command produces a booting image from a TOML profile, and CI posts benchmark numbers for every PR.

## Phase P: Packages (`fox`, working name)

The target package system is Cargo-based: packages come from git URLs, prebuilt binaries are protected by hashes, a signed registry is backed by git, and SBOMs exist from day one. In the design, a package is a git repo at a signed tag, binaries are reproducible `pkgar` archives (reused from upstream) with signed build attestations, the registry is a git repo of TOML files with TUF-style signed metadata, every artifact ships a CycloneDX SBOM, and `fox` replaces `pkg` and wraps the cookbook and installer. Only part of this exists so far: local archives and signed git sources work, while the registry is P3 work that has not merged. [Package tooling: fox](ferric-packaging.md) describes what is implemented.

**When:** alongside Phases 0–1, starting right after the monorepo restructure. P1–P3 count toward Phase 0's exit, so images get assembled from verified packages from the start.

| Step | Delivers | Depends on |
|---|---|---|
| **P1 Core and verify** | `fox install <file.pkgar>`, `fox verify`, `fox sbom`, an in-place store. `fox` replaces `pkg` on the target. | Monorepo restructure |
| **P2 Git and Cargo builds** | Hermetic Cargo builds, attestations carrying SBOM hashes, `cargo fox build/package/release`, and `fox install <git-url>` (builds from source, trust-on-first-use keys) | P1 |
| **P3 Registry** | The signed git registry (`ferric-registry`) with CI builders, advisories and `fox audit`, image SBOMs, `ferric.toml [packages]` plus `fox.lock`, images built through fox, and cookbook recipes imported | P2, Phase 0 CI |
| **P4 Variants** | wasm32/wasm64 artifacts with the install-time AOT hook into the look-aside cache, plus x86-64-v4 and aarch64 native artifacts | Phase W2/W3 for wasm, the Phase 1 AVX-512 state work for v4 |
| **P5 Hardening** | Generations and rollback, independent rebuilders with threshold signing, VEX statements, sandboxed builds through `contain`, and capability manifests enforced at install | Phase 4 |

**Working defaults** (the maintainer can still override them): the tool is called `fox`; packages install in place first, with generations in P5; one CI signing key now and two independent reproducible builders later; prebuilt binaries for unregistered git URLs only from explicitly trusted builders, off by default.

**Exit:** a Phase 0 image is built entirely from registry packages listed in `ferric.toml`/`fox.lock`. Every installed artifact has passed author-tag, registry and builder signature checks. A system SBOM is produced, and `fox audit` flags a seeded test advisory. A package also installs straight from a git URL by building from source. P5 is required for M1.

## Phase T: Kernel tracing and introspection

This runs alongside Phases 1–2 because every later benchmark depends on it. Before this phase the kernel had only a small `log` module, a time-sampling profiler behind the `profiling` Cargo feature, `syscall_debug` printing, and ptrace; what Phase T has added so far is in [Kernel tracing](ferric-tracing.md). The goal is Linux-class introspection (ftrace, perf, bpftrace, `/proc`) with overhead you choose.

- **Structured tracepoints:** typed, compile-time-registered trace events for syscalls, scheme requests and responses (tagged with the ring SQE/CQE ids, so one I/O can be followed app → RedoxFS → NVMe), context switches and scheduler decisions, page faults and mappings, IRQs and MSI-X vectors, IOMMU map/unmap, futexes, and process lifecycle. Built on the Rust `tracing` model (spans plus events) with a `no_std` kernel backend.
- **Near-zero cost when off:** each tracepoint is a patched-out jump (a static key) using the kernel's existing `self_modifying` code-patching. Disabled tracepoints cost a NOP. Enabled ones write to **per-CPU lock-free ring buffers** in a compact binary format, with no formatting in the kernel.
- **Three levels of control:**
  1. **Build time** (TOML/TUI config): compile each subsystem's tracepoints in or out entirely, and set a maximum level (error, warn, info, debug, trace). A `perf` profile can ship with everything compiled out except the essentials.
  2. **Boot time:** a kernel command line, or an initfs TOML file, enables chosen events from the first instruction, for debugging boot.
  3. **Run time:** a capability-gated `trace` scheme turns individual events and filters on and off live, and per-CPU buffers can be mapped read-only into a collector. Getting the handle is a capability (Phase 4), not a root check, because traces leak timing and addresses.
- **Consumers:** a `ftrace`-like text viewer, export to Perfetto/Chrome trace format and OpenTelemetry, flamegraphs from the existing profiler extended to user space with frame pointers, and a `/scheme/sys`-style introspection view of per-process state (fds, grants, rings, IOMMU domains, CPU time).
- **Persistent kernel log:** a crash-surviving pstore-style buffer in reserved memory, so panic logs survive a reboot. A panic dumps the last N trace events.
- **Programmable probes (later):** dynamic filters and aggregations that run inside the kernel use the same verified-bytecode machinery as Phase W, eBPF-style, and appear in the config as an opt-in.
- **Overhead budget in CI:** the Phase 0 benchmarks run with the default tracing profile, and CI fails if the "all compiled in, all disabled" build is more than 1% slower than the "compiled out" build, or if enabling the default event set costs more than about 5%.

**Exit:** one storage request can be traced end to end across the app, RedoxFS, the NVMe driver and the IRQ path, from a TOML toggle, with no reboot, and the overhead budgets above hold.

## Phase S: Timers, scheduling and kernel async

The proposal in [issue #177](https://github.com/rexlunae/ferric-oxide/issues/177)
and its issue track defines this phase: a new modular event-driven kernel
scheduler with per-CPU one-shot timers, an initial best-effort EEVDF policy,
an optional admitted partitioned EDF/CBS real-time class, and an optional
kernel async runtime whose workers the scheduler wakes with explicit events —
never a periodic poll. Core class, SMT/cache and NUMA geometry are first-class
inputs to placement and RT admission ([#191], [#192]); the scheduler/timer
files are x86-64 only (32-bit x86 retirement is confirmed). Performance and
latency benefits are hypotheses to measure: the phase opens with a measured
baseline protocol ([#179], [#185]) whose numbers decide rollout.

| Step | Delivers | Depends on |
|---|---|---|
| S1 Baseline protocol | Reproducible periodic-tick measurements: timer IRQ rate, wake jitter, switch cost, waiting-ring worker cost ([#179], [#185]) | T |
| S2 Timekeeping | Independent ClockSource/ClockEvent; per-CPU one-shot timers (TSC-deadline, calibrated LAPIC, HPET fallback) ([#186]) | S1, S6 |
| S3 Scheduler core | Event-driven EEVDF with measured-time accounting; one shared deadline service and rearm protocol ([#187]) | S1, S2, S6 |
| S4 Preemption | Bounded IRQ/preemption-off sections; safe worker preemption points ([#188]) | S1 |
| S5 Async runtime | Portable executor core ([#180]) bound as kernel workers with wake/timer integration ([#181]); nonblocking scheme completion ([#182]); optional `kernel.ring` async backend ([#183]) | S3, S4 |
| S6 Topology | Host-tested core-class/SMT/cache/NUMA model with a versioned snapshot ([#191]) | S1 |
| S7 Placement | Capacity- and NUMA-aware placement and steal domains ([#192]) | S3, S6 |
| S8 Real time | Admitted per-CPU EDF/CBS reservations ([#189]) and their propagation through async work and scheme IPC ([#190]) | S3, S4, S5, S7 |
| S9 Validation | Deterministic interleaving, fault injection and teardown stress ([#184]) | S2–S8 |
| S10 Decision | The P0–P6 factorial comparison and the keep/expand/retire decision ([#185]) | S1–S9 |

**Exit:** on the declared x86-64 profile, a tickless idle system generates zero
scheduler timer IRQs and Pit broadcasts over a controlled 10-second
no-deadline window; the waiting-ring workload shows at least 75% less
execution-overhead memory at 256 rings with no more than a 5% controlled-
hardware throughput/p99 regression; admitted reservations show zero observed
deadline misses over a million releases with bounded dispatch latency; and the
phase publishes the baseline-vs-candidate evidence and an explicit decision.
Real-time service claims stay soft/firm unless a separate schedulability and
platform-interference argument supports them. Design of the runtime:
[Kernel async executor](ferric-async.md).

## Phase 1: Modern baseline and hardening

- Switch the target to `x86-64-v3` across the toolchain, relibc and recipes. Add a `v4` profile. Remove the i586 and BIOS paths.
- **Kernel AVX-512 state support (blocks the v4 profile).** Upstream enabled only the FPU, SSE and AVX components in XCR0, so any AVX-512 instruction in user space faulted. The fix: when the CPU reports AVX-512, set the opmask, ZMM_Hi256 and Hi16_ZMM bits in XCR0. Size per-context XSAVE areas from CPUID leaf 0xD (about 2.7 KB instead of about 1 KB). Use XSAVES/XRSTORS with XSAVEOPT/init-optimization on context switch so threads that never touch AVX-512 pay almost nothing. Report the features through relibc/`getauxval`-style hwcaps for runtime dispatch. Signal frames must carry the larger state too. Test: an AVX-512 stress test across context switches and signals on Zen 4 and in QEMU with `-cpu` AVX-512 models. AMX tile state (XFD-based lazy enable) uses the same mechanism later.
- Make the boot-time CPU feature check fail closed with a clear message on older CPUs.
- **Kernel:** KASLR (bootloader relocation plus a randomized physmap base) and Spectre v1 hardening (`lfence`/`array_index_nospec` on syscall table and user-index paths). KPTI is a toggle, off by default on CPUs that report themselves not vulnerable to Meltdown (RDCL_NO), which covers modern AMD and recent Intel. CET IBT and shadow stacks in the kernel go in where supported.
- **User space:** PIE by default, ASLR in `ld_so`/`redox-rt` for stack, heap, mmap and executable base, user CET shadow stacks, stack probes and guard pages.
- **Code assurance:** move the kernel Clippy lints (`arithmetic_side_effects`, `indexing_slicing`, `cast_ptr_alignment`) from warn to deny, run continuous syscall and scheme-protocol fuzzing (syzkaller-style), and track `unsafe` counts in CI.

**Exit:** the book's feature table can honestly say "Exploit mitigations: Yes", and the fuzzers run nightly with no open crash bugs.

## Phase 2: I/O architecture (the core design work)

### Starting point (upstream, October 2026)

Redox already does more than plain read/write on fds. Kernel-to-scheme IPC runs over submission/completion queues (`Sqe`/`Cqe` in `crates/kernel/src/scheme/user.rs`) with direct context switching. Schemes can hand back shared mappings (`ProvideMmap`, `fmap` with `MAP_SHARED`) and pass fds between processes. User-space drivers map device memory and translate virtual to physical addresses for DMA through the `memory` scheme (`physmap` and `translation` handles in `crates/kernel/src/scheme/memory.rs`). "Redox Rings" (Aug 2026) adds an io_uring-style API for applications.

The real bottlenecks at the start:
1. Every request still goes through the kernel twice: app to kernel to driver daemon, then back.
2. Data gets copied between app buffers, the filesystem daemon and the driver.
3. **The kernel didn't program any IOMMU.** User-space drivers hand raw physical addresses to devices, so a buggy or compromised NVMe or NIC driver can DMA over any memory. That undoes the isolation the microkernel is supposed to provide.

### Design items

**2.1 IOMMU support (prerequisite for everything else).** Add VT-d and AMD-Vi drivers in the kernel, with one DMA domain per driver process. The `memory` scheme's `translation` handle becomes "map this buffer into my device's IOVA space", so drivers never see host physical addresses. Interrupt remapping goes in too. With this, a user-space driver really is unprivileged: it can touch only its own device and the buffers it has been granted.

**2.2 Registered buffers and zero-copy grants.** An application registers a memory region once through a ring. That region can be granted, read-only or read-write and revocably, to the scheme serving a request. The filesystem can forward the grant to the block driver, which maps it into the device's IOMMU domain, so NVMe DMA lands directly in the application's buffer. The fd stays the capability that authorizes the operation. Only the bytes skip the copies.

**2.3 Rings end to end.** Generalize Redox Rings so every scheme (filesystem, block, network, GPU) speaks the same SQ/CQ format, and requests can be chained across schemes (app to RedoxFS to NVMe) without a trip back to the app. Notification options, picked per ring: an eventfd-style wakeup, polled mode on a dedicated core (SQPOLL equivalent), or `UMWAIT`/`TPAUSE` (WAITPKG) on shared doorbell words where the CPU supports it. The steady-state fast path then needs no syscalls.

**2.4 Per-core queues and interrupt steering.** Use the new MSI-X multi-vector support to give each core its own NVMe queue pair and NIC RX/TX queue, steer IRQs to the core that owns the ring, and keep the whole path on one NUMA node.

**2.5 Kernel-bypass networking.** Applications get an AF_XDP-like scheme: a NIC queue mapped directly into a process through the IOMMU, for packet-processing workloads. The normal TCP/IP stack stays as it is for everything else.

**2.6 Shared page cache.** RedoxFS exposes file pages as shared mappings, so `mmap` and buffered reads hit cached pages with no IPC at all. This is how monolithic kernels win on small cached reads.

### Drivers in global code, but unprivileged

The aim is driver-in-kernel performance (no address-space switch, no IPC) without giving the driver kernel privileges. There are three options:

| Option | How it isolates | Cost and risk |
|---|---|---|
| **A. Protection-key domains (PKS)** | The driver runs in ring 0 in the shared kernel address space, but its pages carry a protection key. Switching domains is one `WRPKRS` (a few tens of cycles) instead of a page-table switch. Kernel memory is unreadable from the driver's domain, and DMA is still fenced by the IOMMU (2.1). | Needs Intel Sapphire Rapids-class PKS. AMD has user PKU but no PKS, so this needs a fallback. Code running in ring 0 can still execute privileged instructions, so it must be combined with B. |
| **B. Language-based isolation** | Drivers are `#![forbid(unsafe_code)]` crates linked against a narrow, audited driver API (MMIO accessors, DMA buffers, IRQ handles as capability types), in the style of Theseus/Singularity. The compiler enforces the boundary. | Rules out drivers that need `unsafe`. Any soundness bug in the API or compiler breaks isolation. |
| **C. Verified bytecode** | Drivers are WASM or eBPF-like code that is verified at load time and JIT-compiled into the kernel. | Portable and strongly sandboxed, but adds a large JIT to the TCB and runs slower than native code. |

**Recommendation:** keep user-space drivers as the default. Add a **kernel-colocated tier** for a few hot drivers (NVMe, NIC, possibly the timer and IRQ paths) that combines **B and A**: safe-Rust-only driver crates, plus PKS domains where the hardware has them, always behind the IOMMU. The same driver crate builds both ways, and the config system chooses per driver, so you can benchmark colocated against user-space on real workloads and keep the tier only where it pays off. Phases 2.1 to 2.4 come first. Zero-copy rings may close most of the gap, in which case the colocated tier stays small.

**Exit:** with IOMMU enforcement on, fio 4K random read at QD32 on NVMe reaches at least 80% of Linux io_uring on the same disk, and a driver crash or rogue DMA can't corrupt the kernel or other processes (tested by fault injection).

## Phase V: Hardware virtualization and enclaves

**Why it comes this early:** the project wants isolated enclaves for bringing over C/C++ code written for operating systems with different security models. Instead of porting everything to relibc and the capability model first, the code can run unmodified inside a lightweight VM, and the VM gets exactly the capabilities the host chooses to give it. That makes the hypervisor a porting tool, not just a server feature, so it now comes right after the I/O groundwork. It runs in parallel with Phase 3 and ahead of the hardware and graphics work.

**Dependencies:** Phase 1 (hardening and KASLR, because a hypervisor bug is a host compromise), Phase 2.1 (IOMMU, for device passthrough and interrupt remapping), Phase 2.2–2.3 (shared-memory rings, which virtio queues map onto directly) and Phase T (VM-exit and enclave tracepoints). It also pulls the capability work forward: the minimum is VM creation and every resource given to an enclave as explicit capabilities. Phase 4 then generalizes that.

### Design

- **Hypervisor core in the kernel:** Intel VMX with EPT and AMD SVM with NPT, both standard on v3-class CPUs, plus VPID/ASID, posted interrupts and an x2APIC virtualization fast path. Only the parts that must run at the highest privilege level go in the kernel. That means VM entry/exit, nested page tables and interrupt injection.
- **VMM in user space:** a Rust VMM in the style of Firecracker or cloud-hypervisor (rust-vmm crates) runs as an ordinary sandboxed process holding a `vm` capability fd. A VMM crash or exploit is then contained to one process, as with drivers.
- **Enclaves (microVMs) as the porting vehicle:**
  - Boot in milliseconds with a minimal guest: a small Linux kernel or a library OS/unikernel.
  - The enclave sees only virtio devices that map onto host schemes it was granted: virtio-fs to one directory, virtio-vsock to specific sockets, virtio-net to a filtered network namespace, and virtio-blk to one LV (from the Phase 3 LVM support).
  - Virtio queues are backed by the Phase 2 shared-memory rings, so data moves zero-copy between enclave and host services.
  - The guest keeps its own security model (Linux users, SELinux, whatever the code expects), while the host enforces the outer boundary with capabilities.
  - A manifest in the TOML config declares each enclave's image, memory, CPUs and granted capabilities, and `contain` can launch an enclave the way it launches a sandboxed process.
- **Driver domains (bonus):** the same mechanism can run a Linux driver (Wi-Fi, GPU) inside a VM with the device passed through the IOMMU, exporting a scheme back to the host, the way Qubes and Xen use driver domains. That adds a third option to the native-vs-LinuxKPI driver decision (Phases 5 and 6): reuse Linux drivers unmodified while keeping their GPL code in a separate guest, outside the MIT kernel.
- **Confidential computing (later, optional):** AMD SEV-SNP and Intel TDX for enclaves whose memory even the host can't read.

**Exit:** a stock C/C++ program built for Linux runs in an enclave that boots in under 200 ms. It reaches only the files and sockets in its manifest, and moves data through virtio-fs and virtio-net within 1.5x of native Linux throughput. A Linux guest runs with hardware acceleration. A VMM crash leaves the host and other enclaves intact.

## Phase 3: Storage and filesystem performance

- Port RedoxFS to rings and registered buffers, with async commit/journaling and multi-queue block I/O.
- Shared page cache (2.6) with readahead and writeback tuned against the Phase 0 benchmarks.
- Add TRIM/discard, and keep full-disk encryption using AES-NI/VAES (v3/v4 paths).
### Linux filesystems and volumes: XFS, ext4, LVM, LUKS

The goal is for Ferric Oxide to share disks with Linux and run on servers that were provisioned the Linux way.

- **A stackable block layer comes first.** This is the equivalent of Linux's device-mapper: a daemon opens a block scheme, transforms it and exposes a new block scheme. Partition tables, LVM, encryption and RAID all become layers. It's built on the Phase 2 rings, so buffer grants pass through every layer down to the NVMe driver and stacking adds no copies.
- **LVM2:** parse PV/VG/LV metadata. Linear and striped volumes come first, then thin pools, snapshots and raid1. Read-only activation lands before metadata writes (create and resize).
- **dm-crypt/LUKS:** LUKS2 and LUKS1 headers, aes-xts-plain64 on VAES/AES-NI, Argon2id and PBKDF2 key derivation, multiple keyslots, and TPM-sealed unlock (tying into Phase 4). Both layouts Linux installers create, LVM-on-LUKS and LUKS-on-LVM, are supported.
- **ext4:** read-write with extents, flex_bg, 64bit, metadata_csum and htree directories, plus JBD2 journal replay and commit. Inline data and fscrypt come later.
- **XFS:** read-write for the v5 format: allocation groups, B+tree extents, CRCs, and log replay and writes. Reflink comes later.
- **FAT/exFAT:** for the EFI partition and removable media.
- **Rollout:** each filesystem ships read-only first. Read-write sits behind a config toggle until it passes a port of the relevant xfstests subset and a cross-check in both directions: Linux's `fsck.ext4`/`xfs_repair` reports clean on disks Ferric Oxide wrote, and Ferric Oxide's checker reports clean on disks Linux wrote.
- **Booting from them:** the bootloader can load the kernel and initfs from ext4 or XFS inside LVM and LUKS, so a standard Linux disk layout can serve as the root.
- **Licensing:** implement from the published on-disk format docs, not by porting GPL kernel code, so the MIT license stays intact. Permissively licensed Rust crates get reused where they're sound.

**Exit:** SQLite, a kernel compile and fio file workloads come within 1.5x of Linux on ext4, for RedoxFS and for Ferric Oxide's own ext4 and XFS. An fsck stress test survives power-cut fault injection. A disk set up by a stock Fedora or Ubuntu installer (LUKS, LVM, then ext4 or XFS) boots and mounts read-write, and Linux's own fsck reports it clean afterward.

## Phase 4: Finish the capability security model

- Replace the remaining `uid != 0` gates in kernel schemes with explicit capabilities: IRQ, physmap, IOMMU domain and port I/O become fds handed out by a device manager.
- Make every system daemon start sandboxed through `nsmgr`/`contain` with a declared manifest of the schemes it needs, the way systemd's sandboxing options or Capsicum work. Finish `chroot` and `O_RESOLVE_BENEATH`.
- Per-app sandbox profiles for desktop apps (the Flatpak-style goal in the book).
- **Boot chain:** UEFI Secure Boot with Ferric Oxide signing keys, TPM measured boot, and FDE unlock sealed to the TPM.
- Write a threat-model document and run an external review once this phase lands.

**Exit:** no kernel scheme branches on uid, every default service runs with a minimal namespace, and the boot chain is measured end to end.

## Phase 5: Hardware for real machines

- **ACPI:** a complete AML interpreter with no hardcoded values, S3 and s2idle suspend, CPU frequency and C-states (amd-pstate/intel_pstate equivalents), battery and thermal support. Without power management, laptops aren't usable.
- **Storage and networking at scale:** multi-queue NVMe, Intel `ice`/`ixgbe`, Realtek 2.5G and multi-queue virtio-net.
- **USB:** complete xHCI, USB 3 hubs, mass storage, audio class and UVC.
- **Wi-Fi and Bluetooth:** this is the largest effort. Decide early between native Rust drivers (start with Intel AX2xx/BE2xx, the most common chips on modern laptops) and a **LinuxKPI-style compatibility layer**, which is how FreeBSD runs Linux's `iwlwifi` and DRM drivers. The compatibility layer is faster to ship but brings in C and GPL code. Its licensing has to be decided first, because the fork is MIT.
- I2C-HID touchpads, plus HDA and SOF audio on modern laptops.

**Exit:** both reference machines install, suspend and resume, run on battery with sane power draw, and connect to Wi-Fi.

## Phase 6: Graphics and desktop

**Direction adopted 26 October 2026:** use Wayland, Smithay, Mesa and Xwayland.
Graphics implementation is deferred until the current PR backlog has been
merged; the prerequisites in Phases 2 and 5 still apply. Existing experimental
recipes are groundwork, not evidence of a working accelerated desktop.

- **Display and compositor:** use [Wayland](https://wayland.freedesktop.org/)
  and standard desktop protocols, with [Smithay](https://github.com/Smithay/smithay)
  as the Rust compositor foundation. Keep Ferric-specific integration in the
  platform backend and window-management policy. Smithay is a library, so a
  complete desktop shell remains future work; COSMIC is no longer the selected
  desktop.
- **Rendering:** use [Mesa](https://docs.mesa3d.org/) for EGL, OpenGL/OpenGL ES
  and Vulkan. Software rendering remains useful for bring-up and diagnostics,
  but does not satisfy the hardware-acceleration milestones.
- **VMs first:** bring up virtio-gpu with VirGL for OpenGL, then Venus for
  Vulkan, on a supported QEMU/KVM host. Verify that rendering uses the host GPU.
- **Bare metal:** provide a DRM/KMS-compatible user-space interface and the
  native GPU driver support Mesa needs, starting with one supported GPU at a
  time. Modesetting, GPU memory management, command submission, buffer sharing
  and synchronization are part of this work; porting the compositor alone does
  not enable acceleration. Keep drivers isolated and license-compatible.
- **Shared buffers:** use GPU driver fds as capabilities, with cross-process
  buffer sharing built on the Phase 2 grant mechanisms. Add the graphics-specific
  buffer formats, ownership and synchronization fences needed by Mesa and the
  compositor; the existing grants are not a complete dma-buf implementation.
- **Application compatibility:** support native Wayland toolkit applications
  and [Xwayland](https://wayland.freedesktop.org/docs/book/Xwayland.html) for
  legacy X11 clients. Applications and toolkits still need target ports or a
  separate execution environment; display-protocol support is not Linux binary
  compatibility.

**Orbital is deprecated.** It is not planned to be brought up to a working or
supported state; existing code and recipes may remain temporarily as legacy
artifacts, but are not a compatibility commitment. New graphics investment
goes into the stack above. Removing Orbital or changing image defaults is a
separate future change.

**Exit:** a Smithay-based Wayland session with hardware-accelerated composition
and Vulkan on both reference machines, native Wayland toolkit applications and
X11 clients through Xwayland, and a 4K desktop running at the display's refresh
rate. Verify GPU use separately from software fallback.

## Phase 7: Server

- **As a guest:** paravirtual clock, virtio-everything and multi-queue support, plus fast boot for the "Hosted" variant.
- **Hosting:** the Phase V hypervisor and enclaves are already in place. This phase adds live migration, snapshots and a management API for running many VMs.
- **Server image:** containers via `contain` plus namespaces, enclaves for workloads that need Linux, a stable OCI-ish runtime, and SSH, nginx, PostgreSQL and Rust toolchains in the package repo.

**Exit:** the server image passes the Phase 0 network benchmarks within 1.5x of Linux, and it runs a mixed set of native, wasm and enclave services under one manifest.

## Phase 8: The "usable" bar and releases

There are two release milestones:
- **M1 "Server/VM usable":** Phases 0–4, Phase P (through P5), Phase T and Phase V (hypervisor and enclaves), plus guest virtualization. An installable image runs real services in a VM with hardening and sandboxing on by default.
- **M2 "Workstation usable":** M1 plus Phases 5–6. It installs on the reference laptop and desktop, gives an accelerated COSMIC desktop with Wi-Fi, suspend and a browser, and builds itself (self-hosting).

Both milestones need versioned releases with signed images, a signed package repository with an update channel, release notes and an upstream-sync policy.

## Phase W: A WebAssembly userland (wasm32 default, wasm64 for large programs) with a kernel-hosted compiler

### Reading of the requirement

Most user programs ship as WebAssembly modules rather than native ELF. **Decided 17 Oct 2026: wasm32 is the default.** On 64-bit hosts it needs no explicit bounds checks, because a 32-bit index plus offset can't reach past an 8 GiB reserved region, and out-of-bounds accesses fault on unmapped guard pages. **wasm64** (memory64, standardized in Wasm 3.0) is reserved for programs that need more than 4 GiB of address space, such as large databases, compilers and data tools. The kernel, or a component the kernel trusts, validates each module and compiles it to native code for the exact CPU it runs on. Code generation is ahead-of-time at install or first run, with a cache, plus tiered JIT for hot code. System calls become wasm imports that map onto schemes and fds. The kernel, the compiler and a small set of native exceptions stay native.

Prior art: Microsoft's Singularity (verified managed code in one address space), Nebulet (a Rust microkernel that ran wasm in ring 0, now abandoned) and Theseus (language-isolated Rust OS). Nobody has shipped a general-purpose POSIX desktop this way, so this is the most research-heavy item in the roadmap.

### Why it's attractive

- **Software-isolated processes.** A validated wasm module can only reach memory and imports it was given. That allows two modes, chosen per process by the config system:
  - **Isolated:** each instance runs in its own address space, exactly like a native process today. This mode keeps portability and specialization but saves no switching cost.
  - **Colocated:** trusted-enough instances share one address space, so system calls and IPC become function calls with no page-table switch or TLB flush. This is the same win the colocated driver tier is after. Option C in the driver table becomes this same mechanism, so drivers and apps share one sandbox technology.
- **Capabilities by construction.** A wasm module has no ambient authority. It can only call the imports it's given, and fds map naturally onto wasm resource handles. That fits the nsmgr/capability model better than POSIX ELF does.
- **The v3/v4 question goes away for wasm code.** One binary per package runs everywhere, and the compiler targets the actual CPU (AVX-512 on Zen 4/5, AVX2 on Intel laptops, NEON/SVE on ARM). Native exceptions still follow the v3 baseline.
- **Portability:** the same packages run on x86-64 and aarch64.

### Costs and risks

| Issue | Detail | Mitigation |
|---|---|---|
| **wasm64 bounds checks** (affects only the large programs that opt into wasm64) | wasm32 avoids bounds checks with guard regions. wasm64 can't, so every load and store needs a check, and browser engines have reported memory64 code running roughly 10% to 2x slower than wasm32. | wasm32 is the default, so most code never pays this. For wasm64: cap each memory at a power-of-two size and **mask** the index with one AND instead of branching (this also blocks Spectre-style speculative out-of-bounds reads), and hoist checks out of loops. |
| **4 GiB limit and address-space use** | A wasm32 program can't grow past 4 GiB. Each wasm32 instance reserves about 8 GiB of virtual space, so a 47-bit address space fits roughly 16,000 instances in colocated mode. | wasm64 for big programs. Use 5-level paging (LA57) on CPUs that have it if instance counts ever get close to the limit. |
| **SIMD width** | Wasm SIMD is 128-bit (plus relaxed-simd). Code written for AVX2/AVX-512 loses width, and auto-widening by the compiler is limited. | Keep SIMD-heavy code native (codecs, Mesa, crypto) or expose host-native kernels to wasm as imports. |
| **Compiler in the TCB** | A JIT in ring 0 means a miscompile or validator bug is a kernel compromise. Cranelift, the obvious backend, has had sandbox-escape CVEs. | Run the compiler as an **unprivileged user-space service**. The kernel keeps only a small verifier that checks the native output's sandboxing invariants (VeriWasm-style) before mapping it executable. Cache and sign AOT output. A full in-kernel JIT can be a config toggle for experiments. |
| **Spectre in colocated mode** | Software isolation in a shared address space is vulnerable to Spectre v1/BTB attacks between instances. | Colocate only mutually trusting components (one app's helpers, or a service and its drivers). Mutually distrusting apps get separate address spaces. Swivel-style hardening in codegen. |
| **POSIX gaps** | WASI and wasm lack `fork`, signals, `setjmp`/`longjmp` (now possible via exception handling), dynamic loading, `mmap` of files into linear memory, and generating code at runtime. | A Ferric Oxide wasm ABI on relibc that implements fork as spawn plus state copy, and signals as async imports. Programs that need these and can't adapt stay native. |
| **Programs that must stay native** | Anything with its own JIT (browsers, JavaScript and JVM runtimes, LuaJIT), Mesa and GPU drivers, the wasm compiler itself, and probably heavy databases and compilers at first. | An explicit allowlist of native packages, with the ELF loader kept forever. |
| **Toolchain** | Rust's wasm32 targets are mature, `wasm64-unknown-unknown` is tier 3, and C and C++ work through clang for both. Ports need a Ferric Oxide target and relibc support either way. | Add `wasm32-ferric` (primary) and `wasm64-ferric` targets to the toolchain and cookbook. A recipe declares `wasm32`, `wasm64` or `native`. |

### Persistent look-aside code cache

Compiled native code is kept in a system-wide, persistent cache that sits beside the compiler. A load looks in the cache first and calls the compiler only on a miss, so a program normally gets compiled once per machine rather than once per run.

- **Key:** the module's content hash, the compiler version, the exact CPU feature set it was compiled for (v3, v4 or a specific feature mask), codegen options (bounds-check strategy, Spectre hardening level, tier) and the import ABI version. Any change in these is a miss, so a CPU swap or a compiler upgrade can never load stale code.
- **What gets stored:** relocatable native code plus metadata (trap tables, stack maps, a hash of the verifier's result). Tier-up profiles are kept with the entry, so a hot program starts at its optimized tier on the next run. That amounts to persistent profile-guided optimization.
- **Shared pages:** cache entries are mapped read-only and executable, and every instance of a program shares one copy of the code. Memory use then works like shared libraries rather than one copy per JIT run.
- **Integrity (the main risk is cache poisoning):** only the compiler service can write to the cache. Each entry carries a MAC from a key held by the kernel (sealed to the TPM where there is one), and the kernel checks the MAC, plus the verifier result for entries it hasn't seen since boot, before mapping code executable. A tampered entry is treated as a miss and recompiled. The cache is a capability-gated scheme that apps can't name directly.
- **Filling it:** lazily on first run, by the package manager at install time (AOT), and in images. Release images can ship pre-filled caches for the reference CPUs, so first boot doesn't stall on compilation.
- **Eviction:** LRU with a size cap set in the TOML config. Entries for uninstalled packages or old compiler versions get collected in the background.
- **Toggles:** cache on/off, read-only mode (for immutable or verified images), size cap, and whether persisted profiles are used.
- **Observability:** hits, misses, compile times and evictions are Phase T tracepoints.

### Interaction with the rest of the roadmap

- **Security (Phases 1 and 4):** wasm adds a second isolation layer on top of the capability model rather than replacing it. Hardening still applies to native code and to the compiler service. KASLR matters more in colocated mode, since all instances share one kernel layout.
- **I/O (Phase 2):** a wasm instance sees one linear memory, so Phase 2.2 grants must map into a window of that memory. Pages are pinned and IOMMU-mapped for DMA, and the multi-memory feature can give each instance a separate "I/O memory." Rings live in shared memory imported by the instance. The IOMMU work (2.1) is still required, because wasm isolation doesn't constrain devices.
- **CPU baseline:** the kernel and native exceptions stay on v3 with an optional v4 build. Wasm code gets per-CPU specialization automatically.
- **Config system:** per-package native vs. wasm builds, isolated vs. colocated mode, JIT tiers, and in-kernel JIT vs. compiler service all become toggles.

### Steps

1. **W1 Runtime:** run wasm32 CLI tools (and a wasm64 test program) under a user-space runtime (a Wasmtime/Cranelift port) with a Ferric Oxide import ABI mapped to schemes. Benchmark against the same tools built natively.
2. **W2 Toolchain:** `wasm32-ferric` and `wasm64-ferric` targets, relibc for wasm, and dual-build recipes. Port the base CLI tools and simple daemons.
3. **W3 Compiler service and loader:** a system compiler service, the persistent look-aside code cache (with MACs and persisted tier-up profiles), CPU-specialized codegen, and an in-kernel verifier that maps verified code into isolated processes.
4. **W4 Colocated mode:** a single-address-space execution domain for trusted groups, with syscalls as direct calls. Benchmark IPC and syscalls against Phase 2 rings.
5. **W5 Migration:** move base daemons and userland packages to wasm by default, keeping the native allowlist.

**Decision gate after W4:** expand only if wasm32 apps run within about 15% of native (and wasm64 apps within about 30%) on the Phase 0 suite and colocated mode measurably beats native IPC. Otherwise keep wasm for portable and sandboxed apps and stay native by default.

**Exit:** a default install where most packages are wasm32, with only the large programs on wasm64, isolation and colocation are both toggleable, and the native allowlist is documented and small.

## Dependency summary

```
Phase 0 ─┬─ Phase P (packages: P1–P3 part of Phase 0 exit, P4 after W2/W3 + v4, P5 after Phase 4 → M1)
         ├─ Phase T (tracing, feeds all benchmarks)
         ├─ Phase 1 (hardening) ──────────────────────┐
         ├─ Phase 2.1 IOMMU ─ 2.2–2.3 ─┬─ Phase V (hypervisor, enclaves) ─┐
         │                             ├─ 2.4–2.6 ─ Phase 3 (storage) ────┼─ M1 ─┐
         │                             └─ Phase 4 (capabilities) ─────────┘      ├─ M2
         └─ Phase 5 (hardware) ─ Phase 6 (graphics) ─────────────────────────────┘
            (driver domains from Phase V are an option here)
         Phase 7 (server) builds on Phase V ── M1/M2
         Phase 2.2 grants ─ Phase W (wasm32/wasm64 userland, W1–W5) ── default for M2 if the gate passes
```

## Open decisions

These are still open. Proposals belong in an issue; the maintainer makes the
call, and the answer is recorded here.

1. **Native vs. compatibility-layer vs. driver-domain drivers** for Wi-Fi and GPU (Phases 5 and 6). Driver domains (Linux drivers in a Phase V VM) are a third option. This is mostly a licensing and time trade-off.
2. **Whether PKS-only hardware is an acceptable requirement** for the colocated driver tier, or whether AMD machines get language isolation alone.
3. **The default root filesystem:** RedoxFS (native, copy-on-write) or ext4/XFS on LVM and LUKS. The current proposal keeps RedoxFS as the default and gives the Linux filesystems full support.
4. **Where the wasm compiler lives:** in ring 0, or as an unprivileged service with a small in-kernel verifier. The current proposal is the verifier design, with an in-kernel JIT as a toggle.
5. **Whether colocated (single address space) mode is a goal** or only an experiment, given Spectre between apps that share an address space.
6. **Packaging (Phase P):** the working defaults listed under Phase P stand until the maintainer decides otherwise.

## Keeping this chapter current

Change this chapter when the plan changes: a phase is added, reordered or
re-scoped, an exit criterion moves, or an open decision is made. Progress
against the plan goes in the status table in `ROADMAP.md`, not here.
