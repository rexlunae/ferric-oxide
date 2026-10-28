//! Address space layout randomization (ASLR) test for Ferric Oxide.
//!
//! Runs itself twice as a child process. Each child prints where its stack, a
//! small heap allocation, a large heap allocation (which the allocator maps
//! with mmap) and its own code ended up. The parent compares the two runs:
//!
//! - the stack comes from relibc's exec (`aslr` relibc feature);
//! - heap and mmap addresses come from the kernel's random mmap base (`aslr`
//!   kernel feature);
//! - the code address only moves for position-independent executables, so a
//!   match there is reported but is not a failure.
//!
//! Also reserves and releases 32 GiB of inaccessible virtual address space,
//! without committing data pages. Exec must not make the argument/environment
//! mapping above the randomized stack a hard floor for later mmap calls (#300).
//!
//! Prints the address-space result and `FERRIC_TEST_PASS aslr ...`, `FERRIC_TEST_SKIP aslr
//! ...` (no RDRAND, so the system keeps its fixed layout) or `FERRIC_TEST_FAIL
//! aslr ...`, and exits non-zero on failure.

#![deny(unsafe_op_in_unsafe_fn)]

use std::io;
use std::process::{Command, ExitCode, Output};

/// What one run reports, in this order.
const NAMES: [&str; 4] = ["stack", "heap", "mmap", "code"];

/// Addresses seen by this process.
fn probe() -> [usize; 4] {
    let local = 0_u8;
    let small = Box::new(0_u64);
    // Large enough that the allocator asks the kernel for a fresh mapping.
    let large = vec![0_u8; 4 << 20];
    [
        std::hint::black_box(&local) as *const u8 as usize,
        &*small as *const u64 as usize,
        large.as_ptr() as usize,
        probe as fn() -> [usize; 4] as usize,
    ]
}

#[cfg(target_arch = "x86_64")]
fn has_rdrand() -> bool {
    // CPUID.(EAX=1):ECX bit 30. Older toolchains declare the intrinsic unsafe.
    #[allow(unused_unsafe)]
    let leaf1 = unsafe { std::arch::x86_64::__cpuid(1) };
    leaf1.ecx & (1 << 30) != 0
}

#[cfg(not(target_arch = "x86_64"))]
fn has_rdrand() -> bool {
    false
}

/// Exercise virtual space larger than the entire 16 GiB stack-ASLR range.
fn sparse_address_space() -> io::Result<()> {
    const SIZE: usize = 32 << 30;
    // SAFETY: null requests a fresh reservation. PROT_NONE and no accesses leave
    // its data pages uncommitted; no existing mapping or reference is replaced.
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            SIZE,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if address == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this function owns the complete live reservation and created no references to it.
    if unsafe { libc::munmap(address, SIZE) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A child failed to start, execute, or follow the address-report protocol.
#[derive(Debug)]
enum ChildError {
    Spawn(io::Error),
    Failed(Output),
    InvalidOutput(Output),
}

impl std::fmt::Display for ChildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "could not start child: {error}"),
            Self::Failed(output) | Self::InvalidOutput(output) => write!(
                f,
                "child status {} stdout {:?} stderr {:?}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ),
        }
    }
}

impl std::error::Error for ChildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            _ => None,
        }
    }
}

/// Runs a child and parses its four addresses.
fn run_child(exe: &str) -> Result<[usize; 4], ChildError> {
    let output = Command::new(exe)
        .arg("--child")
        .output()
        .map_err(ChildError::Spawn)?;
    read_child(output)
}

fn read_child(output: Output) -> Result<[usize; 4], ChildError> {
    if !output.status.success() {
        return Err(ChildError::Failed(output));
    }
    let values: Result<Vec<usize>, _> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(|word| usize::from_str_radix(word, 16))
        .collect();
    values
        .map_err(|_| ChildError::InvalidOutput(output.clone()))?
        .try_into()
        .map_err(|_| ChildError::InvalidOutput(output))
}

fn main() -> ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_else(|| "ferric-aslr-test".into());
    if args.next().as_deref() == Some("--child") {
        let addrs = probe();
        println!(
            "{:x} {:x} {:x} {:x}",
            addrs[0], addrs[1], addrs[2], addrs[3]
        );
        return ExitCode::SUCCESS;
    }

    if let Err(error) = sparse_address_space() {
        println!("FERRIC_TEST_FAIL aslr-address-space: {error}");
        return ExitCode::FAILURE;
    }
    println!("FERRIC_TEST_PASS aslr-address-space: sparse 32 GiB reservation");

    if !has_rdrand() {
        println!("FERRIC_TEST_SKIP aslr no RDRAND, layout stays fixed");
        return ExitCode::SUCCESS;
    }

    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or(argv0);
    let runs = match (run_child(&exe), run_child(&exe)) {
        (Ok(a), Ok(b)) => [a, b],
        (Err(e), _) | (_, Err(e)) => {
            println!("FERRIC_TEST_FAIL aslr {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut report = Vec::new();
    let mut same = Vec::new();
    for (i, name) in NAMES.iter().enumerate() {
        report.push(format!("{name}={:#x}/{:#x}", runs[0][i], runs[1][i]));
        if runs[0][i] == runs[1][i] && *name != "code" {
            same.push(*name);
        }
    }
    let report = report.join(" ");
    if same.is_empty() {
        println!("FERRIC_TEST_PASS aslr {report}");
        ExitCode::SUCCESS
    } else {
        println!(
            "FERRIC_TEST_FAIL aslr same {} in both runs: {report}",
            same.join(",")
        );
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn output(status: i32, stdout: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(status),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"loader failure context".to_vec(),
        }
    }

    #[test]
    fn a_failed_child_cannot_pass_with_four_addresses() {
        let error = read_child(output(1 << 8, "10 20 30 40")).unwrap_err();
        assert!(matches!(error, ChildError::Failed(_)));
        assert!(error.to_string().contains("loader failure context"));
    }

    #[test]
    fn reports_require_exactly_four_hexadecimal_addresses() {
        assert_eq!(
            read_child(output(0, "10 20 30 40\n")).unwrap(),
            [16, 32, 48, 64]
        );
        for text in ["10 20 30", "10 20 30 40 50", "error 10 20 30 40"] {
            assert!(matches!(
                read_child(output(0, text)),
                Err(ChildError::InvalidOutput(_))
            ));
        }
    }
}
