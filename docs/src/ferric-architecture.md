# Architecture and component boundaries

Ferric Oxide retains Redox's division between a kernel that manages execution,
memory and communication, and user-space services that provide filesystems,
devices and higher-level facilities. Applications use Redox system calls and
relibc rather than a Linux kernel ABI. Linux application compatibility is
developed through libraries and ports; it does not make this a Linux kernel.

## Source and build boundaries

| Location | Responsibility and build boundary |
| --- | --- |
| Root `Cargo.toml` | Host workspace for `tools/` and host-testable shared crates; root `Cargo.lock` controls these builds |
| `tools/cookbook/` | Recipe fetching, dependency builds, staging, packaging and the `repo` command |
| `tools/ferric-build/` | Native rustup/Clang setup, image build and QEMU orchestration (`cargo xtask`) |
| `tools/ferric-config/` | Feature selection and build configuration generation |
| `tools/installer/` | Image configuration parsing and installation; shared by host tooling and the target installer recipe |
| `tools/redoxer/` | Component execution/build support in a Redox environment; its example is standalone |
| `tools/fox/` | Package verification (including build attestations), installation from archives, signed git sources or registries, trust store, install policy, `fox audit`, package and system SBOMs, and `fox sync` with `fox.lock` |
| `tools/ferric-trace/` | Kernel trace captures: saving one on the target (recipe `recipes/tools/ferric-trace`), and a text view and Perfetto/Chrome trace JSON export on the host |
| `tools/cargo-fox/` | `cargo fox`: hermetic package builds, signed archives, attestations and signed release tags for package authors |
| `tools/fox-registry/` | `fox-registry`: creating a package registry, admitting packages, recording releases, checking them, building and recording prebuilt artifacts as a registry builder, publishing advisories, importing cookbook recipes, and signing the registry metadata; `init --github` writes the CI template |
| `crates/kernel/` | Independent `no_std` kernel workspace, custom target specifications, linker scripts and its own lockfile |
| `crates/bootloader/` | Standalone bootloader, including the UEFI target and optional kernel relocation |
| `crates/relibc/` | C/POSIX library and `redox-rt` process/thread runtime, with its own workspace |
| `userspace/base/` | Workspace of essential services and drivers; `bootstrap/` builds separately |
| `userspace/redoxfs/` | RedoxFS library, target filesystem service and host filesystem tools; built separately |
| `recipes/`, `config/`, `mk/` | Package definitions, installed image contents and Make orchestration |
| `docs/` | This mdBook, including inherited Redox chapters and the Ferric system chapters |

Shared libraries live next to the upstream components under `crates/` and are
consumed by path from each build. Being a root-workspace member for host tests
does not turn a library's kernel consumers into host programs. The kernel,
UEFI and Redox user-space builds keep their own targets, profiles and dependency
resolution. There is no `tools/redoxfs-host/` crate in the current tree: host
RedoxFS tools come from `userspace/redoxfs/` through the recipe/fstools path.

`crates/aligned-buffer/` is a shared `no_std` library using `alloc` for owned,
zeroed byte buffers with explicit alignment. The kernel uses it for saved CPU
state. It contains no kernel mappings or scheduling policy. The imported `rmm`
and `redox_syscall` packages remain under `crates/kernel/`: their existing paths
and workspace boundary are retained for upstream integration.

Inside the kernel, `cpu_set` owns logical CPU publication and affinity masks;
`sync/records` owns the initialized record representations and whole-record
copy/dequeue protocol used by `WaitQueue`. Interrupt, logging, allocation and
trace macros use explicit paths rather than `macro_use` scope propagation.
Boot messages identify Ferric Oxide; the `Redox` uname system identifier,
system-call ABI and target names remain for compatibility with existing tools.

`crates/devicetree/` provides allocation-free, borrowed hardware-description
access for both the kernel and bootloader through the published `fdt` parser.
It checks header block extents and tree structure before traversal, preserves
console aliases/parameters and phandles, and bounds address/size cell decoding.
Its MMIO translation follows every ancestor bus and rejects range crossings and
overflow. The kernel retains its independent reservation-map and reserved-memory
policy checks. This does not replace drivers or validate every device binding.

`crates/kernel/`, `crates/bootloader/`, `crates/relibc/`, `userspace/base/`,
`userspace/redoxfs/` and `docs/` were imported from Redox OS as git subtrees,
so their history is preserved, and Installer and Redoxer as snapshots. None of
them is synced from Redox: they are maintained here, and a Redox change is
ported by hand when it is wanted. relibc's `openlibm` and
`dlmalloc-rs` remain nested submodules, registered in the root `.gitmodules`.

## From firmware to user programs

1. UEFI starts the bootloader in `crates/bootloader/`. It obtains firmware
   memory/hardware information, finds the RedoxFS boot partition, and loads the
   kernel and initfs. Live images can load the filesystem into memory.
2. The loader establishes the initial mappings and enters the kernel. With
   kernel-image KASLR selected, it first applies the retained ELF relocations
   and loads the kernel at a randomized virtual address.
3. Architecture startup and `crates/kernel/src/startup/` establish kernel
   memory, CPU state, interrupts, scheduling and initial scheme handles. The
   kernel starts the bootstrap executable embedded with initfs.
4. `userspace/base/bootstrap/` publishes the initial kernel schemes, exposes
   initfs and starts `init`. It recognizes the optional `kernel.iommu` and
   `kernel.regbuf` and `kernel.trace` scheme IDs through the shared ABI crates.
5. `userspace/base/init/` runs the initfs configuration. Services and storage
   drivers make the root filesystem accessible; RedoxFS supplies the file
   scheme. Filesystem init scripts under `/usr/lib/init.d` start the remaining
   services and the configured console or desktop.

Image composition determines the exact daemons and scripts. Inspect
`config/base.toml`, the chosen `config/<name>.toml`, and included configuration
files before assuming a service is installed. The smoke-test image replaces
the console stage with its test script.

On x86, legacy port-I/O debug UARTs are probed before memory initialization.
With `kernel.serial_debug` and `kernel.lpss_debug` enabled, the LPSS MMIO probe
runs after the kernel installs its page tables and initializes the frame
allocator, before application processors start. It uses the final physical-map
address. Mapping it during early serial setup would attempt to insert a 4 KiB
mapping into the bootloader's huge-page mapping, before the allocator is ready.

## Schemes, descriptors and services

A scheme is a named service accessed through file descriptors. Scheme-rooted
paths such as `/scheme/kernel.regbuf/region` identify a service and a resource
within it. Namespaces determine which schemes a process can reach. The kernel
implements some schemes itself; user-space schemes receive requests through
the kernel and return results to their clients.

`userspace/base/` contains `init`, `logd`, `randd`, `netstack`, audio services,
PCI discovery and drivers for storage, networking, USB, input and graphics.
`pcid` enumerates PCI devices and supplies the driver interface;
`pcid-spawner` starts matched drivers. DMA-domain transfer and interrupt
message allocation are integrated into that interface.

Descriptors also act as capabilities for the new buffer and DMA interfaces:
a registered-buffer region controls its grants, a grant gives bounded access
to memory, and a DMA-domain descriptor allows operations subject to binding
and ownership checks. Root privilege currently controls access to the IOMMU
attach capability; a general replacement capability system is not implemented
by this interface.

See [Schemes and Resources](schemes-resources.md) and
[Scheme Operation](scheme-operation.md) for the inherited communication model,
and [Registered buffers and IOMMUs](ferric-io.md) for the new interfaces.

## Execution and memory ownership

The scheduler in `crates/kernel/src/context/switch.rs` implements EEVDF
(earliest eligible virtual deadline first). It tracks virtual execution time
and deadlines, chooses eligible runnable contexts, and accounts for CPU
eligibility and an existing time slice. It also supports direct context
switches for scheme requests. [Scheduling](scheduling.md) describes it and
the planned tickless, asynchronous and real-time work.

A direct scheme handler can run on its client's timeslice. Keeping that
timeslice does not mean keeping the handler running: a client that becomes
runnable during selection must regain execution when the handler parks.
`context/handoff.rs` carries the retained or replacement owner guard through
this decision, including the address-space handoff. Discarding a retained
owner used to let a still-blocked NVMe handler return from its event wait
with a spurious `EINTR` ([#308](https://github.com/rexlunae/ferric-oxide/issues/308)).
Only the actual idle context reports an idle or contended scheduler return;
a retained running user context continues its work.

Timed waits are tracked by `context/timeout.rs`; direct switches preserve
their deadlines so blocked scheme handlers can still wake. Futex wait setup
and wakeups synchronize through the futex table in `syscall/futex.rs` so a
wakeup cannot pass between the comparison and waiter registration. Expired
waiters cannot consume a later wake intended for another waiter.

A wake-up for a context that last ran on another CPU goes on that CPU's
cross-CPU wake-up list, and the waker sends the wake-up IPI to that CPU only
(`context::wakeup_context`; contexts the scheduler hands to a CPU their
affinity allows go the same way). Upstream broadcast the IPI to every other
CPU, so each cross-CPU wake-up interrupted every CPU, and every CPU drained
its own list as a side effect of other CPUs' wake-ups. Ferric Oxide's
`schedule` drains the CPU's cross-CPU list too, so a wake-up whose IPI is not
delivered (the target's APIC ID not yet recorded, or a port without IPIs) runs
at that CPU's next switch rather than waiting for a later wake-up. The IPI does
not preempt a busy CPU: the woken context waits for that CPU's next switch,
which today is at most one scheduler tick away.

### Timers and timekeeping

Without `kernel.time.one_shot` (the default), time, timeouts and preemption
are driven by the periodic PIT (or HPET) interrupt as upstream: the BSP's
handler advances `time::OFFSET`, broadcasts the `Pit` IPI and runs
`timeout::trigger` and `switch::tick`, and the monotonic clock is `OFFSET`
plus an interpolation of the current period. Tickless scheduling (issue #177)
replaces that in steps; the first is `kernel.time.one_shot` (off by default).

With the option, the BSP calibrates the TSC at boot against the KVM clock, the
HPET main counter (32- or 64-bit) or PIT channel 0, over five short windows
whose median it takes, refusing windows that disagree. The monotonic clock
(`time::monotonic`) then comes from a *continuous* source selected at the same
point (`arch/x86_shared/time/continuous.rs`): the KVM paravirtual clock when
built in and available, else the calibrated invariant TSC, else the HPET
counter, else the previous OFFSET-plus-interpolation path (reported, and the
only case where the clock depends on the periodic PIT interrupt). The clock
keeps advancing while the periodic interrupt is masked, and the realtime
offset contract is unchanged. Every read publishes its value with one atomic
`fetch_max` on a shared watermark, so a CPU delayed between reading the source
and publishing cannot move the clock back. If the KVM clock steps backward (its
page re-bases), the clock hands over for good to the next continuous source,
offset so it continues from the last published time; later KVM readings on the
new, lower epoch are ignored rather than freezing the clock until they catch
up (`ferric_timed::monotonic`).

The per-CPU one-shot scheduler timer (`arch/x86_shared/timer.rs`) uses the
LAPIC timer in TSC-deadline mode (Intel SDM) when CPUID reports it: a deadline
in TSC ticks is written to IA32_TSC_DEADLINE, and one MSR write is the whole
reprogram, so the rearm protocol is "recheck the clock after programming".
Without TSC-deadline mode, the CPU measures its LAPIC countdown against the
TSC and arms one-shot countdowns, re-arming one that its 32-bit count ended
early. Deadlines are monotonic nanoseconds, turned into a TSC deadline relative
to the current time when programmed, so they hold whichever clock source was
chosen. The portable arithmetic (ordered `Nanos`/`Duration`/`Deadline` types,
rate conversions, calibration, the backend's programming and the
`ArmedDeadline` rearm cell) lives in the host-tested `ferric-timed` crate.
With the feature on, every CPU arms a series of 24 deadlines from 50 µs to
5 ms and logs how late they fired, and the BSP arms the timer for a second,
arms an already-due deadline and checks a disarmed timer stays quiet. The
BSP's checks run before the bootstrap context is spawned, with interrupts
open for one instruction at a time while waiting: the kernel's log and serial
writers use plain spinlocks, and a PIT tick that preempted the selftest while
it held one would leave any context that logs on that CPU spinning forever.
The periodic PIT path remains the scheduler's event source until the event-driven
scheduler (#187) takes over.

Address-space mappings, sharing and copy-on-write live under
`crates/kernel/src/context/`; physical allocation and page tables are in
`src/memory/` and the kernel's `rmm/`. Registered buffers and DMA retain frame
references until their mappings are safely removed. File-backed private
mappings use owned copy-on-write grants; pages not owned by the allocator
cannot be treated as ordinary private frames to copy.

## Shared implementation modules

| Crate directory | Role |
| --- | --- |
| `crates/nospec/` | `no_std` bounds-index hardening helpers used at kernel trust boundaries |
| `crates/hwrand/` | RDRAND-based layout entropy with an unavailable-source result |
| `crates/elfreloc/` | Safe, host-testable ELF relocation replay for kernel-image KASLR |
| `crates/cet/` | CET capability/policy decoding and shadow-stack layout/token arithmetic |
| `crates/regbuf/` | Registered-buffer ABI, byte-range planning, grant lifecycle, bounce copyback and pin budgets |
| `crates/trace/` | Kernel trace event catalog, record format, per-CPU SPSC ring, event selection and `kernel.trace` scheme ABI |
| `crates/iommu/` | DMA ABI, PCI requester aliases, shared table/ring logic, IOVA allocation and DMA lifecycle bookkeeping |
| `crates/vtd/`, `crates/amdvi/` | Vendor-specific ACPI parsing, table/register formats and invalidation encodings |
| `crates/fox-core/` | Package identity, manifests, trust and signed archive verification |
| `crates/fox-store/`, `crates/fox-sbom/` | Installed package state and reproducible CycloneDX output, including build SBOMs and system SBOMs |
| `crates/fox-git/`, `crates/fox-build/` | Git commits, `fox-tree/1` BLAKE3 tree hashes, exports, fetching and SSH-signed release tags; the hermetic Cargo build, packing and attestation pipeline |
| `crates/fox-index/` | Package registry layout, `fox-tuf/1` signed metadata (a TUF subset), client verification, maintainer-side editing and RustSec-format advisories |

The low-level protection and I/O crates are `no_std`; optional `alloc` layers
hold bookkeeping that needs allocation. `fox-core` is `no_std` with `alloc`,
with filesystem access behind `std`; the store, SBOM, git, build and registry
layers use `std`.
Hardware access stays in kernel integration modules while portable rules can
be exercised on the host.

User-space drivers reach device registers through `common::io::Mmio` and
`common::io::MmioPtr` in `userspace/base/drivers/common/`. On x86 and ARM64 each
access is one inline-assembly load or store of the register's width. On ARM64
that is an `ldr`/`str` with a plain base-register operand, because KVM and
QEMU's HVF backend can only emulate a trapped access whose exception syndrome is
valid. `read_volatile` lets the compiler choose writeback, pair or SIMD
encodings, and HVF aborts the VM on those. Do not access a mapped BAR or ECAM
window with `read_volatile`/`write_volatile` directly.
