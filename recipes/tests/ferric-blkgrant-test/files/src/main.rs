//! Storage DMA into registered buffers: on-target test and benchmark for Ferric Oxide
//! (`drivers.grant_dma`, io/registered-buffers-design.md in the project files, PR 4).
//!
//! The program registers a buffer in a `kernel.regbuf` region, attaches a read-write grant of
//! it to a slot of a partition descriptor of nvmed's disk scheme (`ferric_blkgrant::abi`), and
//! checks each step:
//!
//! - `nvme-grant-attach`: nvmed maps the grant into its DMA domain (`MapGrant`).
//! - `nvme-grant-read`: blocks read through the grant (the controller writes the buffer by
//!   DMA) equal the same blocks read with an ordinary `pread`, also at an offset in the grant.
//! - `nvme-grant-write`: blocks written through the grant (the controller reads the buffer)
//!   read back with `pread`; the original blocks are put back afterwards. The test writes only
//!   the BIOS boot partition, which a UEFI boot does not use.
//! - `nvme-grant-checks`: the driver refuses a read into a read-only grant (`EACCES`), an
//!   unaligned offset and a range past the grant (`EINVAL`), an empty slot (`ENOENT`) and a
//!   range past the partition (`EOVERFLOW`), without touching the device.
//! - `nvme-grant-revoke`: once the application revokes the grant, reads through it fail with
//!   `ECANCELED` and the buffer keeps what the application put there.
//! - `nvme-grant-detach`: detaching the revoked grant gives its reserved IOVA back.
//!
//! Between `nvme-grant-checks` and the revoke it times sequential reads of 128 KiB and of
//! 4 KiB, through the grant and with `pread` (the bytes copied through the kernel and the
//! driver's bounce buffer), and prints `FERRIC_BENCH nvme-read-...` lines with both.
//!
//! Each step prints `FERRIC_TEST_PASS <name>` or `FERRIC_TEST_FAIL <name>: <why>`; the run
//! stops at the first failure and exits non-zero. Without an NVMe disk, a `kernel.regbuf`
//! scheme, grant support in nvmed (`drivers.grant_dma`) or a DMA domain (no IOMMU), it prints
//! `FERRIC_TEST_SKIP nvme-grant: <why>`, still prints the `pread` benchmark when it can, and
//! exits zero.

#[cfg(target_os = "redox")]
mod redox;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "redox")]
    {
        redox::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        println!("FERRIC_TEST_SKIP nvme-grant: only runs on Redox");
        std::process::ExitCode::SUCCESS
    }
}
