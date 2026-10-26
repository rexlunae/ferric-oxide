//! `ferric-blocklayer-test`: serves LUKS and LVM images with `ferric-blockd` and checks them
//! on the running system.
//!
//! Prints one `FERRIC_TEST_PASS blocklayer-<step>` or `FERRIC_TEST_FAIL blocklayer-<step>:
//! <why>` line for each step, and exits with an error if any failed:
//!
//! - `blocklayer-image`: the images and key files are written to `/tmp`;
//! - `blocklayer-lvm-on-luks`: a logical volume inside LUKS2 with 4096-byte sectors reads
//!   back its pattern, whole and from an offset;
//! - `blocklayer-luks-on-lvm`: LUKS2 with 512-byte sectors inside a logical volume that is
//!   split over two extent runs;
//! - `blocklayer-luks1`: a LUKS1 volume;
//! - `blocklayer-erofs`: opening a volume for writing fails with `EROFS`;
//! - `blocklayer-access`: a process that is not root gets `EACCES`;
//! - `blocklayer-refuse`: with a wrong passphrase the daemon exits without serving.
//!
//! Without `/usr/bin/ferric-blockd` (fs.luks and fs.lvm off) it prints
//! `FERRIC_TEST_SKIP blocklayer`. The images are those of
//! `crates/blocklayer/tests/fixtures/`, whose script says what each holds.

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
        eprintln!("ferric-blocklayer-test runs on Redox");
        ExitCode::FAILURE
    }
}
