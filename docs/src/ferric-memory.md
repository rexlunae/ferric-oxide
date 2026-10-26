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

Saved CPU-state buffers use `ferric-aligned-buffer` (`crates/aligned-buffer/`).
It retains each allocation's exact layout for deallocation and uses the same
implementation in host tests and the kernel. Empty buffers require no allocation;
invalid alignments and oversized layouts return typed errors. Kernel context
creation maps allocation failures to `ENOMEM`. The buffer holds initialized bytes
rather than arbitrary zeroed Rust types, so it needs no unsafe zero-validity trait.

The boot-time `rmm::BumpAllocator` requires an unsafe constructor: its caller
must transfer exclusive ownership of disjoint, aligned, writable mapped areas.
It rejects zero or overflowing frame requests
before advancing its cursor. An initial offset beyond every memory area produces
an exhausted allocator with zero remaining capacity. Its host tests cover these
boundaries and zeroing/disjointness of allocations in the software memory model.
These checks do not validate real page-table or TLB operations.

An empty kernel wait condition can be destroyed without acquiring a lock or
creating another clean lock token. This covers drained page-cache objects and
queues closed during RedoxFS startup, including with `kernel.drop_panic` enabled.
The diagnostic still catches implicit destruction of a nonempty wait list;
callers that hold a token use explicit notification/destruction paths. Host
tests compile the actual ordered-lock and wait-condition modules and verify
empty destruction while another lock is held. They do not emulate scheduling.

### ARM64 signal arguments

In relibc, `redox-rt/src/arch/aarch64.rs` saves an interrupted program's
integer registers in the order defined by `ArchIntRegs` and restores them
after the signal handler. In particular, the saved x2 slot precedes x1.
Reversing that pair swaps arguments when execution resumes: a signal arriving
during the dynamic loader's `memcpy` can replace its byte count with its source
address and make an otherwise ordinary `ls` launch fault. The signal frame
now saves that pair in the same order as its restore sequence.

ARM64 also defers signal delivery while a system call is executing. The SVC
exit first publishes the result in x0, then selects a pending handler, so its
saved x0 is the result rather than the call's original argument. In particular,
an interrupted startup `yield` must return zero; restoring its sentinel `-1`
argument instead made relibc's host check abort a valid executable.

The `ferric-signal-test` recipe exercises asynchronous delivery with a source
pointer in x1 and a length in x2, then checks repeated dynamic command
launches; see [validation](ferric-testing.md#arm64-signal-return-regression).
It also verifies SIMD registers and FPCR/FPSR across a handler that deliberately
clobbers them. The trampoline preserves SIMD state privately below the public
integer frame; the opaque ARM64 `ucontext_t` layout remains unchanged.

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

## Mapping ranges stay inside user space

User space ends at `USER_END_OFFSET` (2^47, `0x0000_8000_0000_0000`, on
x86-64 with four-level paging). The grant table of an address space keeps
its grants and free holes only below that limit, so a span that leaves the
range is not accounted for, and one whose end wraps past the top can make a
later selection hand out pages that a grant already owns. The address space
then panics on the overlap assertion in `UserGrants::insert`; the syscall
fuzzer found this through `fmap` and `mremap` with addresses in the kernel
half.

Every entry point rejects such a span before it reaches the grant table:

| Entry point | Validation |
| --- | --- |
| Anonymous `fmap` (memory scheme) and `proc` mappings | `PageSpan::validate_nonempty` on the requested range |
| `fmap` of a scheme file descriptor (`fmap_inner`) | the page count must fit in `USER_END_OFFSET`, and a `MAP_FIXED` address plus that size must end below it |
| `funmap` | the aligned length must be representable and the range must validate |
| `mremap` | both the old and the new range must validate |
| `AddrSpace::r#move` (proc transfer, `mremap`) | the destination size and, for a fixed base, base plus size must stay below `USER_END_OFFSET`, because it inserts a grant without calling `mmap` |
| `AddrSpace::mmap` | last check for every caller: the span size and, for a fixed base, base plus size are compared with `USER_END_OFFSET` before any hole or grant is touched |

The `AddrSpace::mmap` check runs inside the region that emits `mm.map`, so a
rejected span is recorded in kernel traces like any other failed mapping. The
hole bookkeeping also saturates and checks its arithmetic, so an unvalidated
span cannot wrap a hole end into a smaller address and pass the fit test in
`find_free`, and a hole can never replace an existing one in only one of the
two indexes. The `ferric-syscall-fuzz` regression calls repeat the requests
that found these bugs on every run: `mremap` and an anonymous `fmap` with
`MAP_FIXED` at the top of the kernel half, a file-backed `fmap` with
`MAP_FIXED` whose end leaves user space, and a `funmap` length with no aligned
value.

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

Changes to kernel mappings that every address space shares, patched kernel
text and tracepoint static keys, go through `rmm`'s `invalidate_kernel_page`,
which drops the page from every PCID that may cache it, not only the loaded
one. `invalidate_kernel_all` flushes every PCID, global entries included, with
`INVPCID` type 2 when PCIDs are on and a CR4.PGE toggle otherwise. `rmm`
reports the loaded table without CR3's PCID bits, and takes the user PCID it
invalidates from the live CR3. With the fixed pair these behave exactly as
before; they are groundwork for per-address-space PCIDs.

A CPU that waits for a contended lock keeps answering TLB shootdowns. Every
ordered lock path, including the context switch's `write_arc` on a context
lock and the upgrade of an upgradeable read guard, retries the lock and
acknowledges any pending shootdown between tries (`sync/spin_wait.rs`). The context switch runs with interrupts off, so without
this a CPU waiting for a context lock cannot acknowledge a flush. If the
flushing CPU holds a lock that the context lock's holder waits for, none of
them can continue; that was the boot-smoke hang in
[#198](https://github.com/rexlunae/ferric-oxide/issues/198). The host tests
in `crates/kernel/tests` model the wait with threads and an acknowledgement
flag; only the image boots exercise the real context-switch path.

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

`kernel.cpu.shstk` accepts `never` (default), `auto`, and `always`. With
`kernel.self_modifying` enabled, it maintains kernel shadow stacks and enables
supervisor return checking on supporting CPUs when KPTI is off. `always`
requires CET support and `kernel.cpu.kpti=never`; `auto` leaves checking off
when CET is missing or KPTI is on. User-mode CET and the KPTI entry shadow-stack
switch remain unsupported. IBT is an independent choice.

Compiled shadow-stack support reserves 32 MiB at boot for 4096 simultaneous
8 KiB shadow stacks, including boot and per-CPU stacks. The pool is excluded
from both the ordinary frame allocator and the writable physical-memory map;
exhaustion returns an allocation failure, with no writable fallback. The kernel
checks every reserved page after constructing its boot mappings and invalidates
old bootloader translations before AP startup. Pool frames are mapped only with
read-only shadow-stack PTEs and an unmapped guard below each stack.

Live CPUs write entries with `WRSS`. Pre-enable initialization makes a bounded
store through the same read-only mapping while interrupts are masked and the
CPU's CET/WP state is temporarily disabled. It restores CR0, CR4 and RFLAGS
before returning, with no calls while protection is off and no writable PTE
that another CPU could cache. The sequence follows Intel SDM Volume 3A sections
2.5 and 4.6. Once a CPU has a live shadow stack it always uses `WRSS`.

Host tests cover slot bounds, reservation exclusion, pool exhaustion and reuse.
The boot self-test also checks PTEs, attempts ordinary writes to the shadow
mapping and both physical-map addresses, and requires page faults without
changing the entries. Its narrowly scoped fault recovery restarts the same
`rep stosq` with zero count, preserving the return RIP and CET shadow frame.
It checks clearing and reuse after the secondary CPUs start but before CET is
enabled, and repeats with real `WRSS` after the BSP enables CET. Secondary CPUs
service TLB shootdowns while waiting for BSP context initialization, because
interrupts are still masked and mapping retirement can otherwise deadlock the
BSP's second probe. See `arch/x86_64/shstk.rs` and `crates/cet/src/shstk/`.
If no complete reserved key fits the supported window, it retains the boot
mapping checks and explicitly skips the window-dependent probe. This preserves
the CET-disabled `auto` boot when the pool is above the 1 TiB window limit.

CI checks IBT and shadow-stack binary sequences and runs the pool isolation
self-test even on CPUs without CET. Those boots do not validate CET enforcement.
An older Kleya image enabled CET and passed its initial self-test, then panicked
during syscall fuzzing; [#78](https://github.com/rexlunae/ferric-oxide/issues/78)
tracks the required fresh hardware rerun.

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
