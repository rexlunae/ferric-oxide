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
//! Prints one result line, `FERRIC_TEST_PASS aslr ...`, `FERRIC_TEST_SKIP aslr
//! ...` (no RDRAND, so the system keeps its fixed layout) or `FERRIC_TEST_FAIL
//! aslr ...`, and exits non-zero on failure.

use std::process::{Command, ExitCode};

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

fn has_rdrand() -> bool {
    // CPUID.(EAX=1):ECX bit 30. Older toolchains declare the intrinsic unsafe.
    #[allow(unused_unsafe)]
    let leaf1 = unsafe { std::arch::x86_64::__cpuid(1) };
    leaf1.ecx & (1 << 30) != 0
}

/// Runs a child and parses its four addresses.
fn run_child(exe: &str) -> Result<[usize; 4], String> {
    let output = Command::new(exe)
        .arg("--child")
        .output()
        .map_err(|e| format!("could not run {exe}: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let values: Vec<usize> = text
        .split_whitespace()
        .filter_map(|word| usize::from_str_radix(word, 16).ok())
        .collect();
    values
        .try_into()
        .map_err(|_| format!("child printed {text:?}, not four addresses"))
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
