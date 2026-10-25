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
job tests `userspace/ext4d/` twice, on the committed ext4 image in
`tests/fixtures/` and on one `make-fixtures.sh` builds with the runner's
`mke2fs` (`EXT4D_FIXTURES`), and lints and documents it for the host and for
`x86_64-unknown-redox`.

Host, kernel and image jobs use `.github/actions/prepare-cargo-git` before
running Cargo. It enables Git's fetch client, caches pinned Git dependencies and
rewrites the verified `pkgar` and `pkgutils` URLs to their GitHub mirrors, retaining
the lockfiles' exact commit IDs. Other mirrors can lack current commits, so their
upstream URLs remain in use with increased retries. This also covers Cargo
dependency resolution during feature generation; successful submodule checkout
alone does not establish that Cargo can reach GitLab.

`cargo test --locked -p ferric-config` includes smoke/fuzz profile regressions
in `tools/ferric-config/tests/profiles.rs`. They exercise the public CLI and
generated Makefile, then load the selected filesystem configuration through the
installer's include resolver. They check inherited source packages, the effective
boot script, image overrides and retention of saved CPU/kernel settings.

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

`ferric-lvm` is a prepared library that nothing in the image uses yet, so its
host tests are its only coverage. `crates/lvm/tests/fixtures.rs` reads PVs
that lvm2 wrote (`crates/lvm/tests/fixtures/README` has the commands): a
two-PV striped group, metadata text that wraps round its area, and a volume
extended after another one. It checks the parsed groups and mappings against
`vgcfgbackup`, flips a bit in every byte the reader depends on, and edits labels
and metadata-area headers to test areas that are empty, broken or past the
device.

`ferric-luks` is a prepared library that nothing in the image uses yet, so its
host tests are its only coverage. `crates/luks/tests/fixtures.rs` reads LUKS1
and LUKS2 headers that cryptsetup wrote (`crates/luks/tests/fixtures/README`
has the commands) and checks the recovered volume keys against
`cryptsetup luksDump --dump-volume-key`, header-copy fallback and random
corruption. Because the bootloader is a soft-float UEFI target, CI also builds
it with `RUSTFLAGS="--cfg aes_force_soft"` and `--features force-soft` for
`x86_64-unknown-uefi`; check that build when changing its dependencies.

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
| `scripts/ferric-ci/check-kernel-ibt.sh <kernel.all>` | Checks `endbr64` at selected entry points in an IBT-built kernel |
| `scripts/ferric-ci/check-kernel-shstk.sh <kernel.all>` | Checks shadow-stack enable, context-switch and syscall alternatives, WRSS, and the #CP handler with IBT on/off |
| `scripts/ferric-ci/check-trace-overhead.sh --out LOG... --in LOG...` | Compares `ferric-trace-bench` results from boots without and with `kernel.trace` against the tracing overhead budget |

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

## Image tests

`config/ferric-smoke.toml` includes the minimal image and forces local source
builds of kernel, relibc, bootloader, RedoxFS, base and Ferric test packages,
even when the surrounding build uses prebuilt packages. Its init script emits
`FERRIC_SMOKE_BEGIN`, runs the tests, emits `FERRIC_SMOKE_OK` and shuts down.

| Recipe under `recipes/tests/` | What it exercises |
| --- | --- |
| `ferric-xstate-test` | SIMD/extended register-state preservation across execution |
| `ferric-aslr-test` | Address variation between program executions |
| `ferric-guard-test` | Thread/main-stack guard faults and accessible stack controls |
| `ferric-tlb-test` | Mapping/protection changes and TLB invalidation across CPUs |
| `ferric-borrow-test` | A scheme call whose buffer cannot be borrowed leaves the caller's shared file mappings unpinned; RedoxFS maps a written or reused file with its current contents, `read` sees stores through a live shared mapping, `fsync` does not write an unmapped page back over a later `pwrite`, and a `pwrite` reaches live shared mappings; prints `FERRIC_BENCH borrow-pwrite`, page writes to an unmapped and a mapped file |
| `ferric-futex-test` | Compare/wait/wake races and expired waiters |
| `ferric-scheme-self-test` | A process opens and reads a file on a scheme it created on the same thread and serves from another, which used to spin the caller's CPU in the kernel |
| `ferric-regbuf-test` | Registration, file-backed refusal, access modes, pinning, re-sharing refusal, forwarding, revocation, unmap/close, shared memory and byte/bounce behavior |
| `ferric-blkgrant-test` | `nvmed` DMA into an attached grant: reads and writes against `pread`, refusals, revocation, detach and a copy versus grant read benchmark |
| `ferric-netq-test` | A queue socket on `e1000d` in copy mode (`drivers.net_bypass`): attach, refusals, ARP and ICMP exchanged with QEMU's gateway through the socket's own rings, netstack beside it, port ownership and refused filters, a DNS round trip from an owned port, spoofed TX frames dropped, a TX benchmark against netstack, counters, and close with traffic in flight |
| `ferric-iobench` | The Phase 2 disk benchmark: fio-style read jobs on the NVMe disk with each queue policy, IOPS, bandwidth and latency percentiles, and the same-boot A/B of per-CPU queues against one queue ([Disk benchmark](ferric-io.md#disk-benchmark-ferric-iobench)) |
| `ferric-pcache-test` | `kernel.pcache` with a file system scheme of its own: a miss filled through the queue, a hit with no scheme read, readahead after a sequential read, `EIO` from a failed fill and a retry, `EIO` (not a hang) from a done answer whose payload faults and the same answer arriving late ignored, leaving the retry alone, records put back after a queue read that faults, `EPERM` for a child process (attaching, or binding an unbound queue), a queue kept across `exec`, an invalidated page filled again and a stale fill not taken, an answer to a closed object's record ignored as stale (it neither fills nor fails the new object's fill of the same page) and no stale records after reopening it, and readers released with `EIO` when the queue closes, after which reads go to the scheme; prints a cached 4 KiB `pread` time beside the scheme's. Then on RedoxFS: a read-only description follows writes, truncations and an append through another one, a shared writable mapping keeps a file out of the cache exactly while it lasts (a private one changes neither the file nor its caching), and cached reads are benchmarked against RedoxFS's own in the same boot (`FERRIC_BENCH pcache-redoxfs-*`, paired A/B) |
| `ferric-ring-test` | `kernel.ring` setup and refusals, `Nop`/`Read`/`Write`/`Fsize`/`Fstat`/`Fsync` through a ring, the fixed-file table, asynchronous chains with `min_complete` waits and close, a chain across RedoxFS, the pipe scheme and the null scheme in one `Enter`, broken and overlong chains, incomplete chains, CQ back-pressure, a corrupt SQ tail, `EVENT_READ` on completion, and ring versus system call timings. `--async-only` selects the asynchronous chain and close probes used by `trace-lifecycle` to check kernel-worker identity restoration |
| `ferric-trace-test` | Boot-time and live tracepoint enable, decoding of every CPU's ring, system call enter/exit pairs and results, scheme request/response matching by SQE tag, timestamp order, mapping-operation results and target spaces (map/protect/unmap, peer context, refusals, validation and disable), queued futex wait/resume results and wake counts (32/64-bit timeout, wake-before-resume order, sustained empty-wake polling, mismatch and disable), ordered kernel context creation/start/exit for a child process, a thread and asynchronous ring workers (including restored PID 0), disable, and read-only mapping of the rings with `commit` checks |
| `ferric-ext4-test` | Mounts the `ext4d` fixture image (`fs.ext4`), lists the root, an empty and a hashed directory, reads files whole, at an offset and through a symbolic link, reads the link, checks `stat`, and checks that writing, creating, removing, renaming and `mkdir` fail with `EROFS` |
| `ferric-trace-bench` | Times RedoxFS requests and null system calls with tracing compiled out, compiled in but off, and every event on, for the overhead budget |
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

`.github/workflows/image.yml` generates feature settings, forces KPTI on,
enables IOMMU `auto`, shadow-stack policy `auto`, registered buffers,
`nvmed` grant DMA (`drivers.grant_dma`) and kernel tracing (with
`sched.switch` on from boot),
and builds a second kernel/image with IOMMU `enforce` and, for `ferric-smoke`,
a third without kernel tracing. Its five boot variants
are no IOMMU, VT-d auto/enforce, and AMD-Vi auto/enforce. It requires the
registered-buffer and tracing result markers, the trace boot log line,
KASLR/KPTI logs, shadow-stack detection and
futex wake-race marker, and checks PCID use when the runner exposes it.
`check-iommu-log.sh` checks driver/domain and device-test evidence for each
IOMMU mode, and `check-blkgrant-log.sh` checks the storage-grant steps and
posts the benchmark as a notice, and `check-iobench-log.sh` checks the disk
benchmark's lines and A/B and writes its figures to the job summary. Read those
scripts for the exact required marker set.
The `trace-overhead` job boots the images with and without tracing ten times
each, interleaved, and runs `check-trace-overhead.sh` on the twenty serial logs
(see [Kernel tracing](ferric-tracing.md#overhead-budget)).

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
```

The script expects QEMU, OVMF and Linux utilities. It uses a copy of the image,
q35, four CPUs, 2 GiB RAM, fixed PCI slots for NVMe, e1000 and edu, two HDA
controllers, and serial output. It uses KVM with `host` when available and
otherwise TCG with `max`. `BOOT_TIMEOUT` changes the default 300-second limit.
`BOOT_IOMMU=vtd` requests interrupt remapping and a split irqchip. AMD DMA
remapping requires QEMU 10 or later; the workflow uses a newer-QEMU container
for that variant. `OVMF_FIRMWARE` selects a firmware file instead of searching
the usual paths.

The script stops early instead of waiting out the timeout when the smoke shell
cannot start: ld.so reporting that it could not link `'/usr/bin/ion'` (the
quoted path it prints), or init reporting that `ion /usr/lib/ferric-smoke.ion`
failed, ends the run with that console error and `boot-smoke: FAIL, the smoke
shell could not start`. The linker line counts only for the shell: every program
reports the same prefix, so a program the script starts that fails to link stays
that program's test result instead of killing the guest. A boot that prints the
marker first still passes, including one that prints it while the runner watches
for the marker in the second after it saw such a line, so a link failure during
shutdown does not fail a green run. `scripts/ferric-ci/test-boot-smoke.sh`
exercises these cases against a fake QEMU, without firmware or virtualization.
This turns the intermittent boot in
[#155](https://github.com/rexlunae/ferric-oxide/issues/155) into a fast,
labelled failure; the underlying `NotFound` during early boot is still open.

The two HDA controllers exercise the audio scheme naming in `ihdad` and
`audiod`. The smoke script waits up to 30 seconds for the second controller's
`audiohw.<pci-address>` scheme and reports `audio-second-controller`: the
first driver keeps `audiohw`, so a machine with the chipset codec and GPU
audio controllers does not lose every controller to a failed registration.

`scripts/ferric-ci/test-boot-smoke.sh` exercises the runner itself with a fake
QEMU, so it needs neither firmware nor virtualization. It covers pass, skip,
failure, a crash after the marker, a missing test result, a missing marker, a
smoke shell that could not be linked (which must fail with that message rather
than a generic timeout), a link failure after the marker and a marker that
arrives while the runner waits after that line (both of which must still pass), a
program the script starts that fails to link (which must not fail the run), and
that the runner adds both HDA controllers. Its fake QEMU stays alive for a
moment after printing, like a guest still shutting down, so the watch loop
reads the log instead of finding QEMU already gone. The host-tools workflow
runs it before the Rust checks.

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
fallback boot, not this hardware result.

The harness rejects crashes and `FERRIC_TEST_FAIL` markers and requires the
smoke completion marker. A completion marker alone is insufficient because
the init script prints failures and continues to shutdown. Use the workflow's
additional log checks for feature-specific validation. Image variants without
the requested kernel features cannot exercise those paths merely by adding a
QEMU device.

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

The native entry point and image writer have host tests:

```sh
cargo test --locked -p ferric-build -p ferric-config
cargo test --locked -p redox_cookbook --lib
cargo test --locked -p redox_installer --no-default-features --features installer
bash scripts/ferric-ci/test-native-toolchain.sh
cargo xtask --dry-run build
```

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
