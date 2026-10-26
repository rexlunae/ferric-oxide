# Hardware Support

Ferric Oxide targets modern machines. Support depends on the specific
hardware: some drivers are device-specific and others work across a whole
class of devices.

[HARDWARE.md](https://github.com/rexlunae/ferric-oxide/blob/main/HARDWARE.md)
lists tested computers. Most of its current entries were inherited from Redox
OS and describe Redox images; they are not proof that a Ferric Oxide image
boots on that machine. New reports name the Ferric Oxide commit they tested.
[Hardware Bring-up](./ferric-hardware.md) maps the project's development
machine to the drivers in this repository.

## Architectures and firmware

| Architecture | Status |
| --- | --- |
| x86-64 | Primary. User space is built for **x86-64-v3** by default; an optional **x86-64-v4** (AVX-512) build is available |
| ARM64 (aarch64) | Second architecture. ARMv8.0-A minimum, with ARMv8.2-A and ARMv9 variants. Boots a RAM-backed live image in QEMU; persistent storage and hardware acceleration are incomplete |
| RISC-V 64 | Frozen: imported code remains, but it is not being developed |
| i586 / 32-bit x86 | Not supported |

Boot is **UEFI only**. BIOS boot code remains in imported sources but is not
supported or extended.

## CPU requirements on x86-64

x86-64-v3 means a CPU with AVX2, BMI1/2, FMA, F16C, LZCNT and MOVBE: Intel
Haswell (2013) and later, or AMD Excavator/Zen and later. This floor was
chosen over v4 because Intel client chips from Alder Lake on ship without
AVX-512, so a v4 floor would exclude most Intel laptops. AMD Zen 4 and 5 and
Intel Xeon support v4.

The kernel checks the CPU against the configured level at boot and halts with
a list of missing features, instead of letting user space fail later with an
invalid-opcode fault.

The platform the roadmap assumes also has NX, SMEP, SMAP, UMIP, PCID,
FSGSBASE, XSAVE, x2APIC, an IOMMU (Intel VT-d or AMD-Vi) and a TPM 2.0. Not
all of these are enforced yet; [Memory and CPU Protection](./ferric-memory.md)
says which are checked and used today. Where the CPU has them, Ferric Oxide
can use CET (indirect-branch tracking and shadow stacks, in the kernel),
AVX-512 and, in future, protection keys.

## I have an older computer. Will Ferric Oxide work on it?

Not if it predates x86-64-v3. Supporting older CPUs would mean keeping
fallback paths in the kernel and building every package for a lower baseline,
which costs performance on the hardware the project targets. Redox OS supports
a much wider range of hardware, down to i586, and is the better choice for
older machines.

The feature catalog still offers the v1 and v2 levels inherited from
upstream, but new Ferric Oxide development does not target them.

## Devices

The drivers below are inherited from Redox unless noted. "In the tree" means a
driver exists; it does not mean every device of that class has been tested.

| Category | In the tree |
|--------------|-----------|
| Hardware interfaces | ACPI, PCI and PCIe with MSI/MSI-X, USB (xHCI) |
| DMA isolation | Intel VT-d and AMD-Vi IOMMU domains (Ferric Oxide; off by default) |
| Storage | NVMe (with optional per-CPU queues and registered-buffer DMA), SATA (AHCI), IDE, USB mass storage, virtio-blk |
| Video | UEFI GOP framebuffer, Intel GPU, virtio-gpu |
| Sound | Intel HDA, AC'97, Sound Blaster 16 |
| Input | PS/2 and USB keyboards, mice and touchpads |
| Ethernet | Intel e1000 and ixgbe, Realtek RTL8139 and RTL8168, virtio-net; Realtek RTL8125B (`rtl8125d`, Ferric Oxide, compiled but not yet run on hardware) |
| Wi-Fi, Bluetooth | None yet (planned, Phase 5) |

Under QEMU, the emulated NVMe, e1000, virtio and xHCI devices are the
best-tested configuration.
