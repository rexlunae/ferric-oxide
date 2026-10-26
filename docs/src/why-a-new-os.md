# Why Ferric Oxide?

Ferric Oxide exists to build a Rust operating system people can actually use
for real work: secure because of its design, and fast because the design is
measured and tuned. It starts from Redox OS, which already showed that a
complete microkernel system can be written in Rust, and takes it in a
direction Redox has not chosen.

## Why start from Redox

Writing an operating system from scratch takes many years before it can run
real programs. Redox has spent a decade building a working kernel, bootloader,
file system, C library, display server, drivers and a large set of ported
programs, all in Rust and all MIT licensed. Forking it lets Ferric Oxide spend
its effort on the parts it wants to change rather than reinventing the parts
that already work. The kernel, relibc, bootloader, base services and RedoxFS
were imported with their history; since then they have been maintained here,
independently of Redox.

## Why a fork rather than contributions

Two differences make a separate project necessary.

**Development process.** Redox does not accept contributions written with AI
tools. Ferric Oxide does, with a human accountable for every change and the
same review, testing and documentation requirements as any other contribution.
This lets the project attempt changes larger than its maintainers could write
by hand, such as a new I/O architecture, clean-room Linux file systems and a
hypervisor. Redox's choice is respected: AI-assisted work is never sent
upstream.

**Technical direction.** Ferric Oxide makes choices that suit its goals but
would not suit an upstream with broader ones:

- **Modern hardware only.** x86-64-v3 and UEFI as the floor, ARM64 as the
  second architecture, and no i586 or BIOS paths. The kernel can rely on
  features such as an IOMMU and XSAVE instead of carrying fallbacks.
- **Performance as a requirement.** Structural kernel changes (an IOMMU-fenced
  shared-memory data plane, per-core queues, a page cache, and a scheduler
  being reworked toward an event-driven design) land when benchmarks justify
  them.
- **Configuration over divergence.** Optional work is a feature toggle, so
  experiments live in the main tree without being forced on every image.
- **Supply-chain verifiability.** The `fox` package manager replaces `pkg`
  with signed git sources, build attestations and SBOMs.
- **Linux interoperability without GPL code.** Clean-room ext4, LVM, LUKS and
  (planned) XFS, and planned microVM enclaves for software that expects
  another OS's security model.

## Written in Rust

Wherever possible, Ferric Oxide is written in [Rust](https://www.rust-lang.org/).
Rust's ownership and type rules rule out buffer overruns, use-after-free,
data races and the other [memory errors](https://en.wikipedia.org/wiki/Memory_safety#Types_of_memory_errors)
that cause most security vulnerabilities in operating systems, and the
compiler catches them before the code runs. [Why Rust?](./why-rust.md) goes
into more detail.

## Microkernel architecture

A [microkernel](https://en.wikipedia.org/wiki/Microkernel) moves as much of
the operating system as possible out of the kernel. Drivers, file systems and
the network stack run as separate user-space processes ("daemons"), each with
only the resources it needs. The kernel manages memory, scheduling and
communication between them.

In a [monolithic kernel](https://en.wikipedia.org/wiki/Monolithic_kernel), all
of that code shares one address space with full privileges, so one bug in a
driver can crash or compromise the whole machine. In Ferric Oxide a failing
driver or service can be restarted, and a compromised one is confined to what
it was given. With an IOMMU domain per driver (implemented, off by default),
that confinement extends to the device's DMA as well.

The benefits:

- **Fault isolation.** A crash in a driver or service doesn't take down the
  kernel or other services.
- **Smaller attack surface.** Less code runs with kernel privileges, and
  services hold only the capabilities they need.
- **Restartable, updatable components.** Services can be restarted or
  replaced without rebooting.
- **Independent development and testing.** Services and drivers are ordinary
  programs that can be built and tested separately.

The traditional cost is performance: each request crosses the kernel several
times and data is copied between processes. Removing that cost without giving
up the isolation is Ferric Oxide's central design work. See
[Our Goals](./our-goals.md#a-microkernel-without-the-ipc-tax) and
[Microkernels](./microkernels.md).

## File systems

The default file system is [RedoxFS](./redoxfs.md), a copy-on-write file
system with checksums and optional encryption. Ferric Oxide adds read-only
ext4, LVM2 and LUKS support so it can share disks with Linux, with writable
ext4 and XFS planned. Choosing the default root file system is still an open
decision.

## Unix-like tools and API

Ferric Oxide provides a Unix-like command line with familiar tools, many of
them written in Rust, and a POSIX-like programming interface through relibc.
Many Linux and BSD programs run after recompilation, and Rust programs that
use the standard library generally work unchanged. The project prefers Rust
for system components, but is agnostic about the languages of the programs
users choose to run.
