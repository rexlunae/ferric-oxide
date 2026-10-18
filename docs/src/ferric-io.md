# Registered buffers and IOMMUs

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

`MapGrant` checks that the caller has bound the domain. `UnmapGrant` currently
operates on the supplied domain descriptor without repeating that binding
check, so a domain descriptor must not be treated as a mapping-only delegated
capability. A mapping ticket distinguishes successive uses of an IOVA so a
delayed revocation cannot remove a newer mapping. The normal grant-revocation
path removes the DMA mapping and waits for invalidation. Without a bound
domain, grant mapping fails with `ENODEV`; the
interface never hands out physical addresses of another process's buffer as
a fallback. Drivers needing a fallback must manage their own bounce buffers.

Domain closure blocks its devices before removing the handle and records
whether that blocking succeeded. On success, it retires the domain's grant
links and completes their matching region records, including restored records
on closed grants. Closed regions release their frames and pin charges when
the last mapping completes. Region records are reconciled outside the handle
lock to preserve the region-before-handle lock order.

Failed DMA-grant unmapping retains its mapping record, grant link, frame
references and allocated IOVA. Once hardware unmapping fails, later attempts
return `EIO` rather than treating cleared page-table entries as proof of
completed invalidation. Resources are retired only after confirmed unmapping
or a safely blocked domain. If device blocking fails during domain close,
its failure marker remains after handle removal: later revocation returns
`EIO`, and the grant records and pinned frames remain retained. Concurrent
close and revocation retire a mapping only once. See `scheme/iommu.rs` and
`arch/x86_64/iommu/dma.rs` in the kernel.

These are available primitives and tested integration paths. They do not
automatically make the filesystem, network stack or all drivers use an
end-to-end registered-buffer protocol. The dedicated image tests exercise CPU
grants, forwarding, sub-grants, device DMA and revocation; see
[Validation and debugging](ferric-testing.md).
