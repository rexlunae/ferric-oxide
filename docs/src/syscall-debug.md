# System Call Tracing

To see which system calls a program makes, to investigate behavior, bugs or
performance, use [kernel tracing](./ferric-tracing.md). It records system
calls, scheme requests, context switches, page faults and more into per-CPU
rings, which you can turn on and off while the system runs and read on the
host as text or as a Perfetto timeline. It is available on x86-64.

An older mechanism inherited from Redox, the kernel's `syscall_debug` feature,
is described at the end of this page. It prints to the console and needs a
kernel change for each program you want to watch.

## Tracing system calls

1. Turn tracing on in `ferric.toml` and rebuild the image:

   ```sh
   cargo run -q -p ferric-config -- set kernel.trace=true
   cargo xtask build
   ```

   `kernel.trace_syscall` and `kernel.trace_scheme` are on by default once
   `kernel.trace` is, so `syscall.enter`/`syscall.exit` and the matching scheme
   requests are compiled in. Include the `ferric-trace` package in the image to
   capture on the target (the `ferric-smoke` image has it).

2. Boot the image (`cargo xtask run`) and, as root, record a workload:

   ```sh
   echo "enable syscall scheme sched" > /scheme/kernel.trace/events
   my-program
   echo "disable syscall scheme sched" > /scheme/kernel.trace/events
   ferric-trace capture capture/
   ```

   To keep only some calls, set a filter first, for example
   `echo "filter syscall.enter nr == 3" > /scheme/kernel.trace/events`.
   [Argument filters](./ferric-tracing.md#argument-filters) lists the
   syntax.

3. Copy `capture/` to the host and decode it:

   ```sh
   cargo run -p ferric-trace-cli -- text capture/
   cargo run -p ferric-trace-cli -- perfetto --tsc-mhz 3000 -o trace.json capture/
   ```

   Open `trace.json` in [Perfetto](https://ui.perfetto.dev/). With `sched`
   events recorded, the timeline shows which process made each call.

To trace a boot, put the events in `kernel.trace_boot` instead (for example
`cargo run -q -p ferric-config -- set kernel.trace_boot=syscall`), or use the
`kernel.trace_initfs_*` options to trace from just before `init` starts.
[Kernel tracing](./ferric-tracing.md) covers every event, the overhead
budget, crash dumps and sampling profiles.

## The `syscall_debug` feature

This prints system calls to the kernel console as they happen. It works on
every architecture but needs a kernel rebuild, and you choose the program by
editing kernel code. Don't commit these changes.

### Enable the feature

Turn on the `kernel.syscall_debug` option, which builds the kernel with its
`syscall_debug` Cargo feature:

```sh
cargo run -q -p ferric-config -- set kernel.syscall_debug=true
```

(Without a `ferric.toml`, the build uses the kernel's Cargo defaults instead;
then uncomment `"syscall_debug"` in the default features of
`crates/kernel/Cargo.toml`.)

### Choose what to print

The file `crates/kernel/src/syscall/debug.rs` contains the code to print out the system calls that match a particular set of conditions.

In the function `debug_start`, the boolean `do_debug` determines if the system calls should be printed.

It starts with `let do_debug = if false && { ... }`, where the block reads the current context's name.

Obviously, the condition `false && whatever` will evaluate to false, so remove the first bit (it's to prevent accidentally turning tracing on)

The program name is read from the context, and compared with the string you specify. The name from the context normally has the full path, so we just use the `contains({name})` test.

But if your program is called "ls" for example, you will get a system call trace for any program that contains the letters "ls", so you could try something like `ends_with("/ls")`.

You can modify the boolean expression however you want, assuming you are not publishing the code. You will need something a little fancier if you want messages for more than one program, for example.

If you want to hold onto the lock for the context a little longer, you will have to rework the expression a bit.

Next, there are some system calls that we skip because they are very frequent and not usually interesting. But if you want to get that level of detail, feel free to modify which system calls are filtered, `gettime`, `yield` and `futex` are typically ignored.

Also, writes to file descriptors 1 and 2 (stdout and stderr) are typically not reported so it doesn't interfere with output and debug code from your program as much.

A message will be printed at the start of the system call, and a message will be printed when the system call completes.

(A flag is set so the kernel knows to print the result.)

### Build and run

Rebuild and boot the image:

```sh
cargo xtask run
```

The kernel prints the messages on its console. With `cargo xtask run
--headless` they appear in your terminal; to keep them, run
`cargo xtask run --headless |& tee my_log.txt`. On real hardware, use a serial
console or the `server` configuration and run commands from the console.
