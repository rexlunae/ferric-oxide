# Hardware bring-up

This chapter maps the current development machine to the drivers in this
repository. It is an inventory and a work plan, not a compatibility report:
the machine has not yet booted a Ferric Oxide image, so an entry saying a
driver binds describes the matching configuration, not a successful probe.
The status of each device still has to be confirmed from a boot log.

The inventory was taken from Linux on 2026-10-19 with `lspci -nn` and
`dmidecode`. Update this page when a driver is added, a binding changes, or a
boot result is recorded in [`HARDWARE.md`](https://github.com/rexlunae/ferric-oxide/blob/main/HARDWARE.md).

## The machine

| Component | Model |
| --- | --- |
| Mainboard | ASRock X870 Challenger WiFi White, AMI UEFI 4.60 (2026-09-17) |
| CPU | AMD Ryzen 9 9950X, 16 cores / 32 threads, Zen 5, AVX-512 |
| Memory | 73 GiB usable, two NUMA nodes reported (one populated) |
| Firmware | UEFI, 64 MiB flash, no legacy BIOS boot target |
| Root disk on Linux | Samsung 9100 PRO 2 TB NVMe (`144d:a810`) |
| Network on Linux | Realtek RTL8125 2.5GbE (`10ec:8125`), RTL8922AE Wi-Fi 7 (`10ec:8922`) |
| Display on Linux | Intel Arc Pro B50 (`8086:e212`), AMD Radeon iGPU (`1002:13c0`) |

The CPU is comfortably above the x86-64-v3 baseline and supports the optional
v4/AVX-512 image. The kernel's boot-time feature check is the first thing a
hardware boot should confirm.

## Device map

Status meanings: **binds** means a `pcid` configuration matches the device;
**boot path** means the device is used indirectly through the firmware or an
existing service; **missing** means there is no driver in this checkout.

### Storage

| Device | ID | Driver and match | Status | Notes |
| --- | --- | --- | --- | --- |
| Samsung 9100 PRO NVMe | `144d:a810` | `nvmed`, class 1 subclass 8 | binds | Multi-queue and per-CPU queue options exist; not yet booted on this disk |
| AMD 600-series SATA | `1022:43f6` | `ahcid`, class 1 subclass 6 | binds | No SATA disk is currently attached |
| RedoxFS root | — | `redoxfs` | boot path | The Linux root is not a RedoxFS volume; a test image needs its own disk, partition or USB device |

### USB

| Device | ID | Driver and match | Status | Notes |
| --- | --- | --- | --- | --- |
| AMD 800-series xHCI | `1022:43fc` | `xhcid`, class 0x0c subclass 0x03 interface 0x30 | binds | |
| AMD Granite Ridge xHCI | `1022:15b6`, `1022:15b7`, `1022:15b8` | `xhcid`, same match | binds | Each controller registers a unique `usb.<pci-address>` scheme |
| ASMedia ASM4242 xHCI | `1b21:2426` | `xhcid`, same match | binds | |
| ASMedia USB4/Thunderbolt router | `1b21:2425` | none | missing | USB4 tunnelling, PCIe tunnelling and Thunderbolt are not supported |

USB keyboards, mice, hubs and mass storage depend on the xHCI controller and
the USB class drivers. They are the first input path to test on the machine,
since there is no PS/2 port to fall back to.

### Networking

| Device | ID | Driver and match | Status | Notes |
| --- | --- | --- | --- | --- |
| Realtek RTL8125 2.5GbE | `10ec:8125` rev 05 | none | missing | The existing `rtl8168d` matches only `0x8168` and `0x8169`. RTL8125 is not a PCI-ID-only addition: its driver needs the new descriptor/interrupt path and the MAC/PHY firmware patch tables. |
| Realtek RTL8922AE Wi-Fi 7 | `10ec:8922` | none | missing | Also supplies Bluetooth on this board. This is a Phase 5 effort and needs a firmware strategy and a native-versus-compatibility-layer decision. |

Ethernet is the highest-impact gap for a usable machine: without it the test
image cannot reach the network or fetch packages. The work is a driver port,
not a configuration change. The permissively licensed FreeBSD `if_re` driver
is the practical reference for RTL8125; the Linux and macOS drivers are GPL
and cannot be ported into the MIT tree.

### Audio

| Device | ID | Driver and match | Status | Notes |
| --- | --- | --- | --- | --- |
| AMD Ryzen HD Audio (onboard codec) | `1022:15e3` | `ihdad`, class 0x04 subclass 0x03 | binds | Should own the `audiohw` scheme when the codec exposes an analog output pin |
| AMD Radeon HDMI/DP audio | `1002:1640` | `ihdad`, same match | binds | Digital-only controller; registers a unique `audiohw.<pci-address>` scheme after the multi-controller change |
| Intel Arc HDMI/DP audio | `8086:e2f7` | `ihdad`, same match | binds | Same digital-only path |

`ihdad` used to register every HDA controller as `audiohw`, so only one of the
three controllers could win and the others panicked. It now classifies the
output pins: an analog codec gets `audiohw`, digital-only codecs get unique
names, and `audiod` falls back to the sorted `audiohw.*` names when no analog
codec exists. The output-pin preference also includes line-out, which desktop
boards use instead of the headphone or speaker pins. The driver still supports
one codec per controller and does not implement HDMI channel mapping or
unsolicited jack-detection events.

### Graphics and display

| Device | ID | Driver and match | Status | Notes |
| --- | --- | --- | --- | --- |
| Intel Arc Pro B50 (Battlemage) | `8086:e212` | none | missing | `ihdgd` lists older Intel generations only; Battlemage needs the Xe2 display engine. No GOP framebuffer driver binds to the PCI device. |
| AMD Radeon iGPU (Granite Ridge) | `1002:13c0` | none | missing | No AMD GPU driver. |
| UEFI GOP framebuffer | — | `vesad` reads the boot framebuffer | boot path | Works only if the firmware exposes a GOP framebuffer for the display output in use; no modesetting or acceleration |

The desktop can only use whichever GPU the firmware has initialised and whose
framebuffer the bootloader passes on. A native DRM/KMS interface for Mesa is
Phase 6 work and is not a quick fix for either GPU.

### Platform, firmware and sensors

| Device | ID | Driver and match | Status | Notes |
| --- | --- | --- | --- | --- |
| AMD IOMMU (AMD-Vi) | `1022:14d9` | kernel `amdvi` | binds | Used when `kernel.iommu` enables DMA domains; interrupt remapping is not implemented |
| AMD PSP/CCP | `1022:1649` | none | missing | Not required for boot; no crypto offload |
| AMD FCH SMBus | `1022:790b` | none | missing | No fan, voltage or temperature monitoring; `k10temp` has no equivalent |
| ACPI | — | `acpid` and the AML interpreter | boot path | The board's ACPI tables have not been exercised; modern ASRock firmware is a likely source of AML gaps |
| TPM 2.0 | — | none | missing | Not required for this bring-up; the Phase 4 measured-boot work will need it |

CPU frequency scaling, C-states, suspend/resume and fan control are not
implemented. They are Phase 5 items and matter for a laptop or a quiet
desktop, but they do not block a first boot.

## Test image

`config/ferric-hardware.toml` builds an image that prints a CPU and scheme
inventory on the console and then starts the login consoles:

```sh
make CONFIG_NAME=ferric-hardware REPO_BINARY=1 all
```

It compiles the kernel, bootloader, relibc, RedoxFS and `base` from this
checkout and downloads the other packages prebuilt. The report sits between
`FERRIC_HARDWARE_BEGIN` and `FERRIC_HARDWARE_END`. `FERRIC_HARDWARE_FAIL
audio-scheme` means that no `audiohw` scheme appeared within 30 seconds; the
script does not shut the machine down, so the console stays available for the
next check.

QEMU can exercise the multi-controller audio path without the physical
machine by adding three HDA controllers: the q35 `ich9-intel-hda` device plus
two `intel-hda` devices, each paired with an `hda-output` codec. A boot with
three controllers on 2026-10-19 initialized all of them, registered `audiohw`
for the first and `audiohw.pci-0000-00-04.0`/`audiohw.pci-0000-00-05.0` for
the other two, and `audiod` started and registered the `audio` scheme. This
exercises the scheme collision and fallback path, not the analog-versus-
digital classification of the physical codecs.

## Bring-up order

1. **Boot the test image on the machine.** Write it to a spare disk or USB
   device, boot it through UEFI, and capture the serial or screen log. Confirm
   the CPU feature check, the GOP framebuffer, NVMe enumeration, xHCI, USB
   input and the audio controller before changing more drivers.
2. **RTL8125 Ethernet.** Port the RTL8125 initialisation, descriptor rings,
   interrupt path and firmware tables from the permissively licensed FreeBSD
   driver, or implement them from the register documentation if it is
   available. Keep the driver in a new crate rather than growing the
   RTL8168 register layout into a chip-specific tangle.
3. **ACPI and platform devices.** Exercise the AML interpreter on this board,
   then add thermal and fan reporting and CPU frequency/C-state support.
4. **Wi-Fi and Bluetooth.** Choose native Rust or a LinuxKPI-style driver
   domain before writing code. The licensing decision comes first because the
   GPL driver cannot be linked into the MIT tree.
5. **Graphics.** Use GOP for the first working desktop, then add a DRM/KMS
   interface and Mesa support for Xe2 and RDNA in Phase 6.

The hardware matrix is deliberately separate from the implementation status
table in [Implemented System](ferric-oxide.md): a driver that binds is not a
driver that has passed a hardware test.
