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
//!   `EROFS`, and the file is unchanged afterwards.
//!
//! Without `/usr/bin/ext4d` (fs.ext4 off) it prints `FERRIC_TEST_SKIP ext4`. The image is
//! `basic.ext4` from `userspace/ext4d/tests/fixtures/`, whose script lists its contents.

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
