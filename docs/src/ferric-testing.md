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
cargo test --locked -p ferric-iommu --all-features
```

The workspace command includes members such as `ferric-hwrand`, installer
and Redoxer that are not all in `default-members`. Alloc-backed registered
buffer and IOMMU bookkeeping also need their feature-enabled runs. Check
`no_std` configurations separately when changing those libraries.

`.github/workflows/host-tools.yml` is the precise CI command list. It denies
Clippy warnings for the fork's own tools and libraries and builds selected
Rustdoc with `RUSTDOCFLAGS="-D warnings"`. Imported host tools still have
upstream lint warnings. It separately runs `cargo test --locked --lib` in
`userspace/redoxfs/`; that is not covered by the root workspace test.

Use `cargo clippy --all-targets -- -D warnings` and `cargo doc --no-deps`
in changed workspaces as required by `AGENTS.md`, accounting for the documented
imported-code exception. For feature-gated APIs, include the relevant feature
combination in tests, lints and docs. Do not hide a new warning by increasing
a baseline or disabling a check.

Package coverage is in `crates/fox-core/tests/archive.rs`,
`crates/fox-store/tests/store.rs` and `tools/fox/tests/cli.rs`. Tests construct
real signed pkgar fixtures through `fox-core`'s `test-fixtures` feature. The
IOMMU vendor crates include known QEMU DMAR/IVRS byte fixtures under
`crates/vtd/tests/fixtures/` and `crates/amdvi/tests/fixtures/`.

## Kernel lint and binary checks

| Check | Purpose |
| --- | --- |
| `scripts/ferric-ci/kernel-lint-ratchet.sh` | Holds per-lint Clippy counts and `unsafe` uses at or below `ci/kernel-lints.json` |
| `scripts/ferric-ci/check-kernel-relocs.sh` | Builds and compares relocation replay against a kernel linked at the shifted address |
| `scripts/ferric-ci/check-kernel-ibt.sh <kernel.all>` | Checks `endbr64` at selected entry points in an IBT-built kernel |

The kernel workflow installs its declared nightly toolchain, `rust-src`,
Clippy and `nasm` as needed. Use the kernel Makefile/custom target path;
ordinary host `cargo test` is not a kernel boot test. When removing warnings,
run the ratchet with `--update` and commit the lower baseline. The workflow
contains the exact preparation for an IBT-configured binary.

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
| `ferric-iommu-test` | QEMU edu device DMA, domain ownership, permissions, invalidation, fork isolation, grant/sub-grant access and revocation |
| `ferric-syscall-fuzz` | Seeded multi-process syscall fuzzing and validation of invalid inputs |

The edu driver is started through `pcid-spawner`, separately from the console
test script. Its PCI matching configuration is in the IOMMU test recipe.
Tests may skip absent optional features; do not interpret a skip as coverage.

`.github/workflows/image.yml` generates feature settings, forces KPTI on,
enables IOMMU `auto`, shadow-stack preparation `auto` and registered buffers,
and builds a second kernel/image with IOMMU `enforce`. Its five boot variants
are no IOMMU, VT-d auto/enforce, and AMD-Vi auto/enforce. It requires the
registered-buffer result markers, KASLR/KPTI logs, shadow-stack detection and
futex wake-race marker, and checks PCID use when the runner exposes it.
`check-iommu-log.sh` checks driver/domain and device-test evidence for each
IOMMU mode. Read that script for the exact required marker set.

The workflow exports recipe-facing kernel/relibc/bootloader settings from its
temporary generated config. It does not install a root `ferric.toml` or
automatically apply every generated user-space CPU flag to this mixed
prebuilt/source image. Dedicated local baseline builds are a separate check.

## Reproducing a boot

For a local smoke image with the desired features saved in `ferric.toml`,
keep the saved target/image name and override only its filesystem contents:

```sh
make config-check
make rebuild FILESYSTEM_CONFIG=config/ferric-smoke.toml
make qemu FILESYSTEM_CONFIG=config/ferric-smoke.toml
```

`ferric-smoke` is not currently an accepted `image.config` choice. Overriding
`CONFIG_NAME=ferric-smoke` while the saved choice is `desktop` makes the
generated target guard omit the saved image, CPU and kernel settings. Keep
`ARCH`, `BOARD` and `CONFIG_NAME` consistent with `ferric.toml` instead.

The filesystem override above uses the smoke config directly, bypassing the
generated package/general image overrides, while retaining the matching
target's CPU and kernel settings. `rebuild` recreates the selected image and
recipe-selection stamp after switching filesystem configs. The output
directory keeps the saved image
name: with the default `image.config = "desktop"`, the smoke image is
`build/x86_64/desktop/harddrive.img`. CI's explicit recipe-setting exports in
`.github/workflows/image.yml` let it use `CONFIG_NAME=ferric-smoke` separately.

For the scripted Linux CI-style boot, after creating an image, set `image` to
its actual path (the default local example above uses `desktop`; CI uses
`ferric-smoke`):

```sh
image=build/x86_64/desktop/harddrive.img
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
