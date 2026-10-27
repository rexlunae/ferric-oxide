# Running in a Virtual Machine

QEMU is the main way to run Ferric Oxide today. There are no release images
yet, so first build one (see [Building Ferric Oxide](./building-redox.md)).

## Running with cargo xtask

```sh
cargo xtask run                  # build if needed, then boot with a display
cargo xtask run --headless       # serial console in the terminal, no display
cargo xtask run --no-build       # boot the image you already built
cargo xtask --arch x86_64 run    # boot the other architecture's image
```

`run` uses the same image `build` produces: a persistent `harddrive.img` for
x86-64, and a RAM-backed `redox-live.iso` for ARM64 (changes to a live image
are lost when QEMU exits). It boots through UEFI firmware, which QEMU usually
ships as `edk2-x86_64-code.fd` and `edk2-aarch64-code.fd`. The build looks in
the usual Linux, Homebrew and MacPorts locations; if it can't find the
firmware, point it there explicitly:

```sh
cargo xtask run -- PFLASH0=/path/to/edk2-x86_64-code.fd
```

### Acceleration and CPU model

On a host with the same architecture as the guest, QEMU uses hardware
acceleration (KVM on Linux) where it can. Cross-architecture guests, and ARM64
guests on Apple Silicon, use software emulation, which is much slower.

Without acceleration, x86-64 images built for x86-64-v3 or v4 use QEMU's `max`
CPU model so the guest has the instructions the image needs. The kernel checks
the CPU at boot and stops with a list of missing features if the model is too
old.

### Common options

Options go after `--`, as Make assignments:

| Option | Effect |
| --- | --- |
| `QEMU_MEM=4096` | Guest memory in MiB |
| `QEMU_SMP=8` | Number of virtual CPUs |
| `kvm=no` | Turn off hardware acceleration (useful to make timing-dependent bugs easier to reproduce) |
| `gpu=no` | No display; the same as `--headless` |
| `gpu=virtio` | A virtio GPU instead of the standard VGA adapter |
| `live=yes` / `live=no` | Boot the live image or the persistent disk, overriding `qemu.live` |
| `iommu=yes` | Add an emulated Intel IOMMU (for testing the [IOMMU support](./ferric-io.md)) |
| `audio=no` | No sound device |
| `net=no` | No network device |
| `gdb=yes` | Wait for a debugger on QEMU's GDB port |

The saved defaults for several of these are in the `qemu` menu of
`make menuconfig`. `mk/qemu.mk` has the full list.

### Seeing the QEMU command

The Make target prints the QEMU command line before running it, so you can
copy it and adjust it by hand:

```sh
make qemu-run gpu=no
```

## Using the emulation

With a display, the boot asks for a screen resolution, then starts the
**Orbital** desktop (in desktop images) and shows a login screen. Log in as
`user` with no password; the `root` password is `password`.

**Ctrl+Alt+G** releases the mouse from the QEMU window, and **Ctrl+Alt+F**
toggles full screen. In headless mode, the serial console is your terminal;
quit QEMU with **Ctrl-a x**.

See [Trying Out the System](./trying-out-redox.md) and [Tasks](./tasks.md) for
things to try.

## Other virtual machines

Ferric Oxide is developed and tested on QEMU. Other hypervisors that boot a
raw disk image through UEFI on a modern virtual CPU may work, but they aren't
tested. On VirtualBox, for example, the VM must use EFI firmware and expose a
CPU with AVX2, and the image must first be converted:

```sh
VBoxManage convertfromraw build/x86_64/native/harddrive.img ferric.vdi
```

Please [report](./creating-proper-bug-reports.md) what works and what doesn't.
