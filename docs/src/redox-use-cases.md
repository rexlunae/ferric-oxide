# Use Cases

Ferric Oxide is a general-purpose operating system, but it has an order of
priorities. The roadmap's two release milestones say what "usable" means
first: **M1, server and VM usable**, then **M2, workstation usable**. Neither
milestone has been reached yet; see [`ROADMAP.md`](https://github.com/rexlunae/ferric-oxide/blob/main/ROADMAP.md)
for progress.

## Servers and virtual machines (M1)

The first target is running real services, as a VM guest and on servers, with
hardening and sandboxing on by default. That is where the project's I/O work
pays off most: NVMe and network paths built on shared-memory rings,
registered buffers and per-core queues, with every driver's DMA confined by an
IOMMU (Phase 2, partly implemented). Read-only ext4, LVM2 and LUKS support
(implemented) lets Ferric Oxide read disks provisioned the Linux way;
writable ext4 and XFS are planned.

Ferric Oxide also plans to host virtual machines (Phase V). A hypervisor core
in the kernel with a Rust VMM in user space would run full guests, and
**enclaves**: lightweight microVMs that boot in milliseconds and run software
written for another OS's security model, unmodified, with access to only the
files, sockets and devices the host grants them. Enclaves are meant as a
porting tool as much as a server feature. Only the hardware formats and the VM
interface exist so far; see [Virtualization](./ferric-virt.md).

## Workstations (M2)

The second target is a desktop or laptop for daily development work: an
accelerated COSMIC desktop, Wi-Fi, suspend and resume, a browser, and the
ability to build Ferric Oxide on Ferric Oxide. This depends on real-hardware
work (ACPI power management, USB, Wi-Fi) and graphics work (virtio-gpu, then
Mesa drivers on a DRM/KMS-compatible interface), which are Phases 5 and 6 of
the roadmap and mostly not started.

Today, Ferric Oxide inherits Redox's desktop: the Orbital display server,
several COSMIC applications and a set of SDL and X11 programs (see
[Important Programs](./important-programs.md)), running mainly in QEMU.

## Sandboxed and portable software

The long-term plan for user programs is WebAssembly (Phase W): one package that
runs on every CPU level and architecture, compiled for the exact CPU, with no
ambient authority. Combined with the capability model (Phase 4) and enclaves,
that would make Ferric Oxide a good host for software you don't fully trust.
This is the most experimental part of the plan and is gated on benchmarks.

## Infrastructure and appliances

Routers, edge servers and similar appliances benefit from the same properties
servers do: a small privileged core, restartable drivers, kernel-bypass network
queues and a verifiable software supply chain through `fox`. These are not a
separate target; they follow from the server work.

## Not a focus

- **Older and low-end hardware.** Ferric Oxide requires x86-64-v3 or ARMv8 and
  UEFI. See [Hardware Support](./hardware-support.md).
- **Small embedded and real-time systems.** Microcontrollers and hard real-time
  systems are outside the project's scope for now. Real-time scheduling
  classes are being explored as part of the scheduler rework, but there is no
  plan for safety-certified use.
