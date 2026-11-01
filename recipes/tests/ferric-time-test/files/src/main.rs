//! Native time-scheme ownership, close/admission races and expiry regressions.
//!
//! Distinctive far-future deadlines identify this test's resources while driver
//! and network timers remain active. Due timers must deliver actual events.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP time: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
