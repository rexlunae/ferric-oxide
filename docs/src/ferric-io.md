# Registered buffers, IOMMUs and rings

The implemented I/O extensions expose registered memory and DMA domains
through kernel schemes. Their ABI definitions live in shared crates so the
kernel, bootstrap, drivers and tests use the same paths and request encoding.

## Registered memory and grants

`kernel.regbuf = true` enables `/scheme/kernel.regbuf`; the default is `false`.
`crates/regbuf/src/abi.rs` defines region/grant paths and requests;
`crates/kernel/src/scheme/regbuf.rs` supplies the kernel implementation.

1. An application opens `region` beneath the scheme root and registers a
   page-aligned range of read-write anonymous memory. Private anonymous and
   `MAP_SHARED` anonymous memory are accepted. File-backed private mappings
   and memory merely borrowed from another process are refused.
2. Registration pins the frames and charges a pin budget to the region
   opener's user ID. Non-root users are limited to 16 MiB each; all users,
   including root, share a total limit of one quarter of memory. Exceeding the
   budget returns `EDQUOT`.
3. Opening a `grant/ro/<offset>/<length>` or `grant/rw/<offset>/<length>` path
   relative to the region creates a descriptor granting only that byte range.
   Offsets and lengths are hexadecimal; use `GrantSpec` to encode them.
4. The application passes the grant descriptor to a service. The holder maps
   it with `MAP_SHARED`; writable mappings require a read-write grant.
5. The owner can revoke a grant through the region. The holder can drop a
   mapping with `Request::Unmap`, or close its grant. Closing the region
   revokes its grants and releases frames only when every mapping is gone.

The mapped whole pages refer directly to the registered frames. A partial
first or last page uses a bounce page containing only the granted bytes, so
bytes outside the grant are not exposed. On teardown, writable bounce pages
copy back only bytes changed from their initial snapshot. That preserves
newer writes by the owner or another grant to untouched bytes; it does not
provide transactional conflict resolution for concurrent writes to the same
byte. The portable rule is in `crates/regbuf/src/bounce.rs`.

A grant holder cannot independently `munmap`, `mprotect` or `mremap` the
pinned mapping (`EBUSY`), or re-share/register it to escape revocation
(`EPERM`). A fork of the holder does not inherit that mapping. Forking the
region owner copies private frames for its child while the region retains the
owner's frames; `MAP_SHARED` frames retain their usual sharing semantics.

Explicit sub-grants let a holder delegate part of its range with equal or
less access. They are opened relative to the parent grant using the `sub`
path encoding. The table permits eight levels of ancestry. Revoking an
ancestor revokes its descendants even if an intermediate descriptor was
closed. Closing an individual grant descriptor does not itself revoke its
descendants. Per-request borrowed buffers can still be forwarded through
scheme requests without becoming permanent grants; the memory capture path
undoes partial forwarding mappings on failure.

## Strict revocation and failed teardown

The grant table in `crates/regbuf/src/table.rs` records both CPU and DMA
mappings. A teardown takes records under the region lock, performs hardware
unmapping after releasing it, then reports success or restores a failed
record. This avoids holding the region lock across other address-space or
domain operations and makes concurrent unmaps visible as work in flight.

A successful `Revoke` waits until all mappings of the selected grant and its
descendants are gone, including work started by another thread. CPU teardown
includes TLB shootdowns; DMA teardown includes completed IOTLB invalidation.
New mappings of revoked grants fail with `ECANCELED`.

If teardown fails, the request reports an error such as `EIO`; it does not
promise revocation completed. The mapping stays recorded for retry, and its
frames remain pinned. Closed regions with outstanding mappings are retained
on the kernel's retry list, keeping their pin charge until drained. A later
close/revoke retries them. Registration racing region closure refunds its
charge and cannot install frames into the closed region. The DMA backend has
additional failure and domain-close limitations described below; the region
table's retry bookkeeping alone does not resolve them.

## IOMMU policies and vendor support

`kernel.iommu` selects an x86-64 policy:

| Policy | Behavior |
| --- | --- |
| `never` (default) | Leaves IOMMU support inactive; no DMA-domain isolation from this feature |
| `auto` | Enables supported firmware-described units, logs unsupported ones, exposes domains; unbound processes retain physical translations and never-attached devices use the compatibility identity domain |
| `enforce` | Requires a bound domain with an attached device for DMA translation; unbound translations fail with `EACCES`, and unattached devices are blocked |

The kernel selects Intel VT-d from ACPI DMAR, or AMD-Vi from IVRS when using
that vendor path. The host-testable vendor crates parse firmware tables and
encode registers, page tables and invalidation commands. Kernel modules under
`arch/x86_64/iommu/` map hardware registers, allocate tables, drive queues and
report faults. `crates/iommu/` supplies common table walking, rings, IOVA
allocation, requester aliases and mapping lifecycle rules.

Firmware-reserved DMA ranges are retained: VT-d RMRRs and applicable AMD-Vi
IVMD ranges receive identity mappings. A domain is therefore constrained by
the platform's required reserved mappings as well as its driver's mappings.
PCI bridges and aliases are resolved so a device's possible requester IDs are
attached together. AMD-Vi tracks shared aliases per domain to prevent takeover
or premature blocking while another function still uses them.

Intel units with the required interrupt-remapping and queued-invalidation
support get interrupt-remapping tables. `pcid` requests MSI/MSI-X entries with
requester validation, and the kernel routes the I/O APIC through remapping.
On those units compatibility-format interrupts are blocked. AMD-Vi currently
implements DMA remapping only and uses compatibility-format interrupts.
Neither the `enforce` policy nor DMA tests establish AMD interrupt isolation.

## Per-CPU interrupt delivery

The `irq:` scheme lets a driver reserve vector `N + 32` on one CPU by
opening `irq:cpu-XX/N` with `O_CREAT`. Upstream counts interrupts and wakes
handles by vector number only, so the same vector held on two CPUs shows up
on both handles. With `kernel.irq_percpu` (off by default, x86_64 only):

- Interrupts are keyed by (logical CPU, vector). Each CPU increments its own
  row of atomic counters (`ferric_irq::Counts`), without a global lock, and
  only handles opened for that CPU and vector are woken. Interrupts from the
  PIC or I/O APIC belong to the BSP's `irq:N` handles. On an AP, IRQ numbers
  0 to 15 are ordinary vectors and can be reserved there.
- The top-level listing names CPUs by logical id, the number `cpu-XX` takes.
- The root-only file `irq:cpus` lists each online CPU as
  `cpu=<logical hex> apic=<APIC id> node=<NUMA node or ->`. `ferric-irq`
  holds the writer and parser, so user space reads it with
  `ferric_irq::parse_cpus`. The file appears in directory listings of
  `/scheme/irq`.
- Without interrupt remapping, pcid looks up the APIC id of the CPU a driver
  asks for in `irq:cpus` when it builds a compatibility-format MSI message,
  instead of using the logical id as one. It refuses a CPU whose APIC id the
  format cannot name (0xFF or above). Without `irq:cpus`, pcid uses the
  logical id, as upstream did.

In every build, opening `cpu-XX` for a CPU that does not exist returns
`ENOENT` instead of panicking the kernel. `ferric-iommu-test` checks
delivery with `iommu-msi-percpu`.

Drivers give MSI-X table entries to CPUs through `pcid_interface`:

- `Msix::alloc_on(cpu)` programs the next entry with a vector on that logical
  CPU.
- `Msix::alloc_spread(count, node)` programs up to `count` entries, one per CPU
  before any CPU gets a second, with CPUs on the device's NUMA node first
  (`ferric_irq::spread`).
- `InterruptVector::cpu` says where each vector went.

Without `irq:cpus` these calls put every vector on the BSP, like upstream's
`Msix::alloc`. Errors are typed (`pcid_interface::percpu::VectorError`).
`percpu::online_cpus` and `percpu::bsp_cpu` read the CPU map and the BSP's
logical id. No driver uses these calls yet; IRQ steering and per-core NVMe
queues are later steps of roadmap Phase 2.4.

## From PCI discovery to a driver's DMA address

1. Bootstrap exposes `/scheme/kernel.iommu`. `pcid` opens its root as root
   to obtain the attach capability and reports bridge information.
2. `pcid` creates an empty `domain`, attaches the function and its requester
   aliases, and passes the domain descriptor through its driver interface.
3. The driver interface binds the first domain to the driver's address space
   before DMA allocation. Additional functions join their devices into that
   domain. A process binds at most one domain, and a domain is bound by at
   most one live address space; a forked child starts unbound.
4. `memory:translation` now returns an IOVA in that domain. The first
   translation pins and maps the frames; repeated translations reuse the
   mapping. Contiguous allocations receive contiguous IOVA ranges.
5. Removing write access with `mprotect` narrows device permissions too.
   Unmapping memory, process exit and domain closure retire translations
   with the required invalidations before frames can be released.

The entry points are `userspace/base/drivers/pcid/src/iommu.rs`,
`src/driver_interface/mod.rs`, `crates/kernel/src/scheme/iommu.rs`, and
`crates/kernel/src/arch/x86_64/iommu/dma.rs`.

`enforce` intentionally rejects callers without a device domain. In-memory
drivers such as `lived`, `nbdd` and `usbscsid` that use DMA allocation interfaces
without their own PCI device need an adapted allocation path. Do not assume
every existing image configuration works unchanged under enforcement.

## Mapping grants for device access

With registered buffers enabled, a driver can issue `MapGrant` on the domain
it has bound, naming a grant descriptor it holds. The request maps the
registered frames at consecutive IOVAs with the grant's access and returns
the starting IOVA. DMA requires a whole-page-aligned grant; byte grants using
partial-page bounce views cannot expose those pages directly to a device.

`MapGrant` and `UnmapGrant` both require the caller's address space to be bound
to the supplied domain. Possessing its descriptor does not grant permission
to remove another address space's mappings. A caller that has not bound this
domain, including one bound to a different domain, receives `ENODEV`. A mapping
ticket distinguishes successive uses of an IOVA so a delayed revocation
cannot remove a newer mapping. The normal grant-revocation path removes the
DMA mapping and waits for invalidation. Revocation does not free the
mapping's IOVA range: the kernel *retires* it, and it stays reserved until the
driver's `UnmapGrant` of that IOVA (which then succeeds) or the domain's close.
A driver that still has a command queued for the revoked grant can only make
the device fault, never reach a buffer mapped later at a reused IOVA. `UnmapGrant`
answers `ENOENT` for an IOVA that is neither mapped nor reserved. The
interface never hands out physical addresses of another process's buffer as
a fallback. Drivers needing a fallback must manage their own bounce buffers.

Domain closure blocks its devices before removing the handle and records
whether that blocking succeeded. On success, it retires the domain's grant
links and completes their matching region records, including restored records
on closed grants. Closed regions release their frames and pin charges when
the last mapping completes. Region records are reconciled outside the handle
lock to preserve the region-before-handle lock order.

When a failed unmap is still in flight during domain closure, the link may
reach its region before the revoker publishes that failure. Before restoring
the record, teardown rechecks confirmed domain closure under the region lock.
It completes an already blocked mapping instead of stranding a restored record
after its link was removed. Failed device blocking still requires retention.

Failed DMA-grant unmapping retains its mapping record, grant link, frame
references and allocated IOVA. Once hardware unmapping fails, later attempts
return `EIO` rather than treating cleared page-table entries as proof of
completed invalidation. Resources are retired only after confirmed unmapping
or a safely blocked domain. If device blocking fails during domain close,
its failure marker remains after handle removal: later revocation returns
`EIO`, and the grant records and pinned frames remain retained. Concurrent
close and revocation retire a mapping only once. See `scheme/iommu.rs` and
`arch/x86_64/iommu/dma.rs` in the kernel.

Failed grant installation is also part of the mapping lifecycle. A confirmed
rollback releases its IOVA and pins. If installation or rollback cannot
confirm invalidation, the kernel retains the reservation, frame pins and a
failed mapping record in both the domain and grant before returning `EIO`.
Revoking or closing that grant keeps reporting failure and retains its region
records until the domain's devices are successfully blocked. This applies to
failures on the first physical run and after earlier runs succeeded; cleared
page-table entries alone do not prove cached DMA translations are gone.

Host fault-injection tests in `crates/iommu/tests/grant_failures.rs` cover
those installation and rollback failures, grant and region closure, and IOVA
retention and release. Hardware boot tests exercise the normal mapping path;
they do not inject invalidation failures into a real IOMMU.

## Storage DMA into registered buffers

`drivers.grant_dma = true` (Drivers menu, default `false`) lets an application
have `nvmed` move disk blocks straight between the NVMe device and its
registered buffer: no bounce buffer, no copy and no per-request mapping. It
depends on `kernel.regbuf` and on `kernel.iommu` other than `never`, and sets
`FERRIC_INITFS_FEATURES=nvmed/grant-dma`, which `userspace/base/Makefile`
passes to the initfs build. Without the option, `nvmed` behaves as upstream.
The call ABI is defined once in `crates/blkgrant/src/abi.rs`; the driver side is
`userspace/base/drivers/storage/driver-block/src/grant.rs` (feature
`grant-dma`) and `nvmed`'s `PcidGrantDma`.

On an `nvmed` disk or partition descriptor:

1. **Attach.** The application passes a whole, page-aligned grant descriptor
   with `SYS_CALL` and `CallFlags::FD`, one metadata word naming a slot
   (0 to 63). The driver checks the grant is not revoked, maps it once with
   `MapGrant` in its bound domain, and keeps the descriptor. Attaching to an
   occupied slot replaces its grant. Errors: `EINVAL` for a bad slot or a
   descriptor that is not a grant, `ECANCELED` for a revoked grant, `ENODEV`
   without an IOMMU domain, and `EOPNOTSUPP` from drivers that do not map grants.
2. **Read and Write.** A `SYS_CALL` with `Request::Read` or `Request::Write`
   moves `len` bytes between block `lba` of the descriptor (relative to the
   partition) and the slot's grant at `offset`, and returns `len`. Errors:
   `ENOENT` for an empty slot, `EINVAL` for a length that is not whole blocks
   or a range outside the grant, `EACCES` for a read into a read-only grant,
   `ECANCELED` once the grant is revoked, and `EOVERFLOW` past the partition.
3. **Detach.** `Request::Detach` empties a slot. Closing the descriptor
   detaches every slot.

The kernel passes a scheme exactly three metadata words for a call on one
descriptor, and fills any the caller did not send with zeros. A request
therefore packs into three words. Word 0 holds the verb (bits 0 to 7), the slot
(bits 8 to 15) and the offset in the grant in whole 4 KiB pages (bits 16 to
63); words 1 and 2 are the LBA and the length. Offsets in a grant are
therefore page-granular, and `Request::encode` refuses an unaligned offset or
a zero length; `decode` reads missing words as zero.

Before each transfer the driver asks the kernel (`Info`) whether the grant was
revoked and refuses it with `ECANCELED`, so the device is normally never
pointed at an unmapped IOVA. A revocation that races a running command makes
the device fault at the IOMMU instead, because the retired IOVA stays reserved
until the driver unmaps it. The driver unmaps only once nothing uses the IOVA:
each transfer holds a lease on its grant until the device completes it, and a
detached, replaced or closed grant is unmapped when its last lease is dropped
(`crates/blkgrant/src/slots.rs`). With the option on, `nvmed` also builds PRP
lists for zero-copy commands, so a command carries up to the controller's
MDTS (one list page; 512 KiB on QEMU) instead of two pages; larger transfers
are split into several commands.

This is a direct-call path on `nvmed` descriptors only. RedoxFS does not
forward grants yet, the ring interface (`redox-rings` SQEs) has no grant slots,
and `ahcid` and `virtio-blkd` do not map grants.

`ferric-blkgrant-test` covers attach, reads and writes compared with `pread`
(writes go only to the BIOS boot partition and are restored), the refusals
above, revocation (`ECANCELED`, buffer untouched), detach releasing the retired
IOVA, and a copy versus grant read benchmark. Without an IOMMU its grant steps
skip, and it still prints the copy benchmark.
`scripts/ferric-ci/check-blkgrant-log.sh` checks those markers and the kernel's
map, unmap and IOVA-release lines in each IOMMU boot variant. Host tests in
`crates/blkgrant` cover the encoding, the slot checks, PRP layout and the lease
rule, including a revocation and detach while a transfer is in flight.

These are available primitives and tested integration paths. Apart from the
`nvmed` path above, they do not make the filesystem, network stack or other
drivers use an end-to-end registered-buffer protocol. The dedicated image tests exercise CPU
grants, forwarding, sub-grants, device DMA and revocation; see
[Validation and debugging](ferric-testing.md).

## Submission/completion rings

`kernel.rings = true` (Kernel menu, default `false`) enables `/scheme/kernel.ring`.
`crates/ring` (ferric-ring) is the one definition of the ring format: the
memory layout, the 64-byte SQE and 32-byte CQE, opcodes (the kernel's scheme
request numbers, so `Read` is 5 in both), flags, notify modes, chain rules and
the scheme's calls. It is `no_std` with no `unsafe` code, and the kernel,
bootstrap and clients all use it. The kernel side is
`crates/kernel/src/scheme/ring.rs`.

1. Open `ring` beneath the scheme root. `Request::Setup` allocates the ring:
   one header page, then the SQ and CQ slots, each on pages of its own. Entry
   counts are powers of two, at most 4096 SQ and 8192 CQ entries, with a CQ at
   least as large as the SQ (twice the SQ by default). Map the whole
   `Layout::map_len()` with `MAP_SHARED`.
2. Write SQEs with a `Producer<Sqe>` and publish them, then call
   `Request::Enter`. The kernel takes complete chains from the SQ and runs
   each SQE in the caller's context as the system call it stands for, on the
   caller's descriptor and buffer: `Read`/`Write` at an offset as
   `SYS_READ2`/`SYS_WRITE2`, at `CURRENT_POSITION` as `read`/`write`, and
   `Fsize`, `Fstat` and `Fsync` as the system calls of those names. A user
   scheme receives an ordinary scheme request, so every scheme works with
   rings unchanged.
3. Read one CQE per SQE with a `Consumer<Cqe>`: the SQE's `user_data` and the
   result, or minus the errno. `fevent` on the ring descriptor reports
   `EVENT_READ` while completions are waiting, and `Enter` triggers it.

Linked SQEs (`SqeFlags::LINK`) form a chain that runs in order, possibly
across schemes. A failure, or a `Read`/`Write` that moves fewer bytes than
asked, cancels the rest of the chain with `ECANCELED`. A chain is taken only
once its last SQE is published and only when the CQ has room for all of its
completions, so completions are never dropped: `Enter` returns `EBUSY` when
not even the first chain fits. Chains of more than 32 SQEs, or ones that fill
the SQ without ending, complete with `EINVAL`.

`Enter` looks up every descriptor a chain names when it takes the chain, before
any SQE runs, and holds those open file descriptions until the chain is done.
If another thread closes a descriptor, or replaces it with `dup2`, while the
chain waits, the chain still acts on the files it named when it was taken, as
one system call would. Whichever holder lets go of a description last (the
descriptor table, the ring, or a file system call still running on it, through
`syscall::fs::release_description`) closes it, so a description closed while a
ring or a call holds it is still closed exactly once.

The ring memory is shared with the application, which may write any of it at
any time. The kernel reads it atomically through its own mapping, keeps its
own indices, copies each SQE once before checking it, and fails `Enter` with
`EINVAL` when the application's SQ tail is out of range. One `Enter` runs on
a ring at a time; a concurrent one gets `EBUSY`.

Each ring has a file table of 1024 slots. `Request::RegisterFiles { first }`
is a `SYS_CALL` with `CallFlags::FD` whose payload holds descriptors (add
`FD_CLONE` to keep them open in the caller); they go into consecutive slots
from `first`, all or none. An SQE with `SqeFlags::FIXED_FILE` names a slot
instead of a descriptor, which skips the descriptor lookup and keeps working
after the application closes its descriptor. A slot keeps its file until
`Request::UnregisterFiles { first, count }` or until the ring is closed. A
full slot is refused with `EBUSY`; slots past the table and ring descriptors
(a ring may not hold a ring open) with `EINVAL`; the descriptors of a
refused call are closed. An empty slot completes with `EBADF`.

A chain whose first SQE has `SqeFlags::ASYNC` runs on the ring's worker, a
kernel context the ring starts on first use. `Enter` takes the chain, sets
aside room in the CQ for its completions, looks up the files it names
(refusing rings with `EINVAL`, since a chain holding its ring would keep the
ring from closing), and returns without waiting; the worker runs asynchronous chains in order, with
the submitter's address space and credentials, and posts each chain's CQEs
together, then triggers `EVENT_READ`. `Request::Enter { to_submit,
min_complete }` waits after submitting until the CQ holds `min_complete`
completions or nothing submitted is still running, on the worker or in
another thread's `Enter` (`EINTR` on a signal, `EINVAL` above the CQ's size);
`Enter` announces each chain it runs itself as soon as its CQEs are posted.
Closing the ring cancels the worker for good, as a signal that never clears:
the wait the running chain is in ends, as does any it starts later, and the
queued chains complete with `ECANCELED`.

Registered-buffer grants as SQE buffers, polled (`SqPoll`) and `UMWAIT`
notification, and rings served natively by user schemes come later (see the design in
`io/rings-design.md` in the project files). Their flags and modes already
have encodings and return `EOPNOTSUPP` today. `ferric-ring-test` covers the
calls, chains across RedoxFS, the pipe scheme and the null scheme, the file
table, `Fstat`, asynchronous chains and waits, closing a ring with a chain
blocked, the refusals, back-pressure and a corrupt tail, and prints ring
versus system call timings, by descriptor, by slot and on the worker.
