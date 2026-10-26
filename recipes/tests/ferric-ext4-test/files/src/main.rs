//! `ferric-ext4-test`: mounts an ext4 image with `ext4d` and checks it on the running system.
//!
//! Prints one `FERRIC_TEST_PASS ext4-<step>` or `FERRIC_TEST_FAIL ext4-<step>: <why>` line
//! for each step, and exits with an error if any failed:
//!
//! - `ext4-mount`: `ext4d` serves the image at `/scheme/ferric-ext4`;
//! - `ext4-list`: the root and the hashed directory `many/` list what the image holds;
//! - `ext4-read`: files read back whole, from an offset and through a symbolic link, and the
//!   link reads as its target;
//! - `ext4-stat`: sizes and kinds come back from `stat`;
//! - `ext4-erofs`: writing, creating, removing, renaming and making a directory fail with
//!   `EROFS`, and the file is unchanged afterwards;
//! - `ext4-journal`: `ext4d` replays the journal of `journal-v3`, a volume that needs
//!   recovery, serves hello.txt as the journal left it, and the image no longer needs
//!   recovery. When `ext4d --features` does not list `journal-replay`
//!   (fs.ext4_journal_replay off) the step prints `FERRIC_TEST_SKIP ext4-journal`; with it,
//!   ext4d exiting before it mounts (a failed replay) fails the step.
//!
//! Without `/usr/bin/ext4d` (fs.ext4 off) it prints `FERRIC_TEST_SKIP ext4`. The images are
//! `basic.ext4` and `journal-v3.ext4.gz` from `userspace/ext4d/tests/fixtures/`, whose
//! script lists their contents.

#![forbid(unsafe_code)]

use std::process::ExitCode;

#[cfg(target_os = "redox")]
mod redox;

fn main() -> ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        eprintln!("ferric-ext4-test runs on Redox");
        ExitCode::FAILURE
    }
}
