//! `ferric-fat-test`: mounts a FAT32 image with `fatd` and checks it on the running system.
//!
//! Prints one `FERRIC_TEST_PASS fat-<step>` or `FERRIC_TEST_FAIL fat-<step>: <why>` line for
//! each step, and exits with an error if any failed:
//!
//! - `fat-image`: the fixture image is unpacked and written to `/tmp`;
//! - `fat-mount`: `fatd` serves the image at `/scheme/ferric-fat`;
//! - `fat-list`: the root and `many/`, a directory over several clusters, list what the
//!   image holds, long names included;
//! - `fat-read`: files read back whole and from an offset, by their long names, their 8.3
//!   short names and in another case;
//! - `fat-stat`: sizes, kinds and modification times come back from `stat`;
//! - `fat-erofs`: writing, creating, removing, renaming and making a directory fail with
//!   `EROFS`, and the file is unchanged afterwards.
//!
//! Without `/usr/bin/fatd` (fs.fat off) it prints `FERRIC_TEST_SKIP fat`. The image is
//! `fat32.img.gz` from `userspace/fatd/tests/fixtures/`, whose script lists its contents.

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
        eprintln!("ferric-fat-test runs on Redox");
        ExitCode::FAILURE
    }
}
