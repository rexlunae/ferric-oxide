# System Design

This chapter describes how Ferric Oxide is put together: a small kernel, system
services in user space, and communication through schemes and file
descriptors. The design comes from Redox OS. Ferric Oxide keeps that
microkernel model and is changing the parts that limit performance and
security, mainly the I/O path, the scheduler and the permission model.

The pages in this chapter explain the general model. For what this
repository implements today, and what is only planned, read these alongside
it:

- [Architecture and component boundaries](./ferric-architecture.md), for where
  each part lives in the source tree and how the system boots.
- [Memory and CPU protection](./ferric-memory.md), for KASLR, KPTI, CET and the
  CPU baseline.
- [Registered buffers, IOMMUs and rings](./ferric-io.md), for the new I/O
  interfaces.
- [Kernel tracing](./ferric-tracing.md), for observing the kernel at run time.
- [Roadmap](./ferric-roadmap.md), for planned changes.
