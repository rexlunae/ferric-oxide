# Memory Management

This page gives an overview of how the kernel manages memory and where to
find the details.

## Physical memory and page tables

The bootloader passes the firmware's memory map to the kernel. The kernel's
frame allocator and page-table code live in `crates/kernel/src/memory/` and
the kernel's `rmm/` crate. All of physical memory is mapped into the kernel's
half of the address space (the *physical map*), which the kernel uses to reach
any frame. With `kernel.kaslr` (on by default) that map starts at a randomized
1 GiB-aligned address, and with `kernel.kaslr_image` the kernel image itself is
loaded at a randomized address.

## Address spaces

Each process has an address space made of *grants*: ranges of pages with a
source (anonymous memory, a file or another process's memory, physical memory
for drivers) and permissions. The code is in `crates/kernel/src/context/`. It
handles `mmap`, `mprotect` and `munmap`, sharing between processes, and
copy-on-write after `fork`. Anonymous memory is usually backed lazily: a page
gets a frame on the first access that faults on it.

User programs get address-space layout randomization (`kernel.aslr`), guard
pages below thread and main stacks, and `PROT_NONE` pages that really fault
(`kernel.prot_none`). The kernel checks that every range a program passes stays
inside user space.

On x86-64, `kernel.cpu.kpti` separates the kernel's page tables from the
user's, against Meltdown-style attacks, and PCIDs keep switching between them
from flushing the TLB.

## Memory shared with the kernel and devices

Ferric Oxide adds interfaces for moving data without copies:

- **Registered buffers** (`kernel.regbuf`) let a program register memory once
  and grant byte ranges of it to a service or a device. A device reaches a
  grant through its IOMMU domain, so DMA can land directly in the program's
  buffer.
- **Rings** (`kernel.ring`) are shared submission and completion queues, so a
  program can submit many operations without a system call each.
- The **page cache** (`kernel.page_cache`) keeps file contents in kernel
  objects that file systems fill, and serves reads from them.

Frames that are registered or mapped for DMA keep a reference until every
mapping of them, including IOMMU mappings, is safely removed.

## Details

- [Memory and CPU protection](./ferric-memory.md): KASLR, ASLR, guard pages,
  user-range checks, KPTI and PCID, CET and speculation hardening.
- [Architecture: execution and memory ownership](./ferric-architecture.md#execution-and-memory-ownership)
- [Registered buffers, IOMMUs and rings](./ferric-io.md)
- [Kernel tracing](./ferric-tracing.md): the `mm.*` tracepoints record page
  faults, mapping operations and a journal of address-space changes.
- [Memory management on the OSDev wiki](https://wiki.osdev.org/Memory_management)
