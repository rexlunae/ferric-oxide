# Programs and Libraries

- Ferric Oxide is a general-purpose operating system, thus it can run any type of program.

Some programs are interpreted by a runtime for the program's language, such as a script running in the GNU Bash shell or a Python program. Others are compiled into CPU instructions that run on a particular operating system (Ferric Oxide) and specific hardware (e.g. x86 compatible CPU in 64-bit mode).

- In Ferric Oxide, the binaries use the standard [ELF](https://en.wikipedia.org/wiki/Executable_and_Linkable_Format) ("Executable and Linkable Format") format.

Programs could directly invoke system calls, but most call library functions that are higher-level and more easy to use. The program is linked with the libraries it needs.

- Most C/C++ programs call functions in a [C Standard Library](https://en.wikipedia.org/wiki/C_standard_library) (libc) such as `fopen()`
- Ferric Oxide includes a Rust implementation of the C Standard Library called [relibc](https://github.com/rexlunae/ferric-oxide/tree/main/crates/relibc), inherited from Redox. This is how programs such as Git and Python can run on it. relibc has partial [POSIX compatibility](https://en.wikipedia.org/wiki/C_POSIX_library).
- relibc has Linux functions for libraries and programs
- Rust programs implicitly or explicitly call functions in the [Rust Standard Library](https://doc.rust-lang.org/std/).
- The Rust libstd includes an implementation of its system-dependent parts (such as file access and setting environment variables) for the `redox` target (`target_os = "redox"`, a name Ferric Oxide keeps), mostly shared with its Unix code. Most of libstd works, so many Rust programs can be compiled for Ferric Oxide.

The cookbook, in [`recipes/`](https://github.com/rexlunae/ferric-oxide/tree/main/recipes), contains recipes (software ports) for compiling C, C++ and Rust programs. Programs outside the base system are being moved to the [`fox`](./ferric-packaging.md) package manager, which builds Rust packages directly from their git source.

Porting is done case by case. If a program needs only small patches, add `.patch` files to its recipe folder. If it needs large ones, keep a fork with a branch for the port (many recipes still point at the Redox project's forks on its GitLab; Ferric Oxide is moving the ones it maintains to mirrors on GitHub).

- [OS Internals Documentation](https://wiki.osdev.org/How_kernel,_compiler,_and_C_library_work_together)
- [ELF Documentation](https://wiki.osdev.org/ELF)
