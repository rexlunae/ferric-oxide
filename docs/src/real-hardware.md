# Running on Real Hardware

Ferric Oxide targets modern UEFI machines with an x86-64-v3 CPU (see
[Hardware Support](./hardware-support.md)). Real-hardware support is early:
the project's own development machine has not yet booted a Ferric Oxide
image (see [Hardware Bring-up](./ferric-hardware.md)), and most entries in
[HARDWARE.md](https://github.com/rexlunae/ferric-oxide/blob/main/HARDWARE.md)
were inherited from Redox OS. Reports from your own machines are very
welcome.

> ⚠️ **Warning:** Back up your data before booting experimental software on a
> computer you care about. The live image runs from RAM and doesn't write to
> your disks unless you run the installer, but bugs are possible.

## Build a live image

A live image loads the whole system into RAM, so it can boot from a USB drive
without installing anything:

```sh
cargo xtask build -- live=yes
```

The result is `build/x86_64/<config>/redox-live.iso` (the image configuration
from your `ferric.toml`, `native` by default). For a desktop, choose a desktop
configuration first:

```sh
cargo run -q -p ferric-config -- set image.config=desktop
cargo xtask build -- live=yes
```

## Write it to a USB drive

On Linux, [Popsicle](https://github.com/pop-os/popsicle) or `dd` both work.
With `dd`, check the device name carefully first (`lsblk`), because the drive
is overwritten:

```sh
sudo dd if=build/x86_64/desktop/redox-live.iso of=/dev/sdX bs=4M status=progress conv=fsync
```

On macOS, find the disk with `diskutil list`, unmount it, and write to the raw
device:

```sh
diskutil unmountDisk /dev/diskN
sudo dd if=build/x86_64/desktop/redox-live.iso of=/dev/rdiskN bs=4m
```

On Windows, [balenaEtcher](https://etcher.balena.io/) writes the image.

## Boot it

Open your firmware's boot menu (often Esc, F2, F8, F11 or F12 at power-on) and
choose the USB drive. The firmware must boot in UEFI mode; BIOS or "legacy"
boot is not supported. If Secure Boot is on, turn it off: Ferric Oxide's
bootloader is not signed yet.

The kernel checks the CPU against the image's baseline early in boot and
stops with a list of missing features if the CPU is too old.

Desktop images start the Orbital GUI. Log in as `user` with no password; the
`root` password is `password`. To switch between Orbital and the console:

- F1: the console log messages
- F2: a text-only terminal
- F3: back to the Orbital GUI

To boot from an internal disk instead, see [Installing](./installing.md).

## Report what you find

Whatever happens, a report helps: add a row to
[HARDWARE.md](https://github.com/rexlunae/ferric-oxide/blob/main/HARDWARE.md)
in a pull request, or [open an issue](./creating-proper-bug-reports.md) with
the commit you built, the machine model and, if it fails, a photo of the
screen or the serial log.
