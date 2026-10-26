# Drivers

Device drivers in Ferric Oxide are user-space daemons, as in Redox. Each runs
as an ordinary process with its own address space and a namespace that limits
which schemes it can reach. A crashed driver does not take the kernel down,
and a buggy driver cannot write over another service's memory the way a
driver in a monolithic kernel can.

The drivers are in `userspace/base/drivers/`, and its `README.md` lists them.

## How a driver starts

1. `pcid` enumerates PCI devices and serves the driver interface: configuration
   space, BARs, interrupt (MSI and MSI-X) allocation and, with an IOMMU, the
   device's DMA domain.
2. `pcid-spawner` matches each device against driver configuration files and
   starts the right driver, handing it a connection to `pcid`.
3. The driver maps its device's registers, sets up interrupts, and serves a
   scheme: `disk.*` for storage, `network.*` for network cards, a display
   scheme for graphics and so on.

Devices not found through PCI (ACPI devices, the PS/2 controller, boards with
device trees) are started by `acpid`, `hwd` and the init configuration.

## DMA isolation

A device that can do DMA can read and write any physical memory unless an
IOMMU stops it. With `kernel.iommu` set to a policy other than `never` (the
default), the kernel programs Intel VT-d or AMD-Vi, puts each device that a
driver is attached to in its own DMA domain, and lets the driver map only
memory it owns into that domain. The driver then gives its device I/O virtual
addresses instead of physical ones, and the mappings are revoked when the
driver exits. Under `auto`, devices with no driver attached keep the
compatibility identity mapping, so their DMA is not restricted; `enforce`
blocks them. The
[I/O chapter](./ferric-io.md#from-pci-discovery-to-a-drivers-dma-address)
follows this path in detail.

With registered buffers, a driver can also DMA straight into memory a client
granted it. `nvmed` does this for disk reads and writes under
`drivers.grant_dma`.

## Ferric Oxide driver work

| Driver or feature | Status | Option |
| --- | --- | --- |
| `nvmed` grant DMA into registered buffers | Implemented, off by default | `drivers.grant_dma` |
| `nvmed` per-CPU queue pairs with steered interrupts | Implemented, off by default | `drivers.nvme_percore` |
| Kernel-bypass network queues (copy mode) in `driver-network` | Implemented, off by default | `drivers.net_bypass` |
| `rtl8125d`, Realtek RTL8125B 2.5 GbE | Builds and passes host tests; not yet booted on hardware | `drivers.rtl8125` |
| Zero-copy network queues | Planned | |
| Kernel-colocated driver tier | Planned | |

The planned **kernel-colocated tier** would let a few hot drivers (NVMe, a
network card) run in the kernel's address space without kernel privileges:
the drivers would be safe-Rust crates written against a narrow driver API,
isolated by protection keys (PKS) where the CPU has them and always fenced by
the IOMMU. The same driver would build either way, chosen per driver in
`ferric.toml`, so the two can be benchmarked. User-space drivers stay the
default. See [the roadmap](./ferric-roadmap.md#drivers-in-global-code-but-unprivileged).

[Hardware bring-up](./ferric-hardware.md) records which devices have been
tried on real machines.
