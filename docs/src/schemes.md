# Schemes

The **scheme**, which takes its name from [URI schemes](https://en.wikipedia.org/wiki/List_of_URI_schemes), identifies the type of resource,
and identifies the manager daemon responsible for that resource.

Within Ferric Oxide, a scheme may be thought of in a few ways. It is all of these things:

- The **type** of a resource, such as "file", "NVMe drive", "TCP connection", etc. (Note that these are not valid scheme names, they are just given by way of example.)
- The starting point for locating the resource, i.e. it is the root of the path to the resource, which the system can then use in establishing a connection to the resource.
- A **uniquely named service** that is provided by some driver or daemon program, with the full path identifying a specific resource accessed via that service.

## Scheme Daemons

A scheme is typically provided by a **daemon**.
A daemon is a program that runs as a process in userspace;
it is typically started by the system during boot.
When the process starts, it registers with kernel using the name of the scheme that it manages.

## Kernel vs. Userspace Schemes

A [userspace scheme](#userspace-schemes) is implemented by a scheme daemon,
described above.
A [kernel scheme](#kernel-schemes) is implemented within the kernel,
and manages critical resources not easily managed with a userspace daemon.
When possible, schemes should be implemented in userspace.

## Accessing Resources

In order to provide "virtual file" behavior, schemes generally implement file-like operations.
However, it is up to the scheme provider to determine what each file-like operation means.
For example, `seek` to an SSD driver scheme might simply add to a file offset, but to a floppy disk controller scheme,
it might cause the physical movement of disk read-write heads.

Typical scheme operations include:

- `open` - Create a **handle** (file descriptor) to a resource provided by the scheme. e.g. `File::create("/scheme/tcp/127.0.0.1/3000")` in a regular program would be converted by the kernel into `open("127.0.0.1/3000")` and sent to the "tcp" scheme provider. The "tcp" scheme provider would parse the name, establish a connection to Internet address "127.0.0.1", port "3000", and return a handle that represents that connection.
- `read` - get some data from the thing represented by the handle, normally consuming that data so the next `read` will return new data.
- `write` - send some data to the thing represented by the handle to be saved, sent or written.
- `seek` - change the logical location where the next `read` or `write` will occur. This may or may not cause some action by the scheme provider.

Schemes may choose to provide other standard operations, such as `mkdir`, but the meaning of the operation is up to the scheme. `mkdir` might create a directory entry, or it might create some type of substructure or container relevant to that particular scheme.

Some schemes implement `fmap`, which creates a memory-mapped area that is shared between the scheme resource and the scheme user. It allows direct memory operations on the resource, rather than reading and writing to a file descriptor. The most common use case for `fmap` is for a device driver to access the physical addresses of a memory-mapped device, using the `memory:` kernel scheme. It is also used for frame buffers in the graphics subsystem.

> TODO: add F-operations.

> TODO: explain file-like vs. socket-like schemes.

## Userspace Schemes

User-space schemes are served by daemons, most of which init starts during
boot (see [Boot Process](./boot-process.md)). Each daemon can provide one or
more schemes.

| **Scheme** | **Daemon** | **Description** |
|------------|------------|-----------------|
| disk.* | ided, ahcid, nvmed, virtio-blkd | Storage drivers |
| disk.live | lived | RAM-disk driver that loads the live image into RAM |
| disk.usb-{id}+{port}-scsi | usbscsid | USB SCSI driver |
| logging | [ramfs](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/ramfs) | Error logging scheme, using an in-memory temporary filesystem |
| initfs | [bootstrap](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/bootstrap) | Startup filesystem |
| file | [redoxfs](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/redoxfs) | Main filesystem |
| network.* | e1000d, ixgbed, rtl8139d, rtl8168d, rtl8125d, virtio-netd | Link-level network send/receive; with `drivers.net_bypass`, also kernel-bypass queue sockets |
| ip | [smolnetd](https://github.com/rexlunae/ferric-oxide/blob/main/userspace/base/netstack/src/scheme/ip.rs) | Raw IP packet send/receive |
| tcp | [smolnetd](https://github.com/rexlunae/ferric-oxide/blob/main/userspace/base/netstack/src/scheme/tcp.rs) | TCP sockets |
| udp | [smolnetd](https://github.com/rexlunae/ferric-oxide/blob/main/userspace/base/netstack/src/scheme/udp.rs) | UDP sockets |
| icmp | [smolnetd](https://github.com/rexlunae/ferric-oxide/blob/main/userspace/base/netstack/src/scheme/icmp.rs) | ICMP protocol |
| netcfg | [smolnetd](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/netstack/src/scheme/netcfg) | Network configuration |
| display.vesa | vesad | Firmware framebuffer driver |
| display.virtio-gpu | virtio-gpud | VirtIO GPU driver |
| orbital | orbital (recipe `gui/orbital`) | Windowing system (window manager and virtual driver) |
| pty | [ptyd](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/ptyd) | Pseudoterminals, used by terminal emulators |
| audiorw | sb16d, ac97d, ihdad | Sound drivers |
| audio | [audiod](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/audiod) | Audio manager and virtual device |
| usb.* | xhcid | USB host controller; hub and HID drivers attach through it |
| acpi | acpid | ACPI driver |
| pci | pcid | PCI devices and the driver interface |
| input | inputd | Virtual device |
| sudo | sudo (recipe `core/userutils`) | Privilege manager |
| chan | [ipcd](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/ipcd) | Inter-process communication |
| shm | [ipcd](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/ipcd) | Shared memory manager |
| log | [logd](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/logd) | Logging |
| rand | [randd](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/randd) | Pseudo-random number generator |
| zero | [zerod](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/zerod) | Discard all writes, and always fill read buffers with zeroes |
| null | [zerod](https://github.com/rexlunae/ferric-oxide/tree/main/userspace/base/zerod) | Discard all writes, and read no bytes |

Daemons started by hand add their own schemes, for example `ext4d` for an ext4
volume and `ferric-blockd` for LUKS and LVM volumes; see the
[I/O chapter](./ferric-io.md).

## Kernel Schemes

The kernel provides a small number of schemes in order to support user space.
Their code is in [`crates/kernel/src/scheme/`](https://github.com/rexlunae/ferric-oxide/tree/main/crates/kernel/src/scheme/). The schemes marked
"Ferric Oxide" exist only when their feature option is on.

| **Name** | **Source** | **Description**  |
|----------|------------|------------------|
| debug | [debug.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/debug.rs) | Debug messages that can't use the `log:` scheme |
| event | [event.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/event.rs) | epoll-like file descriptor read/write "ready" events |
| memory | [memory.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/memory.rs) | Physical memory mapping and, with an IOMMU, DMA address translation |
| pipe | [pipe.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/pipe.rs) | Kernel manager for pipes |
| serio | [serio.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/serio.rs) | Serial I/O (PS/2) driver (must stay in the kernel due to PS/2 protocol issues) |
| irq | [irq.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/irq.rs) | Interrupt manager (converts interrupts to messages); per-CPU counts and steering with `kernel.irq_percpu` |
| time | [time.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/time.rs) | Clocks and timers |
| sys | [sys/](https://github.com/rexlunae/ferric-oxide/tree/main/crates/kernel/src/scheme/sys) | System information: contexts, CPUs, block and I/O statistics, the kernel log |
| kernel.proc | [proc.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/proc.rs) | Process and context management |
| kernel.acpi | [acpi.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/acpi.rs) | ACPI tables |
| kernel.dtb | [dtb.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/dtb.rs) | The device tree, on device-tree platforms |
| kernel.iommu | [iommu.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/iommu.rs) | Ferric Oxide: IOMMU DMA domains (`kernel.iommu`) |
| kernel.regbuf | [regbuf.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/regbuf.rs) | Ferric Oxide: registered buffers and grants (`kernel.regbuf`) |
| kernel.ring | [ring.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/ring.rs) | Ferric Oxide: submission/completion rings (`kernel.rings`) |
| kernel.pcache | [pcache.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/pcache.rs) | Ferric Oxide: page cache read path (`kernel.page_cache`) |
| kernel.trace | [trace.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/trace.rs) | Ferric Oxide: tracepoints, trace rings and profiles (`kernel.trace`) |

User-space schemes reach the kernel through [user.rs](https://github.com/rexlunae/ferric-oxide/blob/main/crates/kernel/src/scheme/user.rs),
which turns system calls on their descriptors into messages to the daemon.
Namespaces are served in user space, by the namespace manager that bootstrap
starts.
