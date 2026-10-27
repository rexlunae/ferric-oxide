# Features

This page compares Ferric Oxide with other operating systems on common
features. "Off by default" means the feature is implemented but has to be
turned on in `ferric.toml`; see [Implemented System](./ferric-oxide.md) for the
limits of each one.

| **Feature** | **Ferric Oxide** | **Linux** | **FreeBSD** | **Plan 9** |
|---|---|---|---|---|
| [SMP](https://en.wikipedia.org/wiki/Symmetric_multiprocessing) | Yes | Yes | Yes | Yes |
| [NUMA](https://en.wikipedia.org/wiki/Non-uniform_memory_access) | Partial: firmware tables parsed (`kernel.numa`, experimental); NUMA-aware placement planned | Yes | Yes | No |
| Full disk encryption | Yes (RedoxFS); LUKS1/LUKS2 volumes read-only | Yes | Yes | No |
| Exploit mitigations | Yes: KASLR, ASLR, KPTI, SMEP/SMAP; supervisor CET optional | Yes | Yes | No |
| [IOMMU](https://en.wikipedia.org/wiki/Input%E2%80%93output_memory_management_unit) DMA isolation | Yes, Intel VT-d and AMD-Vi (off by default) | Yes | Yes | No |
| Asynchronous I/O rings | Yes, `kernel.ring` (off by default) | Yes (io_uring) | No (POSIX AIO, kqueue) | No |
| Kernel tracing | Yes, x86-64 (off by default) | Yes | Yes (DTrace) | No |
| Hardware virtualization host | Planned | Yes (KVM) | Yes (bhyve) | No |
| OpenGL/Vulkan | Only OpenGL with CPU emulation | Yes | Yes | No |
| [UEFI](https://en.wikipedia.org/wiki/UEFI) boot loader | Yes | Yes | Yes | No |
| [IDE](https://en.wikipedia.org/wiki/Parallel_ATA) | Yes | Yes | Yes | Yes |
| [SATA](https://en.wikipedia.org/wiki/SATA) | Yes | Yes | Yes | Yes |
| [NVMe](https://en.wikipedia.org/wiki/NVM_Express) | Yes | Yes | Yes | No |
| [PCI](https://en.wikipedia.org/wiki/Peripheral_Component_Interconnect) and [PCIe](https://en.wikipedia.org/wiki/PCI_Express) | Yes | Yes | Yes | No |
| [USB](https://en.wikipedia.org/wiki/USB) | Yes (incomplete) | Yes | Yes | Yes |
| [Ethernet](https://en.wikipedia.org/wiki/Ethernet) | Yes | Yes | Yes | Yes |
| [Wi-Fi](https://en.wikipedia.org/wiki/Wi-Fi) | No (planned) | Yes | Yes | No |
| [Bluetooth](https://en.wikipedia.org/wiki/Bluetooth) | No (planned) | Yes | Yes | No |
| ext4 | Read-only (off by default) | Yes | Yes | No |
| LVM | Read-only (off by default) | Yes | No | No |

Phones and tablets are not a Ferric Oxide target. The
[roadmap](./ferric-roadmap.md) covers desktops, servers and real hardware.

## External References

- [Rust OS Comparison](https://github.com/flosse/rust-os-comparison) - A table comparing some Rust-written operating systems.
