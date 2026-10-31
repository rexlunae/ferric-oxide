//! Checks authorization of the x86 port-I/O request through `kernel.proc`.
//!
//! Separate children request access as root, as user 1000, and after opening
//! the handle as root then dropping only the effective UID. No child executes
//! port-I/O instructions. The test checks authorization, not device behavior.

#![deny(missing_docs, unsafe_op_in_unsafe_fn)]

#[cfg(all(target_os = "redox", target_arch = "x86_64"))]
mod redox;

#[cfg(all(target_os = "redox", target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    redox::main()
}

#[cfg(not(all(target_os = "redox", target_arch = "x86_64")))]
fn main() {
    println!("FERRIC_TEST_SKIP ioport: requires x86_64 Redox");
}
