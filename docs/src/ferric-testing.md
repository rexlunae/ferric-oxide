# Validation and debugging

Validation spans host rules, kernel binaries and the running OS. A passing
host test does not demonstrate a hardware invalidation or context switch, and
a configured workflow is not a record of a successful run. Report the exact
commands, target and limitations with a change.

## Host and shared-library checks

From the repository root:

```sh
cargo fmt --all --check
cargo test --locked --workspace
cargo test --locked -p ferric-regbuf --all-features
cargo test --locked -p ferric-blkgrant --all-features
cargo test --locked -p ferric-trace --all-features
cargo test --locked -p ferric-iommu --all-features
cargo test --locked -p ferric-wasm-abi --all-features
```

The workspace command includes members such as `ferric-hwrand`, installer
and Redoxer that are not all in `default-members`. Alloc-backed registered
buffer and IOMMU bookkeeping also need their feature-enabled runs. Check
`no_std` configurations separately when changing those libraries.

`.github/workflows/host-tools.yml` is the precise CI command list. It denies
Clippy warnings for the fork's own tools and libraries and builds selected
Rustdoc with `RUSTDOCFLAGS="-D warnings"`. Its selected-package Clippy check uses
`--no-deps` because imported host tools still have upstream lint warnings,
including when they are dependencies of a Ferric integration test. It separately
runs `cargo test --locked --lib` in
`userspace/redoxfs/`; that is not covered by the root workspace test. Its `ext4d`
job tests `userspace/ext4d/` twice, on the committed ext4 images in
`tests/fixtures/` and on ones `make-fixtures.sh` builds with the runner's
e2fsprogs (`EXT4D_FIXTURES`); the generator fixes tested permissions and ownership
independently of the runner's umask and identity. Checks find a file's actual
inode rather than assuming the inode numbers of an older mkfs configuration;
newer defaults such as orphan files and checksum seeds remain enabled.
Each set is tested without and with the journal replay
(`journal-replay`, whose tests compare ext4d's replay with `e2fsck`'s) and with
the journaled writer (`write`, whose tests hand every volume they leave,
including ones crashed part-way through a change, to `e2fsck` through
`EXT4D_E2FSCK`), and lints and documents it for the host and for
`x86_64-unknown-redox` with each of those feature sets. The `ferric-jbd2` step tests, lints and documents the
journal crate, and builds it for `x86_64-unknown-none` to keep it `no_std`.
Its `blockd` job does the same for `userspace/blockd/` (with each
of its layers alone and both, and with `ferric-unlock`'s `unlock` feature), and runs `ferric-blocklayer`'s fixture tests
again on images `crates/blocklayer/tests/fixtures/make-fixtures.sh` makes as
root with the runner's cryptsetup and lvm2 (`BLOCKLAYER_FIXTURES`). Its `fatd`
job tests `userspace/fatd/` on the committed FAT12, FAT16 and FAT32 images and on
ones `make-fixtures.sh` builds with the runner's dosfstools and mtools
(`FATD_FIXTURES`), with and without its `exfat` feature, then fatd's exFAT tests
and `ferric-exfat`'s fixture tests on exFAT images
`crates/exfat/tests/fixtures/make-fixtures.sh` makes as root with the runner's
exfatprogs (`EXFAT_FIXTURES`), and lints and documents fatd both ways for the
host and for `x86_64-unknown-redox`.

The exFAT regressions reject oversized and odd-length up-case streams before
reading their payload, and check generated wire layouts against literal byte
fixtures. CI also checks `ferric-fat-types` on the host and bare metal and verifies
that fatd's default dependency graph excludes `ferric-exfat`.

Host, kernel and image jobs use `.github/actions/prepare-cargo-git`, directly or
through `.github/actions/prepare-submodules`, before running Cargo. It enables
Git's fetch client and caches pinned Git dependencies from the
[Ferric-maintained mirrors](ferric-build.md#dependency-sources), retaining the
lockfiles' exact commit IDs. The submodule setup restores Git metadata and then
materializes the pinned working trees. These are separate prerequisites: relibc's
workspace resolution needs its `dlmalloc-rs` submodule, while Cargo also needs
its registry and Git dependencies.

`cargo test --locked -p ferric-config` includes smoke/fuzz and scheduler-baseline profile regressions
in `tools/ferric-config/tests/profiles.rs`. They exercise the public CLI and
generated Makefile, then load the selected filesystem configuration through the
installer's include resolver. They check inherited source packages, the effective
boot script, image overrides and retention of saved CPU/kernel settings.
`tests/catalog.rs` also checks every direct or conditional kernel Cargo feature
named by the catalog against `crates/kernel/Cargo.toml`, including options that
are disabled by default. The Make/CLI regressions reject the retired `kernel.pti`
setting before writing build files and keep the supported `kernel.cpu.kpti`
policy inactive when selecting ARM64.

The generator tests also switch A → B → A across relative and absolute output
directories sharing one recipe tree. They check targeted and global invalidation,
legacy-state migration, previews that preserve pending rebuilds, and filesystem
failures that retain typed sources and the applied baseline. The Make tests use
the real configuration fragments and a cached feature-stamped artifact to
verify that returning to a previously generated profile actually rebuilds it.

Use `cargo clippy --all-targets -- -D warnings` and `cargo doc --no-deps`
in changed workspaces as required by `AGENTS.md`, accounting for the documented
imported-code exception. For feature-gated APIs, include the relevant feature
combination in tests, lints and docs. Do not hide a new warning by increasing
a baseline or disabling a check.

Package coverage is in `crates/fox-core/tests/archive.rs`,
`crates/fox-store/tests/store.rs`, `tools/fox/tests/cli.rs`,
`tools/fox/tests/git_sources.rs`, `tools/fox/tests/registry.rs`, `crates/fox-git/tests/repo.rs`,
`crates/fox-git/tests/tags.rs`, `crates/fox-build/tests/build.rs`,
`tools/cargo-fox/tests/cli.rs`, `crates/fox-index/tests/registry.rs` and
`tools/fox-registry/tests/cli.rs`. The registry tests sign metadata with
fixed-seed keys and attack it: rollback, expiry, a changed file, a delegation
claiming another path, and a key rotation signed by the new keys only.
`tools/fox/tests/registry.rs` installs by name from a local registry: from
the owner-signed tag, from a prebuilt artifact with too few and then enough
builders, under `build = "never"`, and refuses a moved tag, a wrong root pin
and a tampered blob. `tools/fox-registry/tests/cli.rs` also runs `build` as a
registry builder, unlisted and then listed in `registry.toml`, then again
with `--level v4` as a second artifact (on x86-64 hosts), runs `rebuild`
as an independent rebuilder in another work directory (refused until it is
listed, then reproducing the archive and adding its attestation to the same
artifact, after which nothing is left to rebuild, and attesting again once
its key is replaced), publishes
an advisory, and imports a small recipe tree (a git recipe, a tarball recipe
with a description, and an invalid name). `fox sync` is tested end to end with a registry package (installed,
locked, unchanged under `--locked`, removed when the set drops it, kept when
it was replaced by hand, and a refused `--locked` sync leaving the registries
and policy untouched) and a git package pinned to its commit (a moved tag
fails until `--update`); unit tests check that changing `rev`, features,
`default-features`, target, `path` or `package` invalidates a pin and that
local git sources lock by their canonical `file://` URL, and that the
x86-64 level is part of the request for x86-64 and implicit targets only.
CPU-level selection is unit tested in `tools/fox/src/cpu.rs` (an explicit
`--level`, the running CPU under the policy cap, the policy or baseline for
another root) and `tools/fox/src/prebuilt.rs` (a v4 machine prefers v4 and
falls back to v3, a v3 machine never gets v4, other targets take only
artifacts without a level, and an attestation of another level is refused);
`fox-registry`'s `is_variant` and `fox-build`'s level choice have unit tests
too, and `fox-build`'s rebuild comparison is unit tested for a changed,
missing or extra file, a changed mode and another version. `fox-registry`
unit tests check that a rebuild with any divergent archive fails while
keeping the ones it reproduced, and that an unlisted builder or unreadable
attestation counts as not attested.
The installer's `fox` image step (`tools/installer/src/fox.rs`) is unit
tested with a stand-in `fox` script: adoption alone without a package set,
then adoption, a `--locked` sync for the image target and level and the system SBOM in
that order, a failing sync that stops before the SBOM, and a missing
binary. `ferric-config`'s `package_manager_choice` test checks the make
variables the `packages.*` options and `cpu.x86_64_level` generate. Building and booting an image
through `fox` (`make all`, `make qemu`) is not covered by these tests.
`fox audit` is tested end to end against a registry advisory
(including the install warning, a registry VEX statement that sets the
finding aside, no VEX for a `fox-installed/1` store without recorded
origins, `--no-vex` skipping a broken VEX document, and a changed SBOM),
and in unit tests for VEX matching by id and alias, other versions, an
`exploitable` statement winning over a clearing one, and VEX applying only
to packages whose recorded origin is that registry's name and root; and
against a RustSec-layout directory of advisories for crates inside build
SBOMs, with withdrawn, build-only and non-semver cases. `fox-sbom`'s VEX
reader is unit tested on purl and component references, versions (including
an `affected` version the purl also names) and every
rejected document, and `fox-registry`'s CLI test publishes, refuses and
removes VEX documents and signs the result. Tests construct real signed pkgar fixtures
through `fox-core`'s `test-fixtures` feature. The git and build tests create
repositories with the `git` command and build a small path-only workspace
with Cargo, so they need `git` and the rustup default toolchain but no
network; fetches use `file://` URLs. Release tags are signed in Rust with
fixed-seed Ed25519 keys rather than with `ssh-keygen`, and `git fsck
--strict` checks the tag objects. `fox-git` pins a known `fox-tree/1`
hash. The
IOMMU vendor crates include known QEMU DMAR/IVRS byte fixtures under
`crates/vtd/tests/fixtures/` and `crates/amdvi/tests/fixtures/`.

`ferric-lvm` is used in the image through `ferric-blockd` (`fs.lvm`); its own
host tests cover the formats. `crates/lvm/tests/fixtures.rs` reads PVs
that lvm2 wrote (`crates/lvm/tests/fixtures/README` has the commands): a
two-PV striped group, metadata text that wraps round its area, and a volume
extended after another one. It checks the parsed groups and mappings against
`vgcfgbackup`, flips a bit in every byte the reader depends on, and edits labels
and metadata-area headers to test areas that are empty, broken or past the
device.

`ferric-luks` is used in the image through `ferric-blockd` (`fs.luks`); its own
host tests cover the formats. `crates/luks/tests/fixtures.rs` reads LUKS1
and LUKS2 headers that cryptsetup wrote (`crates/luks/tests/fixtures/README`
has the commands) and checks the recovered volume keys against
`cryptsetup luksDump --dump-volume-key`, header-copy fallback and random
corruption. Because the bootloader is a soft-float UEFI target, CI also builds
it with `RUSTFLAGS="--cfg aes_force_soft"` and `--features force-soft` for
`x86_64-unknown-uefi`; check that build when changing its dependencies.

`ferric-blocklayer`'s host tests (`crates/blocklayer/tests/fixtures.rs`) read
volumes cryptsetup and lvm2 made, each holding a known byte pattern: a logical
volume across two extent runs, LUKS2 inside an LV, an LV inside LUKS2 with
4096-byte sectors (which pins the IV unit; see
[Block layers](ferric-io.md#block-layers-luks-and-lvm-read-only)), and LUKS1
data OpenSSL encrypted, with reads that start and end inside sectors, wrong
passphrases, missing or duplicate PVs, PVs of two groups and a device cut short.
CI checks each feature combination and a bare-metal build.

## Kernel helper tests

Kernel algorithms that do not touch hardware have a host test package at
`crates/kernel/tests/`, sharing the kernel's workspace and dependency lockfile.
It includes the actual `cpu_set`, `sync/records`, TLB shootdown mailbox and
acknowledgements (`memory/tlb.rs`), broadcast global-flush generation
(`percpu/global_flush.rs`) and local APIC ID decoding
(`arch/x86_shared/device/apic_id.rs`) modules, rather than duplicating their implementations.
`tests/tlb_ack.rs` wires the mailbox and the generation together as `percpu.rs`
does and races broadcasts against targeted rounds on threads. Run from the root:

```sh
cargo test --locked -p ferric-aligned-buffer
cargo test --locked --manifest-path crates/kernel/tests/Cargo.toml --all-features
cargo clippy --locked --manifest-path crates/kernel/tests/Cargo.toml \
  --all-targets --all-features --no-deps -- -D warnings
cargo test --locked --manifest-path crates/kernel/Cargo.toml -p rmm --features std --lib
cargo miri test --locked -p ferric-aligned-buffer
cargo miri test --locked --manifest-path crates/kernel/tests/Cargo.toml --all-features
```

Miri requires the `miri` component for the declared nightly toolchain. The
kernel-lints workflow runs these tests, including Miri; the shared buffer also
receives the host workspace's strict Clippy and Rustdoc checks.

The helper suite's default features also import the actual x86 serial module
with mock UARTs and a mapper that rejects calls before memory is ready. The
regression checks that early initialization installs only the port-I/O UARTs,
then the LPSS probe maps and installs its MMIO UART after memory initialization.
The all-features case includes the embedded-controller output; no real hardware
access occurs in these host tests. The image CI configuration enables both
`kernel.serial_debug` and `kernel.lpss_debug`, so its boot matrix also exercises
the real startup ordering on QEMU. Retain the first panic log if startup fails.
This checks initialization order, not physical LPSS support.

Local validation on 31 October 2026 retained a panic from the early LPSS probe:
`map_device_memory` attempted to descend through a huge-page entry before memory
initialization. After deferring the probe, the combined x86_64 `wayland` image
built at integration `d5b2d41c4b11`, with the same LPSS setting and
`kernel.cpu.kpti = "always"`. Its four-vCPU, 4 GiB KVM snapshot boot reported
KPTI enabled, reached login, and passed the 64 MiB pipe byte-count, SHA256 and
file-comparison checks plus the exact Ion smoke fragment.

The included `context/handoff.rs` tests distinguish a timeslice owner from
the physically running scheme handler. They cover a client becoming runnable
after the direct-switch check, retention of the running owner, selection of
an already-running handler, and preservation of the previous/next context and
address-space guards when a switch is needed. These are the actual handoff
decisions used by the kernel; host tests do not execute the architecture switch.

The helper package also compiles the actual ordered-lock and wait-condition
code for empty-list destruction tests, including `drop_panic`. Its host-only
context/shootdown stubs fail if those hardware paths are reached. The regression
holds another lock while destroying an empty condition and verifies no further
lock acquisition occurs. This covers the page-cache cleanup panic found during
an ARM64 native boot; it does not simulate blocked waiters or context switches.

Coverage includes alignment, zero-length buffers, layout overflow and exact
deallocation layouts, independent buffer clones, every CPU-mask word boundary,
out-of-range CPU IDs, nonzero xAPIC and x2APIC IDs, duplicate/gapped/concurrent CPU publication, initialized
event/SQE ABI bytes, wrapped queues, whole-record reads and copy-failure rollback.
Wait queues return zero immediately for an empty destination, reject nonempty
destinations smaller than one record with `EINVAL`, and leave queued records
untouched if either user-copy segment fails. Larger buffers receive only whole
records. Record tests simulate copy failures; they do not execute the user-copy
assembly or the scheduler's block/wake protocol. Full kernel host `cargo test`
is separate and still has older scheduler fixtures that do not compile against
the current `Context::new` interface. Use image tests for actual context switching
and fault handling.

## Device-tree parser checks

The kernel and ARM64/RISC-V bootloader share `ferric-devicetree`, which consumes
the published `fdt` prerelease. Run its host fixtures and memory checks with:

```sh
cargo test --locked -p ferric-devicetree
cargo clippy --locked --no-deps --all-targets -p ferric-devicetree -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p ferric-devicetree
cargo miri test --locked -p ferric-devicetree
```

Tests cover console aliases and parameters, compatible lists, phandles, CPU
registers with zero size cells, independent default cell widths, PCI ranges,
nested MMIO translation and range boundaries, malformed headers/structures and
invalid cell widths. CI runs these with the host workspace, strict docs/Clippy
and Miri. Regenerate the local DTS fixture with the command in the crate README.
These tests do not execute firmware or initialize hardware. Dependency updates
also require ARM64 bootloader/kernel builds and a software QEMU image boot;
check console, CPU topology, interrupt/timer progress and the signal/clock
regressions. Record image/configuration details and every skipped check.

## Kernel lint and binary checks

| Check | Purpose |
| --- | --- |
| `scripts/ferric-ci/kernel-lint-ratchet.sh` | Holds per-lint Clippy counts and `unsafe` uses at or below `ci/kernel-lints.json` |
| `scripts/ferric-ci/check-kernel-relocs.sh` | Builds and compares relocation replay against a kernel linked at the shifted address |
| `scripts/ferric-ci/check-kernel-ibt.sh <kernel.all>` | Checks `endbr64` at linked interrupt declarations and selected entry points in an IBT-built kernel; direct-call helpers are excluded |
| `scripts/ferric-ci/check-kernel-shstk.sh <kernel.all>` | Checks shadow-stack enable, context-switch and syscall alternatives, WRSS, and the #CP handler with IBT on/off |
| `scripts/ferric-ci/check-trace-overhead.sh --out LOG... --in LOG...` | Compares `ferric-trace-bench` results from paired boots without and with `kernel.trace` (the Nth of each list is a pair) against the tracing overhead budget, on the median of the pairs' differences |
| `scripts/ferric-ci/print-test-fails.sh LOG TITLE` | Prints each `FERRIC_TEST_FAIL` line of a console log with the 40 lines before it, in a log group, for steps that keep only a boot's last lines |

The kernel workflow installs its declared nightly toolchain, `rust-src`,
Clippy and `nasm` as needed. Use the kernel Makefile/custom target path;
ordinary host `cargo test` is not a kernel boot test. When removing warnings,
run the ratchet with `--update` and commit the lower baseline. The workflow
contains the exact preparation for an IBT-configured binary.

The workflow also builds kernel Rustdoc with warnings denied, `regbuf` and the
tracing features enabled, in two configurations: `kernel.iommu = "never"`
without `rings`, and `"auto"` with `rings`. Between them they cover the common
kernel documentation, the optional DMA-grant APIs and the ring scheme, and
catch links to items that a feature compiles out. After generating and copying
the desired kernel configuration for each, run the matching command from
`crates/kernel/`:

```sh
# With kernel.iommu = "never" generated and copied:
RUST_TARGET_PATH="$PWD/targets" RUSTDOCFLAGS="-D warnings" \
  cargo doc --locked --no-deps \
  --features regbuf,trace_syscall,trace_sched,trace_scheme,trace_mm,trace_irq,trace_iommu,trace_level_info \
  --target targets/x86_64-unknown-kernel.json \
  -Z build-std=core,alloc -Z build-std-features=compiler-builtins-mem \
  -Z json-target-spec

# With kernel.iommu = "auto" generated and copied:
RUST_TARGET_PATH="$PWD/targets" RUSTDOCFLAGS="-D warnings" \
  cargo doc --locked --no-deps \
  --features regbuf,rings,trace_syscall,trace_sched,trace_scheme,trace_mm,trace_irq,trace_iommu,trace_level_info \
  --target targets/x86_64-unknown-kernel.json \
  -Z build-std=core,alloc -Z build-std-features=compiler-builtins-mem \
  -Z json-target-spec
```

### Trace mode and the boot environment

The [trace-mode kernel](ferric-tracing.md#trace-mode) and the
[boot environment](ferric-tracing.md#setting-the-boot-environment) the bootloader
builds are checked without booting:

- `cargo test -p ferric-abi` covers `boot_env`: line classification, the
  first-assignment rule, `FERRIC_KERNEL` and `FERRIC_BOOT_RESOLUTION` values,
  and the preset buffer the bootloader merges fw_cfg lines into and the menu's
  `t` key edits (merging, replacing, a full buffer left unchanged).
- `cargo test -p ferric-fwcfg` runs the fw_cfg protocol against a fake device:
  a byte fixture of a big-endian file directory, finding and reading the
  harness's file, another device's signature, a file over the buffer, and a
  directory count past the limit.
- `cargo test -p ferric-config` checks `FERRIC_TRACE_KERNEL_FEATURES` (the
  normal kernel untouched, `profiling` dropped, a traced image kernel's choices
  kept, every feature declared in `crates/kernel/Cargo.toml`), the
  `kernel-trace` package and flag exclusion, the rebuild fingerprints and
  `fw_cfg_env` in `FERRIC_BOOTLOADER_FEATURES`.
- host-tools builds the UEFI bootloader without and with
  `kaslr fw_cfg_env`, and kernel-lints' `trace-mode` job builds the trace-mode
  kernel from a generated configuration and checks it carries its
  `trace: trace-mode kernel` line while the normal kernel's features have no
  tracing.

Loading the trace-mode kernel, the `t` key, the fw_cfg path under QEMU and
user space's behaviour without the trace scheme need an image with
`kernel.trace_mode` and a boot.

### x86-64 signal state

`ferric-xstate-test` runs only when CPUID and XCR0 permit AVX-512. The controlled
signal probe loads every ZMM register and mask, then publishes an assembly
handshake. A sender delivers SIGUSR1 or an application realtime signal (44);
only the handler releases the held-register loop. An alternate-stack case also
checks that the handler actually ran in its registered allocation. A synchronous
UD2 case resumes by advancing RIP through the existing public context. Handlers
clobber representatives of every additional AVX-512 state component. BW-capable
CPUs use full 64-bit mask canaries; F-only CPUs use 16 bits. Missing delivery or
changed state fails, without retrying the probe. Existing preservation and
cross-context leakage stress still run afterward.

The host capability/layout checks compile the actual runtime module without the
rest of the Redox-only runtime. Cargo commands for the relibc workspace also
require its pinned submodules, including `dlmalloc-rs` for workspace dependency
resolution:

```sh
rustc --edition=2024 --test -D warnings \
  crates/relibc/redox-rt/tests/vector_state.rs -o /tmp/ferric-vector-state-tests
/tmp/ferric-vector-state-tests
git submodule update --init --recursive
cargo build --locked --manifest-path crates/relibc/Cargo.toml \
  -p redox-rt --target x86_64-unknown-redox
cargo clippy --locked --manifest-path recipes/tests/ferric-xstate-test/files/Cargo.toml \
  --all-targets --target x86_64-unknown-redox -- -D warnings
```

Host CI runs these checks, strict runtime/recipe Clippy and Rustdoc, and the
recipe's host program (a capability skip when AVX-512 is unavailable).
Its test job uses the same cached pinned-submodule setup as image builds,
including materializing working trees after restoring Git metadata. That setup
also prepares Cargo's Git dependencies.
The native image test exercises the actual signal trampolines. A host execution
of the recipe program uses the host's signal runtime, so it validates the probe
rather than relibc. Running all of `redox-rt`'s unit tests as a Linux-target
package is not supported: its process code uses Redox-only auxiliary-vector
constants. The direct harness keeps the new capability/layout tests runnable
without changing that target boundary.

## Image tests

`config/ferric-smoke.toml` includes the minimal image and forces local source
builds of kernel, relibc, bootloader, RedoxFS, base and Ferric test packages,
even when the surrounding build uses prebuilt packages. Its init script emits
`FERRIC_SMOKE_BEGIN`, runs the tests, emits `FERRIC_SMOKE_OK` and shuts down.

The ASLR test links dynamically against the image's in-tree relibc so it exercises
the interpreter exec path, which installs the mmap floor. A static executable
would bypass that path and miss the sparse-address-space regression. Its host
protocol tests run with `cargo test --locked --manifest-path
recipes/tests/ferric-aslr-test/files/Cargo.toml` and in `host-tools.yml`.

The owned-UDP queue test also has host protocol tests:
`cargo test --locked --manifest-path recipes/tests/ferric-netq-test/files/Cargo.toml`.
`host-tools.yml` runs them and strict host/Redox Clippy and Redox Rustdoc.
The smoke runner serves its exact single-block TFTP fixture from a private,
per-boot directory on QEMU's gateway. No public DNS service or external
network is needed for `netq-udp`; a missing or incorrect reply still fails.
See [the network queue test](ferric-io.md#kernel-bypass-network-queues) for manual setup.

| Recipe under `recipes/tests/` | What it exercises |
| --- | --- |
| `ferric-xstate-test` | Every ZMM register and k0-k7 across preemption and controlled standard/realtime signals, alternate-stack delivery and a resumable invalid-opcode exception; full 64-bit masks on AVX512BW; syscall-return leakage probes |
| `ferric-aslr-test` | Sparse 32 GiB virtual reservation after exec, and address variation between executions |
| `ferric-guard-test` | Thread/main-stack guard faults and accessible stack controls |
| `ferric-tlb-test` | Mapping/protection changes and TLB invalidation across CPUs |
| `ferric-borrow-test` | A scheme call whose buffer cannot be borrowed leaves the caller's shared file mappings unpinned; RedoxFS maps a written or reused file with its current contents, `read` sees stores through a live shared mapping, `fsync` does not write an unmapped page back over a later `pwrite`, and a `pwrite` reaches live shared mappings; prints `FERRIC_BENCH borrow-pwrite`, page writes to an unmapped and a mapped file |
| `ferric-futex-test` | Compare/wait/wake races and expired waiters |
| `ferric-exit-test` | Process exit while a second thread is busy reading a pipe or parking on a futex: the force-killed thread must not block in its next wait, so every child exits ([#200](https://github.com/rexlunae/ferric-oxide/issues/200)) |
| `ferric-scheme-self-test` | A process opens and reads a file on a scheme it created on the same thread and serves from another, which used to spin the caller's CPU in the kernel |
| `ferric-regbuf-test` | Registration, file-backed refusal, access modes, pinning, re-sharing refusal, forwarding, revocation, unmap/close, shared memory and byte/bounce behavior |
| `ferric-blkgrant-test` | `nvmed` DMA into an attached grant: reads and writes against `pread`, refusals, revocation, detach and a copy versus grant read benchmark |
| `ferric-netq-test` | A queue socket on `e1000d` in copy mode (`drivers.net_bypass`): attach, refusals, ARP and ICMP exchanged with QEMU's gateway through the socket's own rings, netstack beside it, port ownership and refused filters, an exact local TFTP fixture exchanged through an owned port, spoofed TX frames dropped, a TX benchmark against netstack, counters, and close with traffic in flight |
| `ferric-iobench` | The Phase 2 disk benchmark: fio-style read jobs on the NVMe disk with each queue policy, IOPS, bandwidth and latency percentiles, and the same-boot A/B of per-CPU queues against one queue ([Disk benchmark](ferric-io.md#disk-benchmark-ferric-iobench)) |
| `ferric-pcache-test` | `kernel.pcache` with a file system scheme of its own: a miss filled through the queue, a hit with no scheme read, readahead after a sequential read, `EIO` from a failed fill and a retry, `EIO` (not a hang) from a done answer whose payload faults and the same answer arriving late ignored, leaving the retry alone, records put back after a queue read that faults, `EPERM` for a child process (attaching, or binding an unbound queue), a queue kept across `exec`, an invalidated page filled again and a stale fill not taken, an answer to a closed object's record ignored as stale (it neither fills nor fails the new object's fill of the same page) and no stale records after reopening it, and readers released with `EIO` when the queue closes, after which reads go to the scheme; prints a cached 4 KiB `pread` time beside the scheme's. Then on RedoxFS: a read-only description follows writes, truncations and an append through another one, a shared writable mapping keeps a file out of the cache exactly while it lasts (a private one changes neither the file nor its caching), and cached reads are benchmarked against RedoxFS's own in the same boot (`FERRIC_BENCH pcache-redoxfs-*`, paired A/B) |
| `ferric-ring-test` | `kernel.ring` setup and refusals, `Nop`/`Read`/`Write`/`Fsize`/`Fstat`/`Fsync` through a ring, the fixed-file table, asynchronous chains with `min_complete` waits and close, a chain across RedoxFS, the pipe scheme and the null scheme in one `Enter`, broken and overlong chains, incomplete chains, CQ back-pressure, a corrupt SQ tail, `EVENT_READ` on completion, and ring versus system call timings. `--async-only` selects the asynchronous chain and close probes used by `trace-lifecycle` to check kernel-worker identity restoration |
| `ferric-trace-test` | Boot-time and live tracepoint enable, decoding of every CPU's ring, system call enter/exit pairs and results, scheme request/response matching by SQE tag, timestamp order, mapping-operation results and target spaces (map/protect/unmap, peer context, refusals, validation and disable), the mapping journal (fixed replacement, kernel-placed and fixed `mremap`, a fork's clone and its teardown, and disable), queued futex wait/resume results and wake counts (32/64-bit timeout, wake-before-resume order, sustained empty-wake polling, mismatch and disable), ordered kernel context creation/start/exit for a child process, a thread and asynchronous ring workers (including restored PID 0), `procmgr`'s fork/exec/exit/wait records for a child with its exit status and the refusal of forged `emit` records, and read-only mapping of the rings with `commit` checks. When the kernel reports host-visible rings in `kernel.trace/host` (booted with `FERRIC_TRACE_HOST`, the harness's trace mode, and the kernel accepted it) its default run starts with `trace-host-read`, which checks that a read of CPU 0's ring returns records and that `commit` is accepted, and then runs the other steps except `trace-mmap`, `trace-mapping` and `trace-clock`, which map the rings and print `FERRIC_TEST_SKIP` because host rings cannot be mapped yet ([#378](https://github.com/rexlunae/ferric-oxide/issues/378)). The smoke script skips `trace-capture`, which captures through the mappings, when `ferric-trace-test host-rings` exits 0 (it prints nothing, and exits 0 only with host rings); `ferric-trace-test crash` runs the crash step either way |
| `ferric-ext4-test` | Mounts the `ext4d` fixture image explicitly read-only (`fs.ext4`), lists the root, an empty and a hashed directory, reads files whole, at an offset and through a symbolic link, reads the link, checks `stat`, and checks that writing, creating, removing, renaming and `mkdir` fail with `EROFS`. `ext4-journal` mounts a volume that needs recovery: ext4d must replay its journal (`fs.ext4_journal_replay`), serve the journaled contents and leave the image clean. The step is skipped only when `ext4d --features` does not list `journal-replay`; once it does, a replay error fails the step. `ext4-image-unchanged` checks the read-only backing image byte for byte; `ext4-write` uses a separate default mount for write/truncate/create when the compiled writer is advertised |
| `ferric-fat-test` | Mounts the `fatd` FAT32 fixture image (`fs.fat`), then the exFAT one (`fs.exfat`, a skip without it), and on each lists the root, an empty directory and one over several clusters, reads files whole and at an offset by their long names, their 8.3 short names (FAT only) and in another case, checks sizes, kinds, modes and modification times from `stat`, and checks that writing, creating, removing, renaming and `mkdir` fail with `EROFS` |
| `ferric-blocklayer-test` | Serves the `crates/blocklayer` fixture images with `ferric-blockd` (`fs.luks`, `fs.lvm`): an LV inside LUKS2 with 4096-byte sectors, LUKS2 inside an LV and a LUKS1 volume read back their patterns whole and across a sector boundary; opening for writing fails with `EROFS`, a UID 2000 process gets `EACCES`, and a wrong passphrase makes the daemon exit without serving. With `ferric-unlock` (`fs.unlock_at_boot`), `blocklayer-unlock-boot` reads back the volumes its `/etc/ferric/crypttab` had unlocked at boot: LVM on LUKS from a key file, and ext4 mounted by `ext4d` through `ferric-blockd` |
| `ferric-trace-bench` | Times RedoxFS requests and null system calls with tracing compiled out, compiled in but off, and every event on, for the overhead budget |
| `ferric-perf` | Not in the smoke image: process spawn, thread and process ping-pong over pipes, pipe throughput and round trip, `/scheme/log` against `/scheme/null` writes, file create/stat/unlink and large-file `mmap` timings, run by `config/ferric-perf.toml` ([Process and IPC benchmarks](#process-and-ipc-benchmarks)) |
| `ferric-iommu-test` | QEMU edu device DMA, domain ownership, permissions, invalidation, fork isolation, grant/sub-grant access and revocation; with tracing, `iommu.fault` records of its faults (`iommu-trace`) completed DMA map/protect/unmap records, cached translations and live disable (`iommu-mapping-trace`), and the `irq.deliver`/`irq.ack` pair of its MSI (`trace-irq`) |
| `ferric-syscall-fuzz` | Seeded multi-process syscall fuzzing and validation of invalid inputs |

`ferric-tlb-test` also covers a relibc join race: an exiting thread stores its
return value and then wakes the joiner, but the joiner can see the value, return
and unmap the thread's control block before that wake runs. relibc's
`Waitval::post` takes the futex address before the store and treats `EFAULT`
from the wake as nobody left to wake, instead of panicking (`RELIBC PANIC` in
`sync/mod.rs`, which boot-smoke fails on). relibc's own `pthread/join_race`
test joins 2000 short-lived threads; neither test can force the window, so a
pass only shows the fix did not break joins.

The edu driver is started through `pcid-spawner`, separately from the console
test script. Its PCI matching configuration is in the IOMMU test recipe.
Tests may skip absent optional features; do not interpret a skip as coverage.

Image, profiler-image and CET-image builds use
`.github/actions/prepare-image-runner` before restoring build caches or compiling
packages. Hosted Ubuntu runners carry large SDKs that these container builds
do not use. The jobs explicitly bind those host SDK directories under
`/ferric-host-sdks`, then remove their contents to make room for source builds
and image staging. Deleting the container's own SDK paths would only affect
its overlay and would not reclaim that host storage.

`scripts/ferric-ci/prepare-image-runner.sh` refuses cleanup outside a
GitHub-hosted Linux Actions runner and validates every directory as a bind
mount before deleting anything. It preserves mount roots and does not follow
SDK symlinks. The workspace, package outputs, build caches and the container's
toolchain remain available. Its log reports capacity before and after cleanup.
Run `bash scripts/ferric-ci/test-prepare-image-runner.sh` for disposable-directory
regressions covering environment guards, missing bindings, hidden/nested files
and symlink targets; the tests mock mount discovery without touching host SDKs.

`.github/workflows/image.yml` generates feature settings, forces KPTI on,
enables IOMMU `auto`, shadow-stack policy `auto`, registered buffers,
`nvmed` grant DMA (`drivers.grant_dma`) and kernel tracing (with
`sched.switch` on from boot),
and builds a second kernel/image with IOMMU `enforce` and, for `ferric-smoke`,
a third without kernel tracing. Its five boot variants
are no IOMMU, VT-d auto/enforce, and AMD-Vi auto/enforce. It requires the
registered-buffer and tracing result markers, the trace boot log line,
KASLR/KPTI logs, the IBPB decision (and barriers issued when it is on), shadow-stack detection and
futex wake-race marker, and checks PCID use when the runner exposes it.
`check-iommu-log.sh` checks driver/domain and device-test evidence for each
IOMMU mode, and `check-blkgrant-log.sh` checks the storage-grant steps and
posts the benchmark as a notice, and `check-iobench-log.sh` checks the disk
benchmark's lines and A/B and writes its figures to the job summary.
`check-oneshot-log.sh` checks that the BSP calibrated the TSC and chose a
clock source, that each of the four CPUs fired every deadline of its one-shot
timer lateness series, and that the BSP's selftest rounds passed
(`kernel.time.one_shot`, on in the CI image), and posts the clock source and
lateness as a notice. Read those scripts for the exact required marker set.
The `trace-overhead` job boots the images with and without tracing ten times
each, as ten back-to-back pairs that alternate which image boots first, and
runs `check-trace-overhead.sh` on the twenty serial logs (see
[Kernel tracing](ferric-tracing.md#overhead-budget)). The gate judges paired
differences, never medians taken over two separate sets of boots: each run's
compiled-out boot against its compiled-in-but-off boot, and each traced boot's
`on` rounds against its own interleaved `pairoff` rounds, with the budget on
the median of those differences, so runner load that drifts during the job
cancels within each pair and a stall in one boot is outvoted (issue #280). The
notice lists every pair's difference, so a failure shows whether most pairs
agree. `scripts/ferric-ci/test-check-trace-overhead.sh` (host-tools CI) runs
the gate on fixture logs built from a recorded noisy job. It discards each
boot's output, so when a boot fails it runs `print-test-fails.sh` on the boot's
console log, and a test that failed early in the boot can be read from the job
log.

The workflow caches `~/.cargo/git/db` and `~/.cargo/registry/cache` under a
`cargo-git-` key derived from the tracked `Cargo.lock` files. Cargo fetches a
git dependency only when its locked revision is missing from the database, so
a GitLab outage no longer fails a build whose locked revisions are already
cached. The key covers this repository's lockfiles; a dependency the cookbook
fetches during a build carries its own lockfile and is not part of the key, so
the `cargo-git-` restore key falls back to an earlier cache and Cargo retries
the fetch (`CARGO_NET_RETRY`).

The workflow exports recipe-facing kernel/relibc/bootloader settings from its
temporary generated config. It does not install a root `ferric.toml` or
automatically apply every generated user-space CPU flag to this mixed
prebuilt/source image. Dedicated local baseline builds are a separate check.

## Process and IPC benchmarks

`recipes/tests/ferric-perf` holds microbenchmarks for the costs that the other
benchmarks do not cover: starting processes, switching between contexts that
talk over pipes, logging, file metadata and mapping files. `config/ferric-perf.toml`
(`image.config = "ferric-perf"`, or `make all CONFIG_NAME=ferric-perf`) boots the
minimal system, waits five seconds for the boot services to settle, runs
`ferric-perf` once between `FERRIC_PERF_BEGIN` and `FERRIC_PERF_DONE`, prints
`FERRIC_SMOKE_OK` and shuts down. It needs no optional kernel features and is not
part of CI's image boots. Run it by hand in any image that includes the recipe:
`ferric-perf` runs everything, `ferric-perf spawn pingpong` runs the benchmarks
whose names start with those words, and `ferric-perf --list` lists them.

| Line (`FERRIC_BENCH perf-…`) | One sample |
| --- | --- |
| `clock` | Two consecutive readings of the monotonic clock: the overhead included in every other sample; adds `res_ns`, the clock's resolution from `clock_getres` |
| `spawn-fork-exec-static`, `-dynamic` | `fork`, `execve` of an empty program in the child, and `waitpid` until it exits |
| `spawn-posix-spawn-static`, `-dynamic` | relibc's `posix_spawn` of the same program, which builds the child without copying the parent, and `waitpid` |
| `spawn-ion` | `ion -c true` through `posix_spawn`: a shell started to run one builtin |
| `pingpong-thread`, `pingpong-process` | One byte to a peer thread, or to a peer process (`ferric-perf --pong`), over one pipe and back over another |
| `pipe-rtt-1b` | One thread writes a byte into a pipe and reads it back: the pipe's system calls without a wake-up or switch |
| `pipe-throughput` | Reading 4 MiB that a writer thread sends in 64 KiB writes; adds `bytes` and `mib_s` |
| `write-null`, `write-log` | One 70-byte line written to `/scheme/null` (`zerod`), or to `/scheme/log`, the standard output and error init gives services; adds `bytes` and `mib_s` |
| `file-create`, `file-stat`, `file-unlink` | Creating (`O_CREAT \| O_EXCL`, then closing) an empty file in a new scratch directory, `/tmp/ferric-perf/ferric-perf-<pid>-files` (`--dir` sets the parent; a name a killed run left gets a `.<n>` suffix, and nothing already there is touched; RedoxFS: `/tmp` is a directory of the root file system), getting a file's metadata by path, removing a file |
| `mmap-map`, `mmap-touch`, `mmap-unmap` | `mmap` of a 32 MiB file (`PROT_READ`, `MAP_PRIVATE`, as `ld.so` maps objects), reading one byte of every 4 KiB page of the new mapping, `munmap`; the first two add `bytes` and `mib_s` |

The static spawn target is `/usr/bin/ferric-perf-true`, an empty Rust program
linked like the other test programs; the recipe builds the same program a second
time with `DYNAMIC_INIT` as `/usr/bin/ferric-perf-true-dynamic`, which `ld.so`
starts with relibc's `libc.so` (as `ferric-aslr-test` is linked). On riscv64gc,
where the cookbook has no dynamic linking, both are static. The `write-log`
lines appear on the serial console; they start with `ferric-perf write-log:`.

Every line has the same fields:

```text
FERRIC_BENCH perf-spawn-posix-spawn-static iters=200 mean_ns=… median_ns=… p99_ns=… min_ns=… max_ns=…
```

- **`iters`** is the number of timed samples. Each benchmark first runs a few
  untimed warm-up iterations, then runs until it reaches its iteration limit (200
  spawns, 10,000 round trips or null writes, 500 files, 20 mappings, …) or until its
  time budget of one to three seconds runs out, whichever comes first, with a small
  minimum. A count below the limit means the operation was slower than planned. A
  run stays well under a minute on KVM even when something waits for the scheduler
  tick on every iteration.
- **`mean_ns`**, **`min_ns`** and **`max_ns`** are exact. **`median_ns`** and
  **`p99_ns`** come from `ferric_iobench`'s fio-layout histogram (the middle of the
  bucket the rank falls in, within 1/64 of the true value), the same percentiles
  `ferric-iobench` and `ferric-trace counts` report. The rank is the nearest rank,
  rounded up: the median of an even count is the lower middle sample, and with
  fewer than 100 samples (the `mmap` lines always, the spawn and `write-log` lines
  when their budget runs out) `p99_ns` is the slowest sample, the same as `max_ns`.
- Each sample is timed with two clock reads, so it includes one `clock` sample.
  On Redox a clock read is a system call; subtract `perf-clock`'s median before
  comparing operations that take only a few microseconds, such as `pipe-rtt-1b`
  and `write-null`. `perf-clock`'s `res_ns` is the resolution of the kernel's
  clock source: 1 for the KVM clock or the TSC, the HPET's period (about 10 ns
  under QEMU), or 838 for the PIT fallback. Under QEMU an HPET read is a VM exit,
  so the `clock` cost itself depends on the source; compare runs only with the
  same source. With `kernel.time.one_shot` the kernel log names it at boot
  (`time: clock source ...`); otherwise it is the KVM clock with `x86_kvm_pv`
  under KVM, else the HPET, else the PIT.
- The run ends with `FERRIC_TEST_PASS perf`. A benchmark that fails prints
  `FERRIC_TEST_FAIL perf-<group>: <cause>`, the others still run, and the run ends
  with `FERRIC_TEST_FAIL perf: …` and exit status 1. `boot-smoke.sh` fails a boot
  with any `FERRIC_TEST_FAIL` line.

Read the lines in pairs as well as one at a time:

- `pingpong-thread` minus `pipe-rtt-1b` is roughly the cost of two wake-ups and
  the context switches between two threads. The scheduler places the two sides:
  on one CPU a round trip is two direct switches, on two CPUs it is four switches
  through the idle contexts plus the wake-up IPIs, so compare runs with the same
  number of vCPUs. `pingpong-process` adds the switch
  between address spaces (CR3 loads, and IBPB barriers when `kernel.cpu.ibpb` is
  on), so compare it under `kernel.cpu.kpti` and `kernel.cpu.ibpb` settings.
- `write-log` minus `write-null` is what the log path costs per line: `logd`
  writes each line to the kernel's serial console before it answers.
- `spawn-*-dynamic` minus `spawn-*-static` is the dynamic linker's share of
  starting a process, and `spawn-fork-exec-*` minus `spawn-posix-spawn-*` is the
  cost of copying the parent's address space with `fork`.
- The `mmap` lines split the work differently on Redox and on Linux, so compare
  their sum across systems. On Redox, RedoxFS reads the file into a buffer of its
  own on the first mapping (the untimed warm-up) and keeps it while the file is
  unchanged, and the kernel maps every page of a private file mapping
  copy-on-write from that buffer before `mmap` returns: a timed `mmap-map` is a
  request to RedoxFS plus mapping 8192 pages, `mmap-touch` takes no page faults
  (only TLB misses), and `mmap-unmap` removes the 8192 pages. On Linux, `mmap`
  only reserves the range and `mmap-touch` takes the page faults. None of the
  timed samples reads the file.

For a comparison between two kernels, configurations or commits, boot each image
several times on the same idle host, alternating which boots first, and compare
the medians of the paired differences, as `scripts/ferric-ci/check-trace-overhead.sh`
does for the tracing budget. Run-to-run noise under QEMU/KVM is several percent
(see [Kernel tracing](ferric-tracing.md#overhead-budget)), so a single boot shows
an order of magnitude, not a small change. To see where the time of an operation
goes, take a capture while the benchmark runs on a kernel with `kernel.trace` and
read it with `ferric-trace latency` ([Latency](ferric-tracing.md#latency)): the
system call, scheme request, wake-up and interrupt durations behind each line.

The benchmarks are plain POSIX, so they also run on Linux for a reference point,
given host paths for their targets:

```sh
cd recipes/tests/ferric-perf/files
cargo build --release
./target/release/ferric-perf --static-true target/release/ferric-perf-true \
    --dynamic-true target/release/ferric-perf-true --ion /bin/sh \
    --log /dev/stderr --null /dev/null --dir target/scratch
```

On the host, `cargo test --locked --manifest-path recipes/tests/ferric-perf/files/Cargo.toml`
runs the unit tests of the timing and statistics code (warm-up, iteration limit,
budget and minimum against a scripted clock; phase timing; line format and
throughput), the system-call wrappers, the benchmark selection, and a CLI test
that runs every benchmark briefly (`--quick`) and checks every line, the
failure path and the cleanup. `host-tools.yml` runs those tests and strict
Clippy for the host and `x86_64-unknown-redox`, and Rustdoc for the target.
The numbers on Redox and the dynamic spawn target need an image build and a boot.

## Reproducing a boot

For a local x86-64 smoke image with the desired CPU and kernel features saved
in `ferric.toml`, select the profile through the configuration tool:

```sh
cargo run -q -p ferric-config -- set image.config=ferric-smoke
make config-check
make all
make qemu
```

The same choices appear in menuconfig. Select `image.config=ferric-fuzz` for
the extended fuzzing profile; both test profiles require `target.arch=x86_64`
because their included tests use x86-64 register-state instructions. Profile
selection changes image contents and the output name, while preserving the
saved CPU/kernel policy and generated package/general image overrides. It does
not force CI's KPTI, IOMMU or registered-buffer settings: select those features
separately when testing them.

Keep `ARCH`, `BOARD` and `CONFIG_NAME` overrides consistent with `ferric.toml`.
A mismatching command-line `CONFIG_NAME` makes the generated target guard omit
the saved image, CPU and kernel settings; selecting the profile in the catalog
keeps them aligned.

For the scripted Linux CI-style boot, after creating an image, set `image` to
its actual path (the local example above and CI both use `ferric-smoke`):

```sh
image=build/x86_64/ferric-smoke/harddrive.img
scripts/ferric-ci/boot-smoke.sh "$image" serial.log
BOOT_IOMMU=vtd scripts/ferric-ci/boot-smoke.sh "$image" serial-vtd.log
BOOT_IOMMU=amdvi scripts/ferric-ci/boot-smoke.sh "$image" serial-amdvi.log
scripts/ferric-ci/boot-smoke.sh "$image" serial-host,-x2apic.log host,-x2apic
```

The script expects QEMU, OVMF and Linux utilities. It uses a copy of the image,
q35, four CPUs, 2 GiB RAM, fixed PCI slots for NVMe, e1000 and edu, two HDA
controllers, and serial output. It uses KVM with `host` when available and
otherwise TCG with `max`. `BOOT_TIMEOUT` changes the default 300-second limit.
`BOOT_IOMMU=vtd` requests interrupt remapping and a split irqchip. AMD DMA
remapping requires QEMU 10 or later; the workflow uses a newer-QEMU container
for that variant. `OVMF_FIRMWARE` selects a firmware file instead of searching
the usual paths.

The third argument is the QEMU `-cpu` model. The image workflow's boot matrix
also runs `host,-x2apic` (the last line above, KVM only): with x2APIC hidden
from the guest, the kernel drives its local APICs in xAPIC mode, so the decoding
of the xAPIC ID register (its ID sits in bits 31:24) and the targeted IPIs, such
as per-CPU TLB shootdowns, that are sent to those IDs get a real boot. The other
boots use `host`, which runs in x2APIC mode. The decoding itself is also
host-tested (`apic_id` in the kernel helper tests). That boot checks
correctness, so its performance A/B gates, the NVMe queue-policy A/B
(`check-blkgrant-log.sh`) and the disk benchmark's adaptive-against-main A/B
(`check-iobench-log.sh`), only warn when they fail (`PERF_AB=report`, set for
the whole boot job; see `scripts/ferric-ci/perf-ab.sh`): every EOI and IPI is
then an xAPIC MMIO access that exits to the host under nested KVM, so nvmed's
per-CPU policies, which take more of them, measure the APIC emulation more
than nvmed. The figures are still reported.
`scripts/ferric-ci/test-perf-ab.sh` (host-tools CI) runs both checks on
fixture logs with and without `PERF_AB=report`.

The script stops early instead of waiting out the timeout when the smoke shell
cannot start: ld.so reporting that it could not link `'/usr/bin/ion'` (the
quoted path it prints), or init reporting that `ion /usr/lib/ferric-smoke.ion`
failed, ends the run with that console error and `boot-smoke: FAIL, the smoke
shell could not start`. A loader failure in any
program before `FERRIC_SMOKE_OK` fails the run even if the shell later prints a
PASS or SKIP: a helper that failed to load can otherwise look like an absent
optional feature. The shell-specific watch ends an impossible boot early; child
failures are checked in the final log so the other tests can finish. A link
failure first printed after the marker does not invalidate completed tests if
QEMU exits successfully. A timeout or nonzero QEMU exit still fails, even after
the marker: finishing the test script does not establish that the guest shut
down cleanly. `scripts/ferric-ci/test-boot-smoke.sh`
exercises these cases against a fake QEMU, without firmware or virtualization.
This turns the intermittent boot in
[#155](https://github.com/rexlunae/ferric-oxide/issues/155) into a fast,
labelled failure. The investigation in
[#300](https://github.com/rexlunae/ferric-oxide/issues/300) connects a reproduced
empty audio-scheme capture to a loader `ENOMEM`, and the original #222 `/tmp`
failures to `mkdir` failing in the loader. The reproduced mapping failure comes
from exec raising its mmap floor to the argument/environment buffer above the
randomized stack. Exec now keeps the floor
at the ELF image spans; `ferric-aslr-test` rejects this failure deterministically
with an uncommitted 32 GiB reservation. Repeated Linux KVM boot validation remains
required; the earlier opaque `NotFound` and executor-interruption reports are not
assumed to share this cause.

ld.so's fatal message is followed by a second `[ld.so]:` line with the reason
for the failing attempt: the path, original errno and exact operation (`open`,
`fstat`, the `pread` of the object's headers, or `mmap`, including the requested
mapping length), the search paths
a library is in none of, or why an opened object could not be loaded (invalid
ELF headers, a missing `PT_LOAD` or `PT_DYNAMIC` segment, an invalid dynamic
segment or section with the `object` crate's error, a segment outside the file
or the mapping, or the `mmap` of its segments with the requested length and
errno). `dlerror` returns a distinct message for each of these load failures
instead of one generic "malformed" message. Thus an `ENOMEM` while loading a library does not by
itself establish physical memory exhaustion or even that `open` failed. The next
occurrence names the file and error without `LD_DEBUG`. Context is carried
by the typed load error, with its original errno, so a later malformed object
or TLS failure cannot reuse an earlier search diagnostic. The host check
`bash scripts/ferric-ci/test-ld-so-errors.sh` compiles the actual object search,
mapping ownership, error, fatal-message formatter and errno modules. It checks
candidate order,
successful fallback, missing and inaccessible libraries reaching the fatal
output, isolation from a later parse/TLS failure, stable `dlerror` messages
(one per load failure kind), load failures naming the object and keeping their
`object::Error` or errno source, every loader error's `core::error::Error`
source chain down to that error,
and descriptor cleanup after failed stat, header-read and map operations. File
descriptors are owned immediately after open, including on error paths; a
successful mapping retains its descriptor until after unmap. It also checks that
only the length the loader asks for after reading the first 4 KiB is mapped, that
a shorter file is read and mapped only to its end, and that an interrupted
(`EINTR`) or short read of those 4 KiB is retried rather than failing the load.
A scheme call can return `EINTR` with no signal handler involved: the kernel
cancels a call whose context was woken, and the smoke image's last commands hit
this at the end of every boot. The script builds the
`object` crate for the host from relibc's own workspace and lockfile, so the
harness also compiles `ld_so/image.rs` and checks which length an ELF object
needs (`image::loaded_len`): on hand-built objects (segment contents, a note after
the last load segment, a program header table that ends last, a segment past the
end of the file, and headers the probe cannot parse, including `PN_XNUM`), and on
the test binary itself, whose mapping must cover every segment and stop before
its section headers. Platform I/O, file guards, errno storage
and its message table use host adapters; this is not an end-to-end dynamic-link
or guest-boot test.

relibc's `pthread_create` (and so Rust's `thread::spawn`) reports every
failure as `EAGAIN`. It logs the step that failed and the underlying errno at
`warn` level, which relibc writes to stderr by default (`RELIBC_LOG_LEVEL`
changes the level): the stack `mmap`, the guard `mprotect`, the TCB, or
creating the thread itself. When creating the thread fails because the kernel
cannot allocate a context's 64 KiB kernel stack, the kernel logs that with the
number of free frames.

When the marker does not arrive within the timeout, the script first samples
every CPU's registers through the QEMU monitor (several times, with the
frame-pointer chain of each CPU spinning in the kernel, or in user space for a
CPU at CPL 3, as `user frame` lines), then sends an NMI to CPU 0. The NMI
handler prints its stack trace and then one line per context:

```text
NMI: 41 contexts
  pid 21 "/scheme/initfs/bin/redoxfs" Blocked "pcache::records" running=false queued=false cpu=Some(1) syscall=Some([22000003, ...])
  0xffffff7f8013bb60: busy
```

Each row gives the process, the context's name, its status and block reason,
whether it is running or queued to run, the CPU it last ran on and the system
call it is in. With every CPU idle (a lost wake-up, as in
[#200](https://github.com/rexlunae/ferric-oxide/issues/200)), the stuck
context is the one blocked on something that will never come. The handler takes
only `try_` locks, so it cannot deadlock on a lock the interrupted code holds:
a context whose lock is held prints as `<address>: busy`, and `NMI: context
list busy` means the context list itself was locked and no rows follow. Writing
to the console still takes the console lock, so an NMI that interrupts a
console writer stalls there, as its stack trace always has. After QEMU is
stopped the script copies the context rows to stderr, so they reach the job log
even where a workflow discards the script's stdout or keeps only the console's
tail (trace-overhead does both).

The two HDA controllers exercise the audio scheme naming in `ihdad` and
`audiod`. The smoke script waits up to 30 seconds for the second controller's
`audiohw.<pci-address>` scheme and reports `audio-second-controller`: the
first driver keeps `audiohw`, so a machine with the chipset codec and GPU
audio controllers does not lose every controller to a failed registration.

`scripts/ferric-ci/test-boot-smoke.sh` exercises the runner itself with a fake
QEMU, so it needs neither firmware nor virtualization. It checks that only the
expected TFTP fixture is served, that paths with spaces and commas reach QEMU
correctly, and that per-boot fixture directories are removed on success and
failure. It covers pass, skip, failure, a crash after the marker, a nonzero
QEMU exit or timeout after the
marker, an exit during the final timeout poll, a missing test result and a missing
marker. A smoke shell that cannot be linked fails early with that message. Any
loader failure before a later marker, including a child failure followed by SKIP,
must fail the run; a link failure first printed after completion may pass with a
clean QEMU exit. This ordering is also checked when the error and marker share
a line. A large UTF-8/ANSI console fixture has a ten-second host deadline: the
final loader-failure check scans records once, avoiding Bash glob removal over
the entire log, which took minutes on a repeated-launch console. NUL padding
and carriage returns are removed before matching. The fixtures also require timeout NMI context rows (and only
those) on stderr and both HDA controllers in QEMU's arguments. A Unix monitor
socket fixture supplies CPL 3 registers and a user frame; it checks the emitted
return address and that unaligned, out-of-range and halted-CPU frame pointers are
not read.
The probe consumes the monitor greeting before sending commands, keeping
register and memory responses aligned with their requests. Its fake QEMU stays alive for a
moment after printing, like a guest still shutting down, so the watch loop
reads the log instead of finding QEMU already gone. The host-tools workflow
runs it before the Rust checks.

`scripts/ferric-ci/test-print-test-fails.sh` checks `print-test-fails.sh`
against fixture console logs: a log with no failed test prints nothing; with two
failures it prints exactly the 40 lines before the first and everything up to
the second, inside a log group; and a console line that looks like a workflow
command (`::error::`) is printed only while workflow commands are stopped. The
host-tools workflow runs it after `test-boot-smoke.sh`.

`scripts/ferric-ci/test-check-sched-bench-log.sh` checks
`check-sched-bench-log.sh` (the [scheduler baseline](ferric-tracing.md#scheduler-and-timer-baseline)'s log
check) against fixture logs whose `counts` lines carry every gap field
`ferric-trace counts` prints: complete 1- and 4-vCPU baselines pass, and a log
missing a tick window, a gap field, the measured TSC rate or, with more than
one CPU, the cross-CPU interrupt lines fails with a message. The host-tools
workflow runs it with the trace-overhead gate's test.

### CET image and Kleya rerun

`cet-image.yml` builds a separate smoke image with `kernel.cpu.shstk=auto`,
`kernel.cpu.ibt=auto`, and KPTI `auto`. Its `ferric-cet-image` artifact contains
`harddrive.img.zst`, the unstripped `kernel.all`, `kernel-config.toml`,
`build-info.txt`, the two boot helper scripts, and `SHA256SUMS`. Build metadata distinguishes the checked-out
PR merge commit from the PR head and records the run ID and kernel settings.
Use all files from the same successful run; retain the symbols for #CP traces.

On Kleya, download and extract the fresh artifact, then verify it and use its
included rerun helper:

```sh
sha256sum -c SHA256SUMS
zstd -d harddrive.img.zst -o harddrive.img
bash boot-cet.sh harddrive.img serial-cet-kleya.log
```

The helper requires writable KVM and uses `-cpu host,migratable=off`, which
exposed CET on Kleya where plain `host` hid it. It runs the existing disposable
image smoke harness and requires CET detection **and enablement**, the pool
isolation self-test, syscall fuzz success and clean smoke completion. A CET
skip or KPTI preventing enablement does not count as a successful rerun.
Preserve `build-info.txt`, `SHA256SUMS`, `kernel.all`, the serial log and QEMU
version with the result for [#78](https://github.com/rexlunae/ferric-oxide/issues/78).
The image workflow's ordinary CET-disabled CI boot validates the pool and
fallback boot, not this hardware result. The pool test runs after the secondary
CPUs reach their BSP wait, retires two successive probe mappings, and requires
those CPUs to service shootdowns with interrupts still masked. This covers the
startup deadlock seen in the PR #174 Kleya rerun even on CI CPUs without CET;
the post-enable WRSS and control-protection paths still need the hardware rerun.

The harness rejects crashes and `FERRIC_TEST_FAIL` markers and requires the
smoke completion marker. A completion marker alone is insufficient because
the init script prints failures and continues to shutdown. Use the workflow's
additional log checks for feature-specific validation. Image variants without
the requested kernel features cannot exercise those paths merely by adding a
QEMU device.

## Test harness

`ferric-harness` (`tools/ferric-harness/`) boots an image in QEMU under a
supervisor and drives it from the host. It logs the serial console with host
timestamps, waits for patterns on it, takes screenshots, reads the text
consoles exactly, sends keyboard and pointer input, and passes a boot
environment through fw_cfg or the bootloader's editor. Each run ends with a
verdict and a `report.json` that holds boot milestones, `FERRIC_TEST_*` and
`FERRIC_BENCH` records, and host CPU and KVM statistics. `report` compares
groups of runs pair by pair, the same way `check-trace-overhead.sh` does. It
applies the crash, loader and smoke rules of `boot-smoke.sh`, uses the same
machine topology, and adds the `mode` check: a normal-mode run fails if the
kernel prints a `trace:` line. For example:

```sh
cargo run -p ferric-harness -- run build/x86_64/ferric-smoke/harddrive.img --display none --smoke
```

[Test harness](ferric-harness.md) documents every command, the scenario
format, the run directory and the report. It also lists what has been
validated against real images. CI's image boots still use `boot-smoke.sh`.

## Fuzzing and diagnostics

The smoke configuration runs a short fixed-seed syscall fuzz test.
`config/ferric-fuzz.toml` replaces its console script with a 20-minute,
eight-process random-seed run, scheduled by the image workflow. The seed is
printed so a failing run can be replayed with the same fuzzer arguments.

Each child also makes the calls that crashed the kernel before, independent of
its random stream, so a reintroduced bug fails every run instead of only the
seed that first found it: `mremap` and an anonymous `fmap` with `MAP_FIXED` at
the top of the kernel half, a file-backed `fmap` with `MAP_FIXED` whose end
leaves user space, `funmap` with a length that has no aligned value, a negative
`FUTEX_WAIT` timeout and a negative timer write. Every result is checked, and
the child exits with a regression status if one is accepted instead of refused.
The anonymous `fmap` needs no descriptor, and the file-backed one fails the run
if its scratch file cannot be created, so neither check can be skipped
silently. A failing check writes its reason to the child's scratch directory,
which the parent prints once per distinct reason, so the boot log names the
check a seed trips without replaying the child.
Mapping requests use `redox_syscall::Map`, including its distinct offset and
address fields. Source mapping setup must succeed before testing `mremap`.
The invalid file-backed requests test rejection at kernel preflight; a legal
file-backed map and unmap also have to succeed to exercise filesystem mapping.

Preserve the serial log, image configuration, feature choices, kernel symbols
and test seed. `scripts/backtrace.sh` helps symbolize kernel addresses;
kernel-image KASLR traces use link addresses. Image CI retains build logs on
failure and serial logs for boot variants. `kernel.serial_debug`,
`kernel.syscall_debug`, `kernel.debugger` and profiling options are defined in
the feature catalog; enable them through configuration when needed.

For behavior changes affecting an image, build with `make all` and boot with
`make qemu` where possible, then state when the environment prevented either.
Actual machine reports belong in root `HARDWARE.md`; its inherited reports
include old Redox versions and are not validation of current Ferric features.

## Native build checks

### Coreutils pipe transfers

The `uutils` recipe applies `dd-short-writes.patch` to retain every byte when an
output write accepts only part of a block. `iflag=fullblock` controls input reads;
it is not required to complete output writes. Interrupted writes resume at the
unwritten suffix, zero-progress writes fail, and terminal errors retain the
complete/partial record and byte counts already committed. This follows Rust's
[`Write` contract](https://doc.rust-lang.org/std/io/trait.Write.html#tymethod.write),
including its permitted short writes.

After the cookbook prepares the recipe source, run its unit tests and the
existing capped-file regression on the host:

```sh
cargo test --locked --manifest-path recipes/core/uutils/source/Cargo.toml -p uu_dd --lib
cargo test --locked --manifest-path recipes/core/uutils/source/Cargo.toml \
  --no-default-features --features dd --test tests test_dd::test_stats_are_reported_when_a_write_fails
```

The patch includes deterministic short-write, interruption, zero-progress and
partial-error tests. The smoke image builds `uutils` from source and checks that
`dd if=/dev/zero bs=1048576 count=4 status=none | wc -c` delivers all 4 MiB,
reporting `FERRIC_TEST_PASS dd-pipe`. A prebuilt `uutils` package does not acquire
this recipe patch until rebuilt. The original 64 MiB transfer failure is tracked
in [#353](https://github.com/rexlunae/ferric-oxide/issues/353).

Local validation on 31 October 2026 rebuilt the combined x86_64 `wayland` image
through `cargo xtask build` at integration `6b6fbdf88068`. It reached login under
KVM with four vCPUs and 4 GiB of memory, using snapshot disk writes. The default
64 MiB `dd` pipe delivered 67,108,864 bytes, matched the expected SHA256, and
compared equal to a separately written regular file. The exact Ion smoke-test
fragment above also passed. Before the patch, the same default pipe delivered
only 4 MiB. This validates the local pipe correction; a graphical COSMIC session
and hardware performance remain untested.

The final COSMIC image was rebuilt and rechecked from merged main after the
implementation fixes landed; its source commit, image checksum and boot/I/O
evidence are recorded under [COSMIC build configuration](./graphics-windowing.md#cosmic-build-configuration).

### Native entry point and image writer

The native entry point and image writer have host tests:

```sh
cargo test --locked -p ferric-build -p ferric-config
cargo test --locked -p redox_cookbook --lib
cargo test --locked -p redox_installer --no-default-features --features installer
bash scripts/ferric-ci/test-native-toolchain.sh
bash scripts/ferric-ci/test-llvm-config-wrapper.sh
cargo xtask --dry-run build
```

The C++ prerequisite tests cover successful compilation, missing standard-library
headers with the compiler's diagnostic, a missing compiler and the Podman bypass.
Their stand-in compilers pass a script to an existing shell executable. They do
not execute newly written fixture files, which can fail with `ETXTBSY` when
parallel subprocess creation briefly inherits a writable fixture descriptor.
The production check still invokes the selected C++ compiler directly.

The shell regression uses a small synthetic runtime archive and mock Rust/Clang
tools, with no network or package installation. It checks compiler exclusion,
argument quoting, explicit LLD/Redox settings and rejection of unsupported
targets. Both x86-64 and ARM64 adapters are checked, including ARM64's Cargo
build-std settings. Mock QEMU checks cover ARM64 live/software defaults, explicit
HVF selection, x86-64 emulation on Apple Silicon, image selection and missing
ARM64 firmware. They also check the Raspberry Pi ROM exception, native versus
unconfigured CPU defaults, and explicit CPU model precedence. CLI tests cover
host defaults, explicit architecture selection, aliases and legacy `ARCH=` overrides.
The script also uses Make's actual native-prefix prerequisite list to check
invalidation after header, manifest, configuration, shared-crate, submodule and
directory changes, while ignoring generated caches. The installer tests
round-trip guest owners and modes through RedoxFS without host `chown`, including
repeated declarations with omitted or partial IDs. Cookbook tests cover
source-cache exclusion and native sysroot invalidation with mixed-age or missing
package variants. These host checks do not prove a complete OS boot: also run
`cargo xtask build` and `cargo xtask run --headless` for image changes, and
report the build and boot results separately.

On Apple Silicon the default build/run pair exercises ARM64 live/software
emulation. Also use
`cargo xtask --arch x86_64 build` and `cargo xtask --arch x86_64 run --headless`
to check cross-compilation and software emulation. A build for one architecture
does not validate the other's boot path.

### ARM64 firmware clock regression

`cargo test --locked -p ferric-boot-time` checks firmware calendar validation,
Gregorian leap years (including century rules), fractional seconds, both UEFI
timezone signs, unspecified timezone and daylight metadata, malformed handoffs,
and the counter offset across delayed kernel entry. The crate is `no_std` and
shared by the ARM64 bootloader and kernel.

For a target check, add `ferric-clock-test = "source"` to the packages in a
native image overlay, then build and boot it as in the signal test below.
Pass an externally known Unix epoch range to `ferric-clock-test MIN MAX`;
using the guest's own date as the expected value cannot detect this bug.
The test checks realtime against that range, verifies monotonic remains
independent, sleeps for 200 ms, and compares both clocks' elapsed time.
Require `FERRIC_TEST_PASS clock` and a zero exit status.

To make the check reproducible, use a QEMU wrapper with a fixed UTC RTC and
the virtual clock (substitute the installed QEMU executable if necessary):

```sh
cat > build/qemu-clock <<'EOF'
#!/bin/sh
exec qemu-system-aarch64 -rtc base=2024-02-29T12:34:56,clock=vm "$@"
EOF
chmod +x build/qemu-clock
cargo xtask --arch aarch64 run --no-build --headless -- QEMU="$PWD/build/qemu-clock"
```

After login, run `ferric-clock-test --offset 1709210095 1709210096`.
This checks realtime minus monotonic against the seeded Unix epoch
1709210096, allowing the RTC's one-second resolution. It detects both an
uninitialized epoch and adding firmware uptime twice. `date -u` and service
logs should show February 29, 2024 plus elapsed virtual time. Also boot with
the normal QEMU executable and check realtime against the host's current UTC
epoch (allowing a few seconds for RTC resolution and command delivery).
These checks exercise the UEFI/ACPI handoff; they do not validate a physical
board's RTC, network time synchronization, or the device-tree fallback.

### ARM64 signal-return regression

`crates/relibc/tests/signals/registers.c` holds a source pointer in x1 and a
small byte count in x2 while another thread sends 64 process-directed
`SIGUSR1` signals. A release/acquire handshake forces every delivery into
that assembly region, where no C call could legitimately clobber the
registers. The handler deliberately clobbers q0/q1 and FPCR/FPSR. The receiver
checks both integer registers, the vector data and the floating-point control
and status after each handler returns, then copies and verifies the data.
An alarm bounds a stalled test.
Other architectures print a skip, which does not count as ARM64 coverage.
The test is registered in relibc's static and dynamic C test suites.

On Redox, a second 64-round handshake sends signals while the receiver calls
`sched_yield` with x0 seeded to `-1`. Every call must return zero. This catches
the kernel saving a signal's x0 before the completed syscall result has been
published, which otherwise made relibc's startup host check abort.

To include it in a native ARM64 development image without changing the saved
configuration, create an overlay under the ignored build directory:

```sh
mkdir -p build/signal-test
cat > build/signal-test/filesystem.toml <<'EOF'
include = ["../../config/native.toml"]
[packages]
ferric-signal-test = "source"
EOF
cargo xtask --arch aarch64 build -- FILESYSTEM_CONFIG=build/signal-test/filesystem.toml
cargo xtask --arch aarch64 run --no-build --headless
```

Log in, then run:

```sh
ferric-signal-test
ion /usr/lib/ferric-signal-launch.ion
```

Require `FERRIC_TEST_PASS signal-registers`,
`FERRIC_TEST_PASS signal-syscall-result` and
`FERRIC_TEST_PASS signal-launch`, with no exception from the tested commands.
The launch script checks the output of `ls -d /usr/bin` and a nested
`ion -c true` in each of 128 rounds, failing on the first changed output or
failed launch. This covers actual dynamic startup alongside the deterministic
register test. The native ARM64 live-image/storage limitations still apply.

For the native fork regression, log in at the serial console with the accounts
from `config/base.toml`, then run:

```sh
ion -c 'for n in 1 2 3 4 5 6 7 8; ion -c true; end'
fox verify --installed
```

Booting through init, mounting the root filesystem, reaching a shell and
launching nested shells exercises the relibc fork handoff under release
optimization. The x86-64 child trampoline must preserve its descriptor buffer
and saved registers while calling Rust. Package verification additionally
checks that the rootless image's installed payloads remain intact.

A full `cargo test --locked --workspace` also enables the imported Redoxer
execution/FUSE feature. On macOS that optional tool requires a libfuse setup;
the native build and the targeted commands above require neither FUSE nor
macFUSE.

## Focused regression checks

### Scheduler baseline accounting

The trace CLI's `counts::tests` includes a multi-CPU capture with unequal rates
and a merged rate over the same interval. The ring-test recipe's host tests
(`cargo test --locked --manifest-path recipes/tests/ferric-ring-test/files/Cargo.toml`)
exercise the same CQE validator used by the target workload, rejecting missing,
duplicate, wrong-request, failed and short completions. The scheduler benchmark
image/boot remains the check for measured wake latency and real completions.
`bash scripts/ferric-ci/test-check-sched-bench-log.sh` checks that the baseline
log validator accepts the actual trace-benchmark workload/state format and
rejects missing measurement states, loss accounting or completion markers.

### Kernel heap regions

`cargo test --locked -p ferric-heap` exercises exact layouts/alignment, initialized
live contents, rejected growth/reinitialization, unaligned and tiny extensions,
cross-increment coalescing, separated small free holes and synchronized threads.
`cargo miri test --locked -p ferric-heap` checks the same pointer-ownership paths.
Host CI includes the crate's tests, Miri and strict Clippy/Rustdoc.

Run `cargo run --locked --release -p ferric-heap --example fragmentation_bench`
for a backend-only comparison with the former first-fit allocator. Both backends
keep small holes separated by live barriers before a large free tail. Report cost
at each hole count, including setup/host conditions. This measures neither SMP
lock waiting nor the mapping/physical-allocation paths; constant bookkeeping is
not a bound for a complete kernel allocation.

Native changes require full image builds and SMP/uniprocessor boots, including
page-cache fill/close, fault/fuzzer teardown and installed-package verification.
A timeout must preserve its CPU/context samples and be investigated; a passing
host allocator test does not establish native progress or IRQ safety.

### Portable executor core

`cargo test --locked -p ferric-exec-core` covers worker sleep/notify publication
races, detached cancellation, live-only deadlines, stale and invalid ids, and
bounded notification storage. Paused producers are coordinated by channels,
without depending on timing. `cargo miri test --locked -p ferric-exec-core --lib`
runs the same state/race checks. Host CI includes it with strict Clippy/Rustdoc.
The crate is not linked into the kernel yet; these tests do not exercise actual
interrupts, scheduler wait registration or timer hardware.

### Measured scheduler accounting

`cargo test --locked -p ferric-sched-core` covers real elapsed execution,
explicit yields, short readiness events, slice exhaustion/overrun, blocking,
fractional progress, CPU-clock population changes, wide virtual time and
signed-lag migration. An irregular-event model checks weighted competing tasks.
Timestamped-population regressions cover a remote steal midway through an owner's
interval, a local observation sampled before that steal, both migration clocks,
idle/zero-weight intervals and full-width timestamps. They exercise the production
`FairClock` used by the kernel; the competing-task model still selects independently
of the kernel's run queue, direct handoffs and migration. Native fairness and
coordinated live priority reweighting remain
[#187](https://github.com/rexlunae/ferric-oxide/issues/187) integration work.
`cargo miri test --locked -p ferric-sched-core` runs the same host logic. Host CI
includes strict Clippy/Rustdoc. For arithmetic cost against the preceding
accounting, run `cargo run --locked --release -p ferric-sched-core --example
accounting_bench`; it is a host microbenchmark, not a kernel speed/latency claim.

Native validation must still build and boot periodic scheduling on one and
multiple CPUs, with one-shot hardware both enabled and disabled, preserving time,
futex, affinity, signals, rings and direct scheme calls. Model fairness does not
prove native fairness, IRQ safety or the Phase S adoption criteria. The scheduler
still has allocating queue paths, a separate context-timer tree and periodic
interrupt delivery; none is made tickless by the accounting extraction.

### Shared timeout queue

`cargo test --locked -p ferric-timed --features alloc` covers monotonic/realtime
ordering and projection, forward/backward wall-clock changes, duplicate dates,
earliest cancellation, tokens from another queue or an earlier queue lifetime,
owner-wide cancellation, identifier exhaustion, dates beyond
the hardware `u64` horizon, arithmetic saturation, concurrent consumers under
one owner lock and exactly-once resource drops. A pending-set model checks mixed
insert/cancel/expiry sequences across both clocks. Run the allocation-free
arithmetic build separately with `cargo test --locked -p ferric-timed`.
An integration test instruments the actual global allocator and verifies zero
calls during due checks, expiry through the last entry, and cancellation with
nonallocating payloads. A calibration free confirms the instrumentation is active.
Host CI runs both configurations and Miri with `alloc`, and the queue must pass
Clippy and warning-free Rustdoc with `--all-features`.

For a reproducible queue-only before/after comparison, run:

```sh
cargo run --locked --release -p ferric-timed --features alloc --example timeout_bench
```

The example compares the former vector scan against ordered storage at 1, 64
and 4096 pending timers. It reports registration and expiry cost per event and
cost per pass with no due event. Ordered lookup also adds heap insertion/removal
work, so report those costs alongside scan savings. These numbers do not measure
kernel locks, event callbacks, hardware timers or scheduler latency.

Kernel validation must build with `kernel.time.one_shot` enabled and disabled,
then boot the corresponding image and exercise time-scheme events. Inspect
`timeout_subscribers`, `timeout_next_monotonic_ns` and
`timeout_latest_monotonic_ns` in `/scheme/sys/stat`. Dates are absolute monotonic
nanoseconds or `none`. The latest-date diagnostic scans the registry; it is not
used for rearming. These fields cover only time-scheme registrations and do not
prove the scheduler has stopped its tick.

The smoke image's `ferric-time-test` covers both clock domains, distinct owners,
non-final descriptor closes, final-close cancellation, multi-record writes
racing close, and due-timer event delivery through `/scheme/event/`. Distant
sentinel deadlines let it check that all of an owner's timers disappear while
ordinary driver and network timers remain live; it does not assume a globally
empty queue. It prints `time-ownership`, `time-close-race` and `time-expiry` pass
markers, required by image CI. The recipe also passes target Clippy/Rustdoc.
Recipe-only pull requests trigger these host-tools timer checks through the
workflow's path filter.
This is regression coverage for
[#362](https://github.com/rexlunae/ferric-oxide/issues/362); bounded IRQ execution
and the full S9 teardown/failure matrix remain separate gates.

The smoke suite mounts its `ext4-erofs` fixture with `ext4d --read-only`,
including when `fs.ext4_write=true`. It checks `EROFS` and verifies that the
backing image is unchanged after the daemon stops. The independent `ext4-write`
step uses the daemon's default policy and checks writing, truncation and regular
file creation when `ext4d --features` advertises `write`; only an absent compiled
capability skips that step. The journal fixture keeps the default policy and
still requires on-disk replay when enabled. Failed checks or daemon teardown
retain their backing images (`/tmp/ferric-ext4-test.ext4`,
`/tmp/ferric-ext4-journal.ext4` and `/tmp/ferric-ext4-write.ext4`) for diagnosis;
only successful checks and teardown remove them, and removal errors fail the
step. The recipe's host tests cover retained images and original errors after
simulated check/stop/reap failures, successful removal and typed removal errors.
These distinguish mount policy from
build capability and cover the mismatch in
[#364](https://github.com/rexlunae/ferric-oxide/issues/364). Host tests of the
daemon's actual startup cover all three feature combinations on committed and
fresh images, including byte-for-byte preservation of a dirty read-only volume.

### Delayed page-cache writeback notifications

RedoxFS's host library tests (`cargo test --locked --manifest-path
userspace/redoxfs/Cargo.toml --lib --no-default-features --features std`)
also exercise closed-file cache count/byte limits, extreme sizes, zero budgets,
stale stamps and unsuccessful reopen attempts. Transaction tests identify
rename targets losing their last link with and without an open description,
and distinguish surviving hard links, same-node renames and absent targets.
These test the shared lifetime decisions; image tests remain necessary for
actual kernel cache-object attachment and page reuse.

`ferric-pcache` also checks its 40-byte queue record against literal little-endian
fixtures, including every writeback reason and unaligned input. The generated
wire layout is shared by kernel and file-system consumers.

`cargo test --locked -p ferric-pcache --all-features` includes the delayed
notification regression: fsync cleans a file before its record is delivered,
writes race on either side of the current-count query, duplicate records keep
the first dirty time, and another dirty file keeps its timer. ABI tests cover
the `DirtyPages` request and reject extra metadata/packed flags. CI compiles
the RedoxFS client and kernel query together; the writeback-enabled image's
fsync, age, mapping and truncation smoke checks cover target integration.

The shared page-cache writeback tests also cover delayed sequential-run hints:
an intervening flush cleans the recorded pages, then the hint writes just one
later dirty run while a concurrent rewrite remains dirty. This pins the
intentional start-hint semantics and the take-size work bound.

### Network driver startup errors

`cargo test --locked --manifest-path userspace/base/Cargo.toml -p driver-network
-p common` covers how a network driver starts and reports failure. The
`driver-network` tests drive the setup order behind `NetworkScheme::new` with
injected failures: a socket or wake-pipe failure returns `Socket` or
`WakePipe` without running the adapter initialiser, a registration failure
returns `Register` naming the scheme, and on success the steps run as socket,
wake pipe, adapter, registration. The `common` tests check that
`error_chain` lists an error and every cause, outermost first, which is what
`exit_with_error` logs. The real socket and registration calls need a Redox
target, so they are only compile-checked here.

### Driver executor event reads

`cargo test --locked --manifest-path userspace/base/Cargo.toml -p executor`
tests the event-read adapter used by queue-based drivers, including NVMe.
An interrupted event read (`EINTR`) is retried without dispatching an event;
other queue errors remain fatal. Regressions cover repeated interruptions,
the eventual event's complete payload, immediate success and terminal errors
before or after an interruption. `cargo test --locked -p ferric-exec-core
--test driver_event_read` includes the same driver module, so its portable tests
also run on hosts without libredox event support. The host-tools driver job runs
the executor package tests, nvmed's host tests (the interrupt masking choice for
pin-based, MSI and MSI-X vectors), pcid's host tests (one MMIO write per MSI-X
mask or unmask, counted on a mock register) and compiles the NVMe configurations below. Host mocks do not establish why
a guest read was interrupted; repeat image boots remain the target check. The
reproduced spurious interruption in
[#308](https://github.com/rexlunae/ferric-oxide/issues/308) came from the
scheduler returning into a blocked NVMe handler after retaining a client that
had just become runnable. The scheduler now hands execution back to that
locked client. The retry remains correct for legitimate signal interruptions.
Repeat image boots and repeated `ferric-blkgrant-test` worker-policy runs
exercise the real parking/handoff path; retain the first failure and its logs.
Linux KVM/Kleya confirmation remains required.

### NVMe command identifiers

`cargo test --locked -p ferric-blkqueue` tests the identifier pool used by every
NVMe queue pair. The regressions retain commands while submission positions wrap,
complete commands out of order, cross the entire 16-bit identifier range while
one command remains outstanding, exhaust and resume the pool, reject duplicate
completions, and keep queue pairs independent. The pool is also checked without
`std` or `alloc`; its doctest and strict Clippy/Rustdoc checks run in host-tools CI.
`cargo test --locked -p ferric-exec-core --test driver_completion` compiles the
driver's actual completion dispatch on the host: live completions wake their
owner and one FIFO submitter, cancelled completions still wake a submitter,
and empty queues do not produce extra wakeups. The same tests run in the
`executor` package.

The driver job compiles the default NVMe driver, `perf`, and
`perf,percore,grant-dma` configurations. The target check remains an image build
and a boot with writes large enough to reuse submission positions, followed by
filesystem readback. Preserve a failing boot's first serial log and NMI context
dump rather than treating a later successful boot as proof the failure was fixed.

The NVMe package's host tests also cover completion publication:
`cargo test --locked --manifest-path userspace/base/Cargo.toml -p nvmed`.
A deterministic controller publishes a new completion between a premature
payload read and the phase read, for both phase values. Reading the phase first
must deliver the new command identifier; an unpublished slot must not be read.
A byte fixture checks the generated little-endian completion layout.

The profile-image boot at `95b031f0e` retained a driver abort during storage
stress on a completion whose command identifier was not outstanding.
The old x86-64 binary loaded the CID before status; its phase check therefore
could accept a torn snapshot. One unmodified local boot also passed, confirming
that a later pass alone does not establish a fix. The deterministic regression
and phase-first implementation address that publication race; retain the full
profile-image smoke boot as the target check. The profile workflow also runs
when NVMe completion/command code, the command-ID pool or the executor changes.
The corrected native profiler image at local integration `b880ecc74737` completed
the full KVM smoke boot, including storage stress, profile collection/flamegraph,
fuzzing, signed package verification and shutdown. Both additional profile
workflow checks (fox manifests and loaded-image/profile records) passed. This
used the CI profiler/frame-pointer settings with a native source rebuild.

Local validation on 31 October 2026 reproduced a first-boot `fox` database write
stall before the fix: seven disk-ring operations remained without completions
after the NVMe hardware queues and executor completion maps had drained. With
the identifier fix in [#352](https://github.com/rexlunae/ferric-oxide/pull/352),
the combined x86_64 `wayland` image built and reached login under KVM (four
vCPUs, 4 GiB, snapshot disk writes). A 64 MiB regular-file write followed by a
complete SHA256 readback matched the host-computed digest. An additional piped
`dd` comparison delivered only 4 MiB and failed, exposing the separate output
short-write bug in [#353](https://github.com/rexlunae/ferric-oxide/issues/353).
The recipe correction and successful full-transfer checks are documented under
[Coreutils pipe transfers](#coreutils-pipe-transfers).
This establishes local boot and file-I/O evidence, not hardware performance.
The final combined build and boot/readback check was repeated after the
cancellation-wakeup correction and the compiler-fixture fix in
[#354](https://github.com/rexlunae/ferric-oxide/pull/354), at local integration
`54c17278747c` (NVMe/executor source `a080d862e41e`). The cancellation case is
covered by the host regression; the guest check exercises startup and file I/O.

### NVMe phase accounting

`cargo test --locked -p ferric-blkqueue --all-features` tests the actual
request-owned timing and resettable reporting windows used by
`drivers.nvmed_perf`, including interleaved requests, multiple commands,
zero-length durations and overflow. The host-tools driver job also compiles
nvmed's plain, timing-only and timing+percore+DMA configurations for Redox.
This checks accounting and feature integration; controller timing and idle-burst
reporting still need an instrumented target boot.

### NVMe request lifetime accounting

`ferric-blkqueue`'s activity tests cover claims surviving inline completion,
cross-thread worker completion and failed delivery. The existing Redox driver
builds cover percore, perf and grant-DMA integration. Smoke CI retains the
serial Adaptive default and its enforced same-boot queue performance gates.

With its `std` feature, `ferric-blkqueue::round` supplies the shared start gate
for the registered-buffer and iobench benchmarks. Host tests cover synchronized
start, caller-prepared state, excluding destruction and joins, worker panic,
thread creation failure, and releasing waiting submitters when the start hook
panics. Run `cargo test --locked -p ferric-blkqueue --all-features` plus the two
recipe runner tests in `host-tools.yml`. Raw per-round `FERRIC_BENCH_SAMPLE`
lines preserve the paired observations; the 5–15% tolerance and three-plus-six
round gate are unchanged. The original AMD-Vi failure is tracked in
[#359](https://github.com/rexlunae/ferric-oxide/issues/359).


### Profiler CPU wakeup acknowledgements

The host kernel helper suite includes `context/idle.rs`, used by the real
`try_idle_context` accessor and x86 wakeup handler. It verifies an uninitialized
idle slot returns `None`, then checks a pending shootdown is serviced before
EOI without reading current-context state. Ordinary idle/running CPU decisions
and exactly one EOI are checked too. These callbacks test dispatch/order, not
TLB hardware; the profile-image boot remains the on-target regression, and
profiling plus CET shadow-stack teardown still needs hardware validation.

### Lazy file-mapping reply protocol

The kernel helper suite includes `context/fmap_reply.rs` directly. It covers
reply-before-park, park-before-reply, exactly-once consumption, stale/duplicate
responses and failed-dispatch cleanup. Normal `fmap` still creates eager
mappings, and no scheme responds to `RequestMmap`; this protocol coverage is
not an end-to-end lazy-fault test. Run the helper host and Miri commands above.

## Socket ancillary-data headers

`bash scripts/ferric-ci/test-socket-header.sh` regenerates relibc's socket
header and its required type headers with `cbindgen`, then compiles and runs
`crates/relibc/tests/sys_socket/cmsg_nxthdr.c` on the host with
`-Wsign-compare -Werror`. It checks aligned traversal, a final header that
fits exactly, truncated buffers and invalid current-message lengths without
linking relibc or booting a guest. Set `CC` to select the host C compiler.
The image workflow runs this check before building the image; the C test also
belongs to relibc's regular test suite.

## Python and guest build-tool regressions

`python3.12 scripts/ferric-ci/test-python-build-tools.py` exercises the cookbook's
Python setup with a compiled host extension and an incompatible target copy.
It requires Python 3.12, its development headers (`python3.12-dev` on the CI
runner) and a C compiler. It rejects other Python versions and keeps the
prescript's `COOKBOOK_PYTHON_V` unchanged to check the recipe's actual ABI.
`bash scripts/ferric-ci/test-build-tools-manifest.sh`
checks that recipe placement edits regenerate the guest-tool list, while an
unchanged list preserves the cached configuration. Both run in `host-tools.yml`
without downloading or building recipes. The cookbook's Rust tests cover
Python recipe placement and cleaning the resolved tool target. The
`guest_build_dependencies` integration test runs `repo cook` over local shell
recipes: a guest-built tool invokes an installed host-only dependency, keeps
target libraries in its link sysroot, and receives shims for a nested guest
tool. It uses signed package creation and installation plus the real build
prescript, replacing only the compiler-environment wrapper; it needs no
compiler or guest boot. See
[build-tool placement](./ferric-build.md#build-tools-in-the-guest) for the policy.

## Port-I/O authorization

`ferric-ioport-test` runs on x86_64 Redox in the smoke image. It starts separate
children to request port access as root (must succeed), as user 1000 (must
return `EPERM`), and through a handle opened as root before dropping the
effective UID to 1000 while keeping real UID 0 (must also return `EPERM`).
The children never execute `IN` or `OUT`; this tests authorization, not device
access or revocation of rights enabled earlier. Other targets print a skip.

Compile and lint the target test independently with:

```sh
cargo clippy --locked --manifest-path recipes/tests/ferric-ioport-test/files/Cargo.toml \
    --all-targets --target x86_64-unknown-redox -- -D warnings
```

`host-tools.yml` checks its formatting, target compilation, Clippy and
Rustdoc. The image workflow builds the recipe and boots it in its smoke
matrix. Kernel changes also require the kernel lint ratchet; host compilation
alone does not exercise the credential check.

## ramfs ownership changes

`ferric-owner-test` runs in the smoke image and checks `ramfs`'s `fchown` and
`fchmod` (#246), its `write` access check (#400), and its `futimens` and
`ftruncate` (#375), on `/scheme/logging`, the instance every image runs. As
root it creates a file root owns that anyone may open for writing, one root
owns that only root may write, and one user 1000 owns with mode `6755`, then
runs itself as user 1000:

- `owner-refused`: on root's file, `fchown` to its own user or group and
  `fchmod` are refused, and root then finds the file unchanged;
- `owner-allowed`: on its own file it may set its own group, which clears the
  set-ID bits, and change the mode, but not give the file to root; root then
  finds the owner, group and mode it set;
- `owner-write`: `write` through a descriptor it opened read-only is refused
  (`EBADF`), even on its own file, and allowed through one it opened
  write-only on root's writable file; root then finds its own file empty and
  root's starting with the bytes written (#400);
- `owner-times-refused`: `futimens` with explicit times on root's files is
  refused (`EPERM`), and so is setting both to the current time on the file it
  may not write (`EACCES`); root then finds that file's times unchanged;
- `owner-times-allowed`: it may set both times of root's writable file to the
  current time, and its own file's access time while `UTIME_OMIT` keeps the
  modification time; root then finds exactly those times, the current ones
  within the realtime clock readings it took before and after the child;
- `owner-truncate`: `ftruncate` through a descriptor opened read-only is
  refused (`EBADF` or `EINVAL`), even on its own file, and allowed through one
  opened for writing, which extends root's file past the bytes `owner-write`
  wrote; root then finds the sizes it expects and the extension zeroed.

`fchown` reaches `ramfs` as the kernel's direct call, and `fchmod`, `futimens`
and `ftruncate` as a `StdFsCall`, so the steps cover both ways `ramfs` learns
the caller's IDs. The test takes the `UTIME_*` sentinels from
`ferric_abi::time`: the `libc` crate's values for Redox differ from relibc's.
Without `/scheme/logging` it prints a skip. The rules themselves have host
tests in `crates/abi` (`ferric_abi::id` and `ferric_abi::time`) and
`userspace/base/ramfs`, which the base-workspace job in `host-tools.yml` runs
with `scheme-utils`' credential decoding. `host-tools.yml` also checks the
test's formatting, target Clippy and Rustdoc; the image workflow requires all
six steps.

## Documentation checks

The system chapters are ordinary mdBook pages listed in `docs/src/SUMMARY.md`.
From `docs/`, run:

```sh
./unused_pages.sh
mdbook build
```

`lychee src` is the book's optional link-checking command. If those tools are
unavailable, report that limitation and at least check local page links,
anchors, source paths and table-of-contents coverage. Do not report an unrun
render or OS boot as successful.

### TLB counters

`cargo test --locked -p ferric-trace` covers the `/scheme/sys/tlb` line format
(`tlb::tests`: round trip, refused lines, wrapping deltas, the per-read
figures `ferric-iobench` prints) and the catalog checks that keep the TLB
events at debug level under `trace_mm` and `trace_iommu`. The iobench runner's
host tests check that the round's start hook (where the first snapshot is
taken) runs once, before any work. The kernel side is checked by building it:
Clippy for the kernel target with `--features tlb_stats` and with the debug
trace levels, and with `iommu = "auto"` so the IOTLB accounting compiles, plus
the lint ratchet for the default build. The counts themselves need a boot
(`kernel.tlb_stats = true`; read `/scheme/sys/tlb`, or run `ferric-iobench`).
