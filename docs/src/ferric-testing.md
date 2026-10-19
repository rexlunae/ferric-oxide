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
`userspace/redoxfs/`; that is not covered by the root workspace test.

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
`tools/fox/tests/git_sources.rs`, `crates/fox-git/tests/repo.rs`,
`crates/fox-git/tests/tags.rs`, `crates/fox-build/tests/build.rs` and
`tools/cargo-fox/tests/cli.rs`. Tests construct real signed pkgar fixtures
through `fox-core`'s `test-fixtures` feature. The git and build tests create
repositories with the `git` command and build a small path-only workspace
with Cargo, so they need `git` and the rustup default toolchain but no
network; fetches use `file://` URLs. Release tags are signed in Rust with
fixed-seed Ed25519 keys rather than with `ssh-keygen`, and `git fsck
--strict` checks the tag objects. `fox-git` pins a known `fox-tree/1`
hash. The
IOMMU vendor crates include known QEMU DMAR/IVRS byte fixtures under
`crates/vtd/tests/fixtures/` and `crates/amdvi/tests/fixtures/`.

## Kernel lint and binary checks

| Check | Purpose |
| --- | --- |
| `scripts/ferric-ci/kernel-lint-ratchet.sh` | Holds per-lint Clippy counts and `unsafe` uses at or below `ci/kernel-lints.json` |
| `scripts/ferric-ci/check-kernel-relocs.sh` | Builds and compares relocation replay against a kernel linked at the shifted address |
| `scripts/ferric-ci/check-kernel-ibt.sh <kernel.all>` | Checks `endbr64` at selected entry points in an IBT-built kernel |
| `scripts/ferric-ci/check-trace-overhead.sh --out LOG... --in LOG...` | Compares `ferric-trace-bench` results from boots without and with `kernel.trace` against the tracing overhead budget |

The kernel workflow installs its declared nightly toolchain, `rust-src`,
Clippy and `nasm` as needed. Use the kernel Makefile/custom target path;
ordinary host `cargo test` is not a kernel boot test. When removing warnings,
run the ratchet with `--update` and commit the lower baseline. The workflow
contains the exact preparation for an IBT-configured binary.

The workflow also builds kernel Rustdoc with warnings denied and `regbuf`
and the tracing features enabled, for both `kernel.iommu = "never"` and `"auto"`. This covers common
kernel documentation and the optional DMA-grant APIs. After generating and
copying the desired kernel configuration, reproduce the check from
`crates/kernel/`:

```sh
RUST_TARGET_PATH="$PWD/targets" RUSTDOCFLAGS="-D warnings" \
  cargo doc --locked --no-deps --features regbuf,trace_syscall,trace_sched,trace_scheme,trace_level_info \
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
| `ferric-futex-test` | Compare/wait/wake races and expired waiters |
| `ferric-regbuf-test` | Registration, file-backed refusal, access modes, pinning, re-sharing refusal, forwarding, revocation, unmap/close, shared memory and byte/bounce behavior |
| `ferric-blkgrant-test` | `nvmed` DMA into an attached grant: reads and writes against `pread`, refusals, revocation, detach and a copy versus grant read benchmark |
| `ferric-trace-test` | Boot-time and live tracepoint enable, decoding of every CPU's ring, system call enter/exit pairs and results, scheme request/response matching by SQE tag, timestamp order and disable |
| `ferric-ring-test` | `kernel.ring` setup and refusals, `Nop`/`Read`/`Write`/`Fsync` through a ring, a chain across RedoxFS, the pipe scheme and the null scheme in one `Enter`, broken and overlong chains, incomplete chains, CQ back-pressure, a corrupt SQ tail, `EVENT_READ` on completion, and ring versus system call timings |
| `ferric-trace-bench` | Times RedoxFS requests and null system calls with tracing compiled out, compiled in but off, and every event on, for the overhead budget |
| `ferric-iommu-test` | QEMU edu device DMA, domain ownership, permissions, invalidation, fork isolation, grant/sub-grant access and revocation |
| `ferric-syscall-fuzz` | Seeded multi-process syscall fuzzing and validation of invalid inputs |

The edu driver is started through `pcid-spawner`, separately from the console
test script. Its PCI matching configuration is in the IOMMU test recipe.
Tests may skip absent optional features; do not interpret a skip as coverage.

`.github/workflows/image.yml` generates feature settings, forces KPTI on,
enables IOMMU `auto`, shadow-stack preparation `auto`, registered buffers,
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
posts the benchmark as a notice. Read that script for the exact required marker set.
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
q35, four CPUs, 2 GiB RAM, fixed PCI slots for NVMe, e1000 and edu, and serial
output. It uses KVM with `host` when available and otherwise TCG with `max`.
`BOOT_TIMEOUT` changes the default 300-second limit. `BOOT_IOMMU=vtd` requests
interrupt remapping and a split irqchip. AMD DMA remapping requires QEMU 10 or
later; the workflow uses a newer-QEMU container for that variant.

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
