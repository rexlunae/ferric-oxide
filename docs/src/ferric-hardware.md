# Hardware bring-up

This chapter maps the current development machine to the drivers in this
repository. It is an inventory and a work plan, not a compatibility report:
the machine has not yet booted a Ferric Oxide image, so an entry saying a
driver binds describes the matching configuration, not a successful probe.
The status of each device still has to be confirmed from a boot log. The image
itself does boot under QEMU with emulated versions of the machine's devices,
which is described under [Booting the image in QEMU](#booting-the-image-in-qemu);
that is a check of the image, not of the hardware.

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
| Realtek RTL8125 2.5GbE | `10ec:8125` rev 05 | `rtl8125d`, ID `10ec:8125` | binds | New driver for the RTL8125B. It compiles for `x86_64-unknown-redox` and its register map was checked against the real card, but it has not run on it; QEMU does not emulate this controller. |
| Realtek RTL8922AE Wi-Fi 7 | `10ec:8922` | none | missing | Also supplies Bluetooth on this board. This is a Phase 5 effort and needs a firmware strategy and a native-versus-compatibility-layer decision. |

Ethernet is the highest-impact gap for a usable machine: without it the test
image cannot reach the network or fetch packages. `rtl8125d` is no longer a
missing driver, but it is not yet a proven one. It initialises the MAC and PHY
through the same OCP access paths as the permissively licensed FreeBSD `if_re`
driver, loads the RTL8125B MAC and PHY patch tables, applies the RTL8125B EPHY
register sequence, and uses the legacy 16-byte descriptor format with the new
interrupt map. The Linux and macOS drivers are GPL and were not used as
references.

The driver deliberately claims only the RTL8125B. It supports one RX queue, one
TX queue, 1500-byte frames, and the controller's legacy interrupt pin. It does
not implement jumbo frames, checksum or segmentation offload, VLAN stripping,
multiple queues, per-queue MSI-X vectors, or wake-on-LAN. The firmware tables
and the part of the initialisation that can be checked without MMIO live in the
host-tested `crates/rtl8125` crate, and the driver's register-level sequence
runs against a model of the controller in its own host tests, so the patch
tables, the OCP encodings and the interrupt setup are checked before the driver
ever sees the hardware.

The driver asks `pcid` for the legacy interrupt pin rather than a message
interrupt: the RTL8125B's MSI-X vectors belong to the individual queues, and
`pcid` hands out a single vector, so a queue's interrupt would be routed to a
vector nobody programmed.

The receive descriptor length includes the Ethernet CRC, so the driver strips
it before handing a frame to the network scheme. The scheme hands the frame to
`netstack`, whose Ethernet link buffer was enlarged from 1500 to 1514 bytes so
that a maximum-size frame is not truncated; the old size dropped the tail of
every full-MTU packet. The router's own MTU is that same 1500-byte IP payload,
taken from the link's constant; it was 1486 (one Ethernet header short), which
made smoltcp fragment egress that a frame carries whole and left the last slot
of a full receive ring without room for a maximum-size packet.

QEMU has no RTL8125 device model, so the driver cannot be exercised in the
smoke VM. Validation requires booting the hardware image on the machine and
checking the boot log for the detected MAC version, both patch versions, a
valid MAC address, and link-up after the PHY completes auto-negotiation. The
first bare-metal boot is also the first test of the DMA mapping and interrupt
path; until that log exists, the device-map entry means "the configuration
matches", not "the driver works".

### What the real card says

The card in the machine can be read from the host, because the Linux `r8169`
driver can dump its registers (`ethtool -d enp9s0`). The dump confirms the
assumptions the driver is built on:

| Register | Value on the machine | What the driver expects |
| --- | --- | --- |
| `0x0040` (TXCFG) | `0x6710_0F00` | `0x6710_0F00 & 0xFCF0_0000 = 0x6410_0000`, the RTL8125B |
| `0x006C` (PHY status) | `0x7080_0483` | bit 0 full duplex, bit 1 link up, bit 10 2.5 Gb/s |
| `0x0037` (command) | `0x0C` | transmit and receive enabled |
| `0x0053` (config 2) | `0xBC` | bit 5 set, the RTL8125 config bit, and bit 7, the ASPM lock |
| `0x0054` (config 3) | `0x60` | bit 4 clear, wake-on-LAN off |
| `0x0056` (config 5) | `0x03` | bit 0 set, the ASPM lock, and the wake-on-LAN bits clear |
| `0x0000` (station address) | `9c:6b:00:dd:f4:59` | the same address the host uses |

The version bits in the transmit configuration register survive the host
driver's writes, so the identification the driver performs at start-up does not
depend on the card being untouched. The register dump is a snapshot of a card
that the host's driver owns, so it confirms the register map and the status
bits, not the driver's own sequence.

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
cargo run -p ferric-config -- set image.config=ferric-hardware drivers.rtl8125=true
make all
```

It compiles the kernel, bootloader, relibc, RedoxFS and `base` from this
checkout. Build the rest from source as well: the validation image must contain
the drivers and services from this tree, not packages fetched from an upstream
binary repository. The report sits between `FERRIC_HARDWARE_BEGIN` and
`FERRIC_HARDWARE_END`. `FERRIC_HARDWARE_FAIL audio-scheme` means that no
`audiohw` scheme appeared within 30 seconds; the script does not shut the
machine down, so the console stays available for the next check.

QEMU can exercise the multi-controller audio path without the physical
machine by adding three HDA controllers: three `ich9-intel-hda` devices, each
with its own bus-qualified `hda-output` codec. A boot with three controllers
on 2026-10-19 initialized all of them, registered `audiohw` for the first and
`audiohw.pci-0000-00-04.0`/`audiohw.pci-0000-00-05.0` for the other two, and
`audiod` started and registered the `audio` scheme. This exercises the scheme
collision and fallback path, not the analog-versus-digital classification of
the physical codecs.

### Booting the image in QEMU

The image boots under QEMU with a device set that mirrors the machine, so the
image and the drivers in it can be checked before the machine is touched:

```sh
qemu-system-x86_64 -enable-kvm -cpu host -machine q35 -smp 4 -m 4096 \
    -bios /usr/share/ovmf/OVMF.fd \
    -device amd-iommu,intremap=off,dma-remap=on \
    -drive file=harddrive.img,format=raw,if=none,id=drv0 \
    -device nvme,drive=drv0,serial=NVME_SERIAL,addr=4.0 \
    -netdev user,id=net0 -device e1000,netdev=net0,addr=5.0 \
    -device qemu-xhci,id=xhci0,addr=6.0 \
    -device qemu-xhci,id=xhci1,addr=7.0 \
    -device qemu-xhci,id=xhci2,addr=8.0 \
    -device usb-kbd,bus=xhci0.0 \
    -device ich9-intel-hda,id=hda0 -device hda-output,bus=hda0.0 \
    -device ich9-intel-hda,id=hda1 -device hda-output,bus=hda1.0 \
    -device ich9-intel-hda,id=hda2 -device hda-output,bus=hda2.0 \
    -nographic -vga none -serial mon:stdio
```

That is the machine's NVMe disk, its three USB 3 controllers, its three audio
controllers and its AMD-Vi IOMMU, with QEMU's `e1000` standing in for the
network card, because QEMU has no RTL8125 model. The Ferric Oxide kernel has
no VFIO support, so the real card cannot be passed through to the guest
either; the RTL8125B only runs on the machine itself.

A boot of the image this way on 2026-10-20 brought up the kernel on the
machine's own CPU (an AMD Ryzen 9 9950X under KVM, with the x86-64-v3 feature
set the image asks for), enumerated the NVMe disk as
`disk.pci-0000-00-04.0-nvme`, registered the three xHCI controllers as
`usb.pci-0000-00-06.0_xhci` and its two siblings with a USB keyboard attached
to the first, registered `audiohw` for the first HDA controller and
`audiohw.pci-0000-00-03.0`/`audiohw.pci-0000-00-09.0` for the other two, and
brought up the emulated network card as `network.pci-0000-00-05.0_e1000`. The
image's own check reported `FERRIC_HARDWARE_PASS audio-scheme`. The RTL8125B
driver was in the image but had no device to bind to, which is the expected
result.

## Bring-up order

1. **Boot the test image on the machine.** Write it to a spare disk or USB
   device, boot it through UEFI, and capture the serial or screen log. Confirm
   the CPU feature check, the GOP framebuffer, NVMe enumeration, xHCI, USB
   input and the audio controller before changing more drivers. The image has
   booted under QEMU with emulated versions of these devices; the machine
   itself is the first run on real hardware.
2. **Validate RTL8125 Ethernet.** `rtl8125d` is in the tree but has only
   compiled, not run. Boot the hardware image and confirm that the driver finds
   the RTL8125B, reads a valid MAC address, accepts both firmware patch
   versions, and brings the link up. If initialisation stalls, use the MAC and
   PHY OCP timeouts and the recorded patch versions to narrow the failure
   before changing the register sequence.
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

### Receive queue saturation

The Ethernet link accepts a full 1500-byte IP packet plus its Ethernet header.
The router's receive and transmit rings each hold one 1500-byte packet per
metadata slot, so the byte ring is exactly its slot count times the MTU; a host
regression fills every slot with a maximum-size packet. The router's bounded
receive queue still checks allocation for each incoming packet, because free
packet slots alone do not imply that enough contiguous payload bytes remain.
When an enqueue fails, the router drops that frame and stops that device's polling
pass, then continues serving queued traffic. A host regression fills the byte
ring before its metadata slots and checks that subsequent traffic still works.

### RTL8125 table provenance

The firmware tables match the BSD-4-Clause Realtek vendor driver maintained by
Alex Dupre, pinned and identified in `ferric-rtl8125::firmware`. They are not in
FreeBSD's base `sys/dev/re/if_re.c`. Their original license is retained in
`crates/rtl8125/LICENSE-BSD-4-Clause` and referenced by `NOTICE.md`; images or
other binary distributions containing the driver must include that notice.
A word comparison validates the imported bytes, not execution on hardware.

`drivers.rtl8125` (Drivers menu, default off) selects the experimental driver.
Without it, `base` builds no RTL8125 daemon and installs no PCI match for it.
With it, the driver and match are installed together with `LICENSE-BSD-4-Clause`
and `NOTICE` under `/usr/share/doc/rtl8125d/`. Image CI enables it to compile the
driver; QEMU has no RTL8125 controller, so that is not physical-device validation.
The host-tools driver job also runs the RTL8125 controller-model and netstack
queue regression tests.

Select the `ferric-hardware` image in ferric-config as shown above, so generated
CPU, kernel and driver options apply to that image. A mismatching command-line
`CONFIG_NAME` override disables generated target options. RTL8125 devices with
no legacy interrupt line are refused with `ENODEV`: message-interrupt queue
routing is not implemented.

The RTL8125 daemon keeps setup and event-loop failures typed, prints their source
chains at its outer boundary and exits on failure. Unsupported interrupt routing
is rejected before allocating or starting the controller's DMA rings.
