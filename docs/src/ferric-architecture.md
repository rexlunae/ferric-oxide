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

The six imported subtrees are `crates/kernel/`, `crates/bootloader/`,
`crates/relibc/`, `userspace/base/`, `userspace/redoxfs/` and `docs/`.
`.github/workflows/subtree-sync.yml` pulls their upstream changes into this
fork and proposes local pull requests. Installer and Redoxer are local snapshots
and are not in that workflow's sync matrix. relibc's `openlibm` and
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
switches for scheme requests. The inherited scheduling chapter still contains
an older round-robin description; use the source for current scheduling work.

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
