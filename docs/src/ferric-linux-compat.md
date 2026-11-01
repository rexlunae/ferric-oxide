# Linux Userspace Compatibility (planned)

**This is an agreed design requirement, not an implemented runtime.** Ferric
Oxide currently runs native Redox-target applications through relibc and its
Rust services. There is no Linux syscall personality, Linux executable launch
path or Linux-compatible `/proc` and `/sys` environment in the image. Relibc's
existing Linux backend runs relibc on Linux; it does not run Linux binaries on
Ferric. Implementation and validation are tracked by
[Phase L](./ferric-roadmap.md#phase-l-linux-userspace-compatibility).

## Scope

The goal is to run unmodified, same-architecture Linux user programs as
Ferric-managed processes. The compatibility contract consists primarily of
Linux system calls and the filesystem and device interfaces those programs
observe. Executable loading, the initial stack, auxiliary vector, thread-local
storage and the requested dynamic interpreter must also support that contract.
Start with an explicit x86_64 compatibility profile; additional architecture
profiles need their own ABI definitions and tests.

Compatibility is a documented subset that grows with tested application needs.
Each profile must name its supported syscalls, flags, pseudo-files, device
interfaces, required libraries and known limitations. Unsupported operations
return the appropriate Linux error. Empty placeholder files or successful
no-op syscalls do not establish support.

## Shared Rust implementation with relibc

Both execution paths must use Ferric's Rust base services and share reusable
runtime and platform mechanisms with relibc:

- Native C/POSIX calls enter through relibc's C interface and its Rust backend;
  native Rust consumers should use the shared Rust interfaces directly.
- Linux applications enter through a Linux ABI adapter, including interception
  of direct syscalls, and reach the same shared Rust mechanisms and services.

The adapter translates syscall conventions, structures, flags and error
numbers and preserves Linux-specific semantics. Sharing mechanisms does not
make every Linux operation equivalent to a native POSIX call. Behavior that
differs needs an explicit implementation at the compatibility boundary.

Relibc's platform abstraction layer and `redox-rt` process, thread and signal
machinery are starting points for reuse. Extract context-independent or
explicitly context-aware Rust components under `crates/` when they are needed
across workspaces. Keep the existing workspace boundaries. This extraction
remains future work; the current runtime is not a ready-made Linux adapter.

Operations performed by a compatibility service must identify the target
process and thread, their address space, descriptor table, credentials and
namespace. Calling the service's ordinary C wrappers could otherwise act on
its own process state. Linux-process pointers must be checked through the
target's memory access mechanisms, and compatibility errors stay typed until
the ABI boundary.

Linux programs may retain their expected glibc or musl and dynamic interpreter,
or contain a statically linked runtime. Their OS requests still reach Ferric's
shared Rust implementation. Replacing every Linux libc with relibc is a
separate library-ABI project and is not required for this syscall-based path.
The [Linux executable-loading contract](https://man7.org/linux/man-pages/man2/execve.2.html)
includes the interpreter named by an ELF executable's `PT_INTERP` segment.

## Filesystem and device views

Provide Linux-facing views through Rust services over native Ferric resources
and compatibility process state. Path resolution must select these views in
the Linux environment's namespace. Native scheme names and interfaces remain
available to native software.

Prioritize interfaces used by the declared application profile:

- **Process information:** `/proc/self`, `/proc/thread-self`, process and thread
  directories, `status`, `cmdline`, `maps`, `fd`, `exe` and `cwd` as needed.
- **System information:** supported views such as `/proc/cpuinfo`,
  `/proc/meminfo`, uptime and mount information, with documented mappings to
  Ferric's accounting and resource model.
- **Devices:** selected `/sys` discovery paths and `/dev` endpoints, including
  terminals, shared memory and graphics devices as their backends become
  available. Device compatibility includes the relevant `ioctl`, mapping,
  polling and synchronization behavior.

Linux's [procfs](https://man7.org/linux/man-pages/man5/proc.5.html) and
[sysfs](https://man7.org/linux/man-pages/man5/sysfs.5.html) documentation define
observable filesystem interfaces. Implement only the supported views and
controls, backed by real state. Their metadata, links, directory iteration,
partial reads, writes, permissions and lifetime behavior are part of the
contract.

Syscalls and filesystem views must agree: a process ID returned by a syscall
identifies the same task under `/proc`; descriptor links reflect its descriptor
table; memory maps describe its mappings. Visibility and controls must respect
the compatibility namespace and the authority granted by Ferric. A Linux-facing
credential or path must not grant access beyond that authority.

## Graphics and other execution environments

Linux Wayland clients should connect to the native Wayland compositor, and
Linux X11 clients should connect through Xwayland. This requires compatible
socket, descriptor-passing and shared-memory behavior. Accelerated clients
also need the DRM device interfaces, buffer sharing, formats and fences used
by their selected Mesa driver. Track that coverage with
[the graphics design](./graphics-windowing.md#planned-native-graphics-stack).
Display-protocol support alone does not establish binary compatibility.

The planned [microVM environments](./ferric-virt.md) provide a complementary
execution path with a guest operating system. Running a program inside a Linux
guest does not validate this native compatibility layer. Neither execution
path is implemented today. Native Linux binaries also remain a distinct
execution profile from the planned [WebAssembly userland](./ferric-wasm.md).

Linux kernel-driver reuse is a separate compatibility project. A
[LinuxKPI-style interface](https://man.freebsd.org/cgi/man.cgi?query=linuxkpi&sektion=4)
can support recompiling selected driver sources against a driver runtime.
For Ferric, that runtime would need native resource adapters and compatible
allocation, locking, memory ordering, interrupt, work-queue, timer, firmware
and DMA semantics. GPU drivers additionally need their subsystem services.
Target a documented driver/API version; Linux does not promise a stable
[internal driver interface](https://docs.kernel.org/process/stable-api-nonsense.html).

The choice among native Rust drivers, compatibility-hosted drivers and Linux
driver VMs remains open for each device family. All should meet the same
native device-service contracts, including the graphics buffer and display
interfaces. No LinuxKPI runtime or driver VM is implemented. Any reuse must
meet the repository's licensing rules; a shim does not change a driver's
license. This design does not authorize integrating GPL-only kernel code.

## Configuration and validation

Implement optional Linux compatibility, Xwayland integration and driver-hosting
choices through `features.toml` and `ferric-config` as they land. There are no
new configuration keys or enabled defaults in this design. Keep native boot
and recovery independent of the Linux compatibility runtime.

Implement from published ABI documentation and specifications under the
repository's clean-room policy. Record the references in the implementing
modules. [Starnix](https://fuchsia.dev/fuchsia-src/concepts/starnix/kernel)
is architectural prior art for a Rust userspace Linux implementation and for
running the same behavioral tests on Linux and a compatibility layer; it is
not a component currently integrated into Ferric.

Validation must include:

- Host tests for ABI layouts, checked conversions, flags, errors, parsing and
  partial filesystem operations.
- The same Linux-built behavioral tests run on Linux and Ferric, covering
  process/thread lifecycle, signals, futexes, mappings, polling, sockets,
  descriptor sharing and consistency with `/proc` and `/sys`.
- Unmodified static and dynamically linked CLI programs, with all required
  libraries and interpreters recorded, followed by Linux Wayland and X11
  clients sharing the native desktop.
- Failure, teardown and authority-boundary tests, plus syscall, I/O and
  graphics latency measurements against the corresponding native paths.

Publish a supported profile and its application results before claiming Linux
binary compatibility. Passing a small CLI example does not establish support
for every syscall, Linux distribution, desktop or container environment.
