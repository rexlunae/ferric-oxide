# Security

This page describes the security model as it stands on `main`, and the parts
that are still planned. [Memory and CPU protection](./ferric-memory.md) has
the details of the hardware mitigations.

## Isolation by design

Drivers, file systems, the network stack and most other services run as
separate user-space processes. A bug in one of them is contained to that
process instead of running with kernel privileges, and the kernel itself
stays small enough to review.

The system is written in Rust. `unsafe` code is kept to small, commented
blocks behind safe interfaces (see [Rusting Properly](./rusting-properly.md)),
and CI fails if the number of `unsafe` uses in the kernel goes up
(`scripts/ferric-ci/kernel-lint-ratchet.sh`).

## Namespaces and file descriptors

Every resource is reached through a scheme, and every process has a
**namespace**: the set of schemes it can open by name. A process can be
started with a smaller namespace, or none, so it cannot open new resources
and can only use the file descriptors it was given. In bootstrap, the initial
namespace manager (`initnsmgr`) serves these namespaces.

A file descriptor works as a capability: holding it grants the operations it
was opened for, and it can be passed to another process. Ferric Oxide's new
interfaces rely on this. A registered-buffer region controls its grants, a
grant gives bounded and revocable access to memory, a DMA-domain descriptor
allows operations on one device's IOMMU domain, and `kernel.trace` hands out
`cap/read` and `cap/control` descriptors so a non-root collector can read
traces without becoming root.

## Users and root

Alongside namespaces, services check the caller's user and group IDs, in the
Unix way. Some kernel schemes still decide access with a plain "is this
root" check: IRQs, physical memory mappings, port I/O and opening the IOMMU
attach capability all require root today.

The `kernel.proc` `Iopl` request checks the **calling thread's effective UID**
when the request is made. Opening a thread handle as root and then dropping
the effective UID does not authorize a later port-I/O request. This protects
the existing all-ports grant; it does not revoke rights that were already
enabled before dropping privileges. Per-device port ranges and delegated
port-I/O capabilities remain Phase 4 work. The
[`ferric-ioport-test`](./ferric-testing.md#port-io-authorization) regression
checks these request-time rules without executing port-I/O instructions.

### Owner, group, mode and time changes

A scheme decides who may change one of its files' owner, group, mode or times.
`ramfs` and `ext4d` share one definition of the POSIX rules, with
`_POSIX_CHOWN_RESTRICTED`, in `ferric_abi::id`:

- only root or the file's owner may change its mode or group, and only root may
  give a file to another owner;
- the owner may set the group only to the file's current group or its own
  effective group;
- an unprivileged change of owner or group clears a regular file's set-user-ID
  and set-group-ID bits, and an unprivileged `chmod` cannot make a regular file
  set-group-ID for a group the caller is not in;
- `futimens` (`ferric_abi::id::times_access`) lets root or the owner set any
  times, lets anyone else set both to the current time (`UTIME_NOW` for both,
  or a null `times`) only with write permission (`EACCES` otherwise), and
  refuses everything else with `EPERM`; two `UTIME_OMIT`s change nothing and
  are not checked. `ferric_abi::time::TimeChange` reads the `UTIME_NOW` and
  `UTIME_OMIT` sentinels, so neither is stored as a time.

The kernel checks `fchown` first: a caller other than root may name only its own
effective IDs, or `-1` to keep one (`crates/kernel/src/scheme/chown.rs`). Schemes
learn only the caller's effective user and group, not its supplementary groups,
so a group check that a supplementary group would have satisfied is refused.

How a scheme learns the caller depends on the call. The kernel sends the
caller's effective IDs with `fchown` and the other direct scheme calls, but a
`StdFsCall` (how relibc sends `fchmod`, `ftruncate` and `futimens`) carries only
its pid, and its caller IDs read as root. A scheme that checks permissions on
one asks procmgr for the pid's credentials through its
`proc-creds-capability` handle (`scheme_utils::proc_credentials`), as `ramfs`
does for `fchmod` and `futimens`. The
[`ferric-owner-test`](./ferric-testing.md#ramfs-ownership-changes) regression
checks `ramfs` as root and as user 1000.

The kernel passes each `read` and `write` the descriptor's open flags but does
not check its access mode itself, so `ramfs` refuses a `read` through a
descriptor not opened for reading and a `write` through one not opened for
writing, with `EBADF` (#400). `ftruncate` arrives without the flags, so each
`ramfs` inode handle keeps the access mode it was opened with, and `ftruncate`
through one not opened for writing is refused with `EBADF` too (#375).

## Hardware mitigations

| Mitigation | Status | Option |
| --- | --- | --- |
| Kernel image and physical-map KASLR | Implemented, on by default | `kernel.kaslr_image`, `kernel.kaslr` |
| User ASLR, stack guards, faulting `PROT_NONE` | Implemented, on by default | `kernel.aslr`, `kernel.prot_none` |
| SMEP, SMAP, NX, UMIP | Used where the CPU has them | |
| KPTI with PCID | Implemented; on automatically for Intel CPUs affected by Meltdown | `kernel.cpu.kpti`, `kernel.cpu.pcid` |
| Supervisor CET (IBT and shadow stacks) | Implemented, off by default; shadow stacks need KPTI off | see [CET](./ferric-memory.md#cet-enforcement-versus-preparation) |
| User-mode CET | Not implemented | |
| IOMMU DMA isolation | Implemented, off by default | `kernel.iommu` |
| Bounds-check hardening against Spectre v1 at kernel trust boundaries | Implemented in the paths `crates/nospec/` covers | |
| Branch predictor barrier (IBPB) between processes against Spectre v2 user-to-user | Implemented; on automatically where the CPU reports IBPB | `kernel.cpu.ibpb`, see [barriers](./ferric-memory.md#branch-predictor-barriers-between-processes) |

## Planned: the capability model

Phase 4 of the [roadmap](./ferric-roadmap.md#phase-4-finish-the-capability-security-model)
finishes the move from root checks to capabilities:

- The remaining root checks in kernel schemes become explicit capabilities: a
  device manager hands out IRQ, physical-memory, IOMMU-domain and port I/O
  descriptors.
- Every system service starts sandboxed, with a manifest of the schemes it
  needs, through the namespace manager and `contain`.
- Desktop applications get per-application sandbox profiles.
- The boot chain uses UEFI Secure Boot with Ferric Oxide keys and TPM measured
  boot, and full-disk encryption can be unlocked by a key sealed to the TPM.
- A written threat model and an external review follow.

`fox` packages already carry signatures and SBOMs; capability manifests checked
at install time are planned with Phase 4 (see [the roadmap](./ferric-roadmap.md)).

## POSIX programs

relibc provides the POSIX and Linux interfaces in Rust on top of schemes, so
ported C programs get the same process isolation. Whether a program uses
safer interfaces is still up to the program.
