//! A process opens a file on a scheme it created on the same thread: on-target regression
//! test for Ferric Oxide.
//!
//! The main thread creates and registers a scheme, a second thread serves it, and the main
//! thread opens and reads a file on it. A request to a user scheme direct-switches the caller
//! to the context that created the scheme, which here is the caller itself; the kernel used to
//! return from that switch at once, so the caller spun in the kernel without blocking and the
//! serving thread never ran on its CPU (the boot then timed out with that CPU stuck). It
//! prints `FERRIC_TEST_PASS scheme-self-open` when the open and the read return what the
//! scheme serves, `FERRIC_TEST_FAIL scheme-self-open: <why>` otherwise.

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP scheme-self-open: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
