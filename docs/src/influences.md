# Influences

This page explains which systems influenced Ferric Oxide. The first group shaped Redox, whose design Ferric Oxide inherits; the second shaped the changes Ferric Oxide is making.

## Inherited from Redox

(Ordered by influence level)

### [Minix](https://minix3.org/)

The most influential Unix-like system with a microkernel. It has advanced features such as system modularity, [kernel panic](https://en.wikipedia.org/wiki/Kernel_panic) resistance, driver reincarnation, protection against bad drivers and secure interfaces for [process communication](https://en.wikipedia.org/wiki/Inter-process_communication).

Redox, and so Ferric Oxide, is largely influenced by Minix: it has a similar architecture, with a feature set written in Rust.

- [How Minix influenced the design](./microkernels.md)

### [seL4](https://sel4.systems/)

The most performant and simplest microkernel of the world.

The kernel follows the same principles, trying to make the kernel-space small as possible (moving components to user-space and reducing the number of system calls, passing the complexity to user-space) and keeping the overall performance good (reducing the context switch cost).

### [Plan 9](http://9p.io/plan9/index.html)

This Bell Labs OS brings the concept of "Everything is a File" to the highest level, doing all the system communication from the filesystem.

- [Drew DeVault explains the Plan 9](https://drewdevault.com/2022/11/12/In-praise-of-Plan-9.html)
- [Plan 9's influence on schemes](./schemes-resources.md)

### [Linux](https://www.kernel.org/)

The most advanced monolithic kernel and biggest open-source project of the world. It brought several improvements and optimizations to the Unix-like world.

Ferric Oxide tries to match Linux's performance in a microkernel design; Linux on the same hardware is the baseline its benchmarks are measured against.

### [BSD](https://en.wikipedia.org/wiki/Berkeley_Software_Distribution)

This Unix [family](https://en.wikipedia.org/wiki/Research_Unix) included several improvements on Unix systems and the open-source variants of BSD added many improvements to the original system (like Linux did).

- [FreeBSD](https://www.freebsd.org/) - The [Capsicum](https://man.freebsd.org/cgi/man.cgi?capsicum(4)) (a capability-based system) and [jails](https://en.wikipedia.org/wiki/Freebsd_jail) (a sandbox technology) influenced the namespaces implementation. Also part of the system tooling was ported/reimplemented in Rust in the beginning.

- [OpenBSD](https://www.openbsd.org/) - The [system call](https://man.openbsd.org/pledge.2), [filesystem](https://man.openbsd.org/unveil.2), [display server](https://www.xenocara.org/) and [audio server](https://man.openbsd.org/sndiod.8) sandbox and [others](https://www.openbsd.org/innovations.html) influenced the security design.

## Ferric Oxide's newer influences

- **Linux [io_uring](https://kernel.dk/io_uring.pdf) and [AF_XDP](https://docs.kernel.org/networking/af_xdp.html)**: shared-memory submission and completion rings, registered buffers and kernel-bypass packet queues are the model for the [rings and network queues](./ferric-io.md).
- **[Theseus](https://github.com/theseus-os/Theseus) and [Singularity](https://www.microsoft.com/en-us/research/project/singularity/)**: language-based isolation, behind the planned safe-Rust colocated driver tier and the WebAssembly userland's colocated mode.
- **[Firecracker](https://firecracker-microvm.github.io/) and [cloud-hypervisor](https://www.cloudhypervisor.org/)**: a minimal Rust VMM in user space and millisecond-boot microVMs, the model for the planned [enclaves](./ferric-virt.md).
- **[Qubes OS](https://www.qubes-os.org/) and Xen driver domains**: running untrusted drivers in their own VM, one planned option for Wi-Fi and GPU drivers.
- **[FreeBSD's LinuxKPI](https://wiki.freebsd.org/LinuxKPI)**: the other option for reusing Linux drivers.
- **[WebAssembly](https://webassembly.org/), [Wasmtime](https://wasmtime.dev/) and [VeriWasm](https://github.com/PLSysSec/veriwasm)**: a portable, sandboxed userland compiled per CPU, with a small verifier instead of a trusted JIT.
- **[TUF](https://theupdateframework.io/), [in-toto](https://in-toto.io/)-style build attestations and [CycloneDX](https://cyclonedx.org/)**: signed registry metadata, attested builds and SBOMs in [`fox`](./ferric-packaging.md).
- **[eBPF](https://ebpf.io/), [ftrace](https://docs.kernel.org/trace/ftrace.html) and [Perfetto](https://perfetto.dev/)**: static-key tracepoints, per-CPU trace rings and the trace export formats in [kernel tracing](./ferric-tracing.md).
- **The Linux kernel's `menuconfig`**: the TUI over a typed feature catalog in `features.toml`.
