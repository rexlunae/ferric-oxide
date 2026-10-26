# Microkernels

The Ferric Oxide kernel, like the Redox kernel it comes from, is a microkernel. Microkernels stand out in their design by providing minimal abstractions in kernel-space. Microkernels focus on user-space, unlike Monolithic kernels which focus on kernel-space.

The basic philosophy of microkernels is that any component which *can* run in user-space *should* run in user-space. Kernel-space should only be utilized for the most essential components (e.g., system calls, process separation, resource management, IPC, thread management, etc).

The kernel's main task is to act as a medium for communication and segregation of processes. The kernel should provide minimal abstraction over the hardware (that is, drivers, which can and should run in user-space).

Microkernels are more secure and less prone to crashes and driver bugs than monolithic kernels. This is because most kernel components are moved to user-space and use different memory address spaces, and thus can't do damage to the system. Furthermore, microkernels are extremely maintainable, due to their small code size the number of bugs in the kernel is reduced a lot.

As with anything else, microkernels do also have disadvantages.

## Advantages of microkernels

There are quite a lot of advantages (and some disadvantages) with microkernels, a few of which will be covered here.

### Better Stability

When compared to microkernels, monolithic kernels tend to be prone to crashes and driver bugs. A buggy driver in a monolithic kernel can crash the whole system because the driver code is running on the same memory address space of the kernel, thus the kernel process can't continue to run (to avoid memory corruption) and crashes (kernel panic). Or the bugs in a driver can spread to other drivers without causing crashes, which makes it hard to ensure stability and also makes it a headache to investigate the root cause of the problems.

While in a microkernel the drivers run in different memory address spaces (separation of concerns) which allows the system to handle any crash safely and isolate driver bugs.

In Linux we often see errors with drivers dereferencing bad pointers which ultimately results in kernel panics.

There is very good documentation in the [MINIX documentation](http://wiki.minix3.org/doku.php?id=www:documentation:reliability) about how this can be addressed by a microkernel.

### Better Security

Microkernels are undoubtedly more secure than monolithic kernels. The minimality principle of microkernels is a direct consequence of the Principle Of Least Privilege, according to which all components should have only the privileges absolutely needed to provide the needed functionality.

Many security-critical bugs in monolithic kernels come from services and drivers running unrestricted in kernel mode, without any form of protection.

In other words: **in monolithic kernels, drivers can do whatever they want, without restrictions, when running in kernel mode**.

### Better Modularity and Configuration

Monolithic kernels are, well, monolithic. They do not allow fine-grained control like microkernels. This is due to many essential components being "hard-coded" into the kernel, and thus requiring modifications to the kernel itself (e.g., device drivers).

Microkernels are very modular by nature. You can add, replace, reload, modify, change, and remove system components or drivers, on runtime, without even touching the kernel.

Modern monolithic kernels try to solve this issue using kernel modules but still often require the system to reboot.

### Better Expansion

In microkernels new system components and drivers can be easily added using daemons, while in monolithic kernels the entire kernel needs to be updated to add them which can introduce bugs to other system components or drivers.

### Less Maintenance Cost

In monolithic kernels many subsystems and drivers need to be updated after API changes, which increase the effort to improve and maintain the system if it has many subsystems or drivers and increases.

While in microkernels the subsystems and drivers can use different protocol versions to keep backwards compatibility on API/ABI changes.

### Better and Cheaper Debugging On Real Hardware

In microkernels most kernel components (drivers, filesystems, etc) are moved to user-space, thus bugs on them can't crash the kernel (kernel panic).

The "system component panics" (where each system component has a separated panic log) are more common in microkernels than kernel panics because the kernel code size is very small. The log of a component panic can be saved without expensive dedicated hardware, which ease debugging.

This is very important to debug in real hardware, because if a kernel panic happens the log can't be easily saved to help the search of the cause of the bug.

While in monolithic kernels a bug in a system component or driver can cause a kernel panic and freeze the system (if it happens in real hardware you can't debug without serial output support, which is expensive)

(Buggy drivers are the main cause of kernel panics)

## Disadvantages of microkernels

### Small Performance Overhead

Any modern operating system needs basic security mechanisms such as memory isolation and virtualization. Furthermore any process (including the kernel) has its own stack and variables stored in registers. On [context switch](https://en.wikipedia.org/wiki/Context_switch), that is each time a system call is invoked or any other inter-process communication (IPC) is done, some tasks have to be done, including:

- Saving caller registers, especially the program counter (caller: process invoking syscall or IPC)
- Reprogramming the [MMU](https://en.wikipedia.org/wiki/Memory_management_unit)'s page table (aka [TLB](https://en.wikipedia.org/wiki/Translation_lookaside_buffer))
- Putting CPU in another mode (kernel mode and user mode, also known as ring 0 and ring 3)
- Restoring callee registers (callee: process invoked by syscall or IPC)

These are not inherently slower on microkernels, but microkernels need to perform these operations more frequently. Much of the system functionality is performed by user-space processes, requiring additional context switches.

The performance difference between monolithic and microkernels has shrunk over time, making their performance comparable. This is partly due to a smaller surface area which can be easier to optimize.

- [Context Switch Documentation](https://wiki.osdev.org/Context_Switching)
- [Microkernels Performance Paper](https://os.inf.tu-dresden.de/pubs/sosp97/)

Ferric Oxide's I/O work aims at this overhead (see
[Registered buffers, IOMMUs and rings](./ferric-io.md)):

- **Direct switches.** A request to a user-space scheme switches straight to
  the daemon serving it and back, without a trip through the run queue.
- **Rings.** Shared submission and completion queues let a program submit many
  operations with one system call, or none while the kernel is already
  working through the queue.
- **Registered buffers and DMA grants.** Data can move between a program, a
  service and a device without being copied at each step; file descriptors
  still control access, and only the bytes skip the copies.
- **PCID.** Tagged TLB entries mean a switch between address spaces does not
  have to flush the TLB.
- **A kernel-colocated driver tier** (planned) would let a few hot drivers run
  in the kernel's address space without kernel privileges, isolated by Rust's
  type system, protection keys and the IOMMU. See
  [Drivers](./drivers.md).

## Versus monolithic kernels

Monolithic kernels provide a lot more abstractions than microkernels.

![An illustration](https://upload.wikimedia.org/wikipedia/commons/6/67/OS-structure.svg)

The above illustration from [Wikimedia](https://commons.wikimedia.org/wiki/File:OS-structure.svg), by Wooptoo, License: Public domain) shows how they differ.

## Documentation about the kernel/user-space separation

- [Dual Mode Operations in OS](https://www.geeksforgeeks.org/dual-mode-operations-os/)
- [User Mode and Kernel Mode Switching](https://www.geeksforgeeks.org/user-mode-and-kernel-mode-switching/)

## Documentation about microkernels

- [OSDev Technical Wiki](https://wiki.osdev.org/Microkernel)
- [Message Passing Documentation](https://wiki.osdev.org/Message_Passing)
- [Minix Documentation](https://wiki.minix3.org/doku.php?id=www:documentation:start)
- [Minix Features](https://wiki.minix3.org/doku.php?id=www:documentation:features)
- [Minix Reliability](https://wiki.minix3.org/doku.php?id=www:documentation:reliability)
- [GNU Hurd Documentation](https://www.gnu.org/software/hurd/hurd/documentation.html)
- [Fuchsia Documentation](https://fuchsia.dev/fuchsia-src/get-started/learn/intro)
- [HelenOS FAQ](http://www.helenos.org/wiki/FAQ)
- [Minix Paper](http://www.minix3.org/docs/jorrit-herder/osr-jul06.pdf)
- [seL4 Whitepaper](https://sel4.systems/About/seL4-whitepaper.pdf)
- [Tanenbaum-Torvalds Debate](https://en.wikipedia.org/wiki/Tanenbaum%E2%80%93Torvalds_debate)

## A Note On The Current State

The kernel in `crates/kernel/src/` has roughly 47,000 non-blank, non-comment
lines of Rust (October 2026), across all architectures; Ferric Oxide's
additions (IOMMU, rings, page cache, tracing) account for part of that growth.
For comparison, MINIX has around 6,000 lines of C in its kernel. Line counts
are only a rough guide, since the two kernels do different amounts of work.

Ferric Oxide keeps new kernel code behind feature options, puts portable logic
in host-tested `no_std` crates under `crates/`, and moves work to user space
where performance allows.
