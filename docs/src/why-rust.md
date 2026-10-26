# Why Rust?

Why did we write an operating system in Rust? Why even write in Rust?

Rust has enormous advantages, because for operating systems, security and stability matters a lot.

Since operating systems are such an integrated part of computing, they are the most important piece of software.

There have been numerous bugs and security vulnerabilities in Linux, BSDs, glibc, GNU Bash, X11, etc. throughout time, simply due to the lack of memory-safety and type-safety. Rust does this right, by enforcing memory and type safety statically (compile-time verification).

Design does matter, but so does implementation. Rust attempts to avoid these unexpected unsafe memory conditions (which are a major source of security critical bugs). Design is a very transparent source of issues. You know what is going on, you know what was intended and what was not.

The basic design of the kernel/user-space separation is fairly similar to Unix-like systems, at this point. The idea is roughly the same: you separate kernel and user-space, through strict enforcement by the kernel (permission system with small attack surface) and CPU (memory address space isolation), which manages system resources.

However, we have an advantage: enforced memory and type safety. This is Rust's strong side, a large number of "unexpected bugs" (for example, undefined behavior) are eliminated with compile-time verification.

The design of Linux and BSD is secure, the implementation is not. Many bugs in Linux originate in unsafe conditions (which Rust effectively eliminates) like buffer overflows, not the overall design.

Using Rust gives Ferric Oxide, like Redox before it, a head start on a more secure and stable operating system.

## Unsafes

`unsafe` is a way to tell Rust that "I know what I'm doing!", which is often necessary when writing low-level code, providing safe abstractions. You cannot write a kernel without `unsafe`.

In that light, a kernel cannot be 100% verified by the Rust compiler, however the unsafe parts have to be marked with an `unsafe`, which keeps the unsafe parts segregated from the safe code. We seek to eliminate the `unsafe`s where we can, and when we use `unsafe`s, we are extremely careful. Ferric Oxide's CI tracks the kernel's `unsafe` count and lint warnings with a ratchet, so they can go down but not up (see [Validation and Debugging](./ferric-testing.md)).

This contrasts with kernels written in C, which cannot make guarantees about security without costly formal analysis.

You can find out more about how `unsafe` works in the [relevant section](https://doc.rust-lang.org/book/ch19-01-unsafe-rust.html) of the Rust book.

## Benefits

The following sections explain the Rust benefits.

### Less likely to have bugs

The restrictive syntax and compiler requirements to build the code reduce the probability of bugs a lot.

The borrow checker verifies all memory allocation at compile-time which eliminate most memory bugs, like buffer overflows or dangling pointers.

The type system verifies invalid type usage at compile-time and allow the creation of new types to invalidate logic states that aren't allowed, which eliminate some logic bugs and reduce the formal verification effort.

### Less vulnerable to data corruption

The Rust compiler helps the programmer to avoid memory errors and race conditions, which reduces the probability of data corruption bugs.

### Fewer memory defects, with mitigations as a backstop

A microkernel written in Rust avoids most of the memory defects found in C and C++ systems, and isolating system components from the kernel keeps the [attack surface](https://en.wikipedia.org/wiki/Attack_surface) small.

Ferric Oxide still uses conventional exploit mitigations. The kernel contains `unsafe` code, imported C code runs in user space, the compiler can have bugs, and hardware has its own flaws (Spectre, Meltdown). KASLR, user ASLR, guard pages, KPTI, Spectre v1 index hardening and optional CET are cheap insurance against all of these. See [Memory and CPU Protection](./ferric-memory.md).

### Improved security and reliability without significant performance impact

As the kernel is small, it uses less memory to do its work, and its limited code size makes it practical to review and test thoroughly ([KISS](https://en.wikipedia.org/wiki/KISS_principle)).

Rust's abstractions compile to code as fast as hand-written C, so safety doesn't have to cost speed. Combined with a small kernel, that makes the system core reliable, fast and maintainable.

### Thread-safety

The C/C++ support for thread-safety is quite fragile. As such, it is very easy to write a program that looks safe to run across multiple CPU threads, but which introduces subtle bugs or security holes. If one thread accesses a piece of state at the same time that another thread is changing it, the whole program can exhibit some truly confusing and bizarre bugs.

You can see [this example](https://en.wikipedia.org/wiki/Time_of_check_to_time_of_use) of a serious class of security bugs that thread-safety fixes.

In Rust, this kind of bug is easy to avoid: the same type system that prevent us from writing memory mistakes also prevents us from writing dangerous concurrent access patterns

### Rust-written Drivers

Drivers written in Rust reduces the probability of bugs, which increases their stability and security.

### Better code quality with much less effort

The combination of more compiler verification and official testing (`cargo test`), linting (Clippy) and formatting (`cargo fmt`) tooling enabled/installed by default reduces the effort to improve code quality and testing coverage.
