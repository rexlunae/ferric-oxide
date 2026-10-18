# Memory and CPU protection

These mechanisms are integrated into the x86-64 kernel, loader and runtime.
Defaults here refer to a build configured through `ferric.toml`, not a direct
kernel Cargo build or a top-level build without that file.

## CPU state and feature policy

`kernel.cpu.*` options generally choose `always`, `auto` or `never`. The
kernel build script reads generated CPU policy and compiles or patches the
appropriate paths. Automatic CPU alternatives require
`kernel.self_modifying`; without it, automatic alternatives become disabled.
An `always` policy requires the relevant hardware rather than silently falling
back.

`crates/kernel/src/arch/x86_64/alternative.rs` enables the selected extended
state components in XCR0 and sizes XSAVE areas from CPUID. It supports AVX,
and, when selected and supported, AVX-512 opmask and ZMM components.
`src/context/arch/x86_64.rs` saves and restores that state across context
switches using the selected XSAVE/XSAVEOPT paths. v3 requires XSAVE; v4 also
requires AVX-512 state. This support is implemented, rather than a future
requirement for using v4.

SMAP and FSGSBASE retain the same policy model. SMAP limits supervisor access
to user mappings; FSGSBASE provides the selected fast FS/GS-base operations.
CPU baseline validation is in `arch/x86_64/cpu_level.rs`.

## Address randomization and guards

| Option | Integration | Limits |
| --- | --- | --- |
| `kernel.kaslr` (default `true`) | `arch/x86_64/kaslr.rs` chooses a randomized, 1 GiB-aligned base for the linear physical-memory map | The initial loader/boot-stack mapping remains reserved |
| `kernel.kaslr_image` (default `true`) | UEFI bootloader and `crates/elfreloc/` replay retained relocations and slide the kernel within its reserved window | Needs retained relocations and entropy; fallback is the link address |
| `kernel.aslr` (default `true`) | Kernel address spaces randomize the search start for non-fixed mappings; relibc `redox-rt` randomizes the main stack and reserves its guard | Fixed mappings, including non-PIE executable mappings, are not moved |
| `kernel.prot_none` (default `true`) | Kernel page-table permissions make `PROT_NONE` inaccessible from user mode | Kernel-mediated access on behalf of syscalls is a separate permission path |

`crates/hwrand/` supplies layout randomness from x86-64 RDRAND. It returns
`None` if unavailable, allowing the caller to keep the fixed layout. This is
layout entropy, not a general cryptographic key generator. Randomization
therefore depends on available hardware and the selected build.

The ELF relocation helper validates every relevant relocation before applying
any. It supports the absolute and PC-relative relocation forms needed by the
linked kernel and ignores relocations for unloaded debug sections. Unsupported
or overflowing relocations return typed errors. Kernel stack traces translate
back to link addresses so they can be compared with `kernel.sym`.

The runtime maps guard pages below allocated thread stacks, and its exec path
also reserves a main-stack guard. With `kernel.prot_none`, user access to a
guard faults instead of reading ordinary mapped memory. The guard and ASLR
image tests exercise these behaviors.

## KPTI and PCID

`kernel.cpu.kpti` defaults to `auto`. It enables kernel page-table isolation
on Intel CPUs that do not report `RDCL_NO`; the automatic policy leaves it
off on AMD and on Intel CPUs reporting that protection. `always` forces the
path for validation.

Each address space has a user shadow page table mapping very little kernel
memory. System-call and interrupt entry switch to the kernel table; return
switches back. The minimal entry mappings and per-CPU entry stacks make those
transitions possible. The implementation is in `arch/x86_64/kpti.rs`,
`arch/x86_64/interrupt/` and the x86-64 `rmm` page-table code.

`kernel.cpu.pcid` defaults to `auto` and applies only while KPTI is active.
With PCID and INVPCID available, a fixed kernel/user PCID pair preserves TLB
entries across entry and return. Page invalidations and shootdowns invalidate
the user PCID as well; switches between address spaces still flush because
they share the pair. This is not a cache of a separate PCID pair per process.

The old catalog option `kernel.pti` is separate from `kernel.cpu.kpti`. It
describes an experimental inherited path and currently names a Cargo feature
absent from the kernel manifest. Use `kernel.cpu.kpti` for the implemented
Ferric isolation path.

## CET: enforcement versus preparation

`kernel.cpu.ibt` defaults to `never`. Selecting `auto` or `always` makes the
kernel Makefile compile the kernel and its build-std libraries with branch
protection and disabled jump tables. Indirect targets and assembly entry points
have `endbr64`; the kernel enables supervisor IBT on supporting CPUs. A bad
target raises the control-protection exception, decoded with `crates/cet/`.
User-mode CET remains off. See `arch/x86_64/ibt.rs` and
`scripts/ferric-ci/check-kernel-ibt.sh`.

`kernel.cpu.shstk` accepts `never` (default) and `auto`. The automatic path
detects support, allocates shadow stacks for kernel stacks, sets up tokens,
and maintains SSP-related state. It does **not** enable shadow-stack checking;
the shadow-stack context-switch instruction sequence remains inactive.
Enabling supervisor shadow stacks across syscalls, non-local returns and KPTI
still needs implementation. Writable physical-map aliases of their frames are
another limitation of the prepared design. See `arch/x86_64/shstk.rs` and
`crates/cet/src/shstk.rs`.

CI checks the IBT binary's entry points and boots shadow-stack detection and
preparation. Those checks do not demonstrate CET enforcement on hardware.

## Speculation hardening and lifetime rules

`crates/nospec/` provides data-dependent bounds-index clamping and checked
slice access for attacker-controlled indices. Kernel consumers use these
helpers at selected lookup boundaries. Their presence is not a claim that
all speculative-execution attacks are mitigated.

Memory protection also depends on frame ownership and invalidation ordering.
Pinned grants cannot be re-shared as ordinary mappings, private copy-on-write
pages must be allocator-owned, and frames cannot be reused while a CPU or DMA
mapping may still reach them. [Registered buffers and IOMMUs](ferric-io.md)
describes the strict teardown and retry behavior that enforces those lifetimes.
