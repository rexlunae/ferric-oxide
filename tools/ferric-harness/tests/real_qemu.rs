//! Opt-in test against a real QEMU and a real image.
//!
//! It boots a smoke image (`config/ferric-smoke.toml`, which runs the target tests, prints
//! `FERRIC_SMOKE_OK` and powers off) headless with `ferric-harness run --smoke`, and checks
//! the verdict. It needs QEMU, UEFI firmware and a built image, so it is ignored by default:
//!
//! ```sh
//! FERRIC_HARNESS_SMOKE_IMAGE=build/x86_64/ferric-smoke/harddrive.img \
//!     cargo test -p ferric-harness --test real_qemu -- --ignored
//! ```
//!
//! Set `FERRIC_HARNESS_SMOKE_IMAGE_TRACES=1` when the image's kernel was built with the
//! `trace` feature: the normal-mode `mode` check must then fail (the kernel prints `trace:`
//! lines) and every other check must pass.

use std::path::PathBuf;
use std::process::Command;

use ferric_harness::report::Report;
use ferric_harness::verdict::{CheckStatus, Outcome};

#[test]
#[ignore = "boots a real image in QEMU; set FERRIC_HARNESS_SMOKE_IMAGE and pass --ignored"]
fn smoke_image_boots_and_passes() {
    let image = PathBuf::from(
        std::env::var_os("FERRIC_HARNESS_SMOKE_IMAGE")
            .expect("set FERRIC_HARNESS_SMOKE_IMAGE to a ferric-smoke harddrive.img"),
    );
    let traces = std::env::var_os("FERRIC_HARNESS_SMOKE_IMAGE_TRACES").is_some_and(|v| v == "1");
    let run = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("ferric-harness-real-qemu-{}", std::process::id()));
    if run.exists() {
        std::fs::remove_dir_all(&run).expect("remove an old run directory");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_ferric-harness"))
        .arg("run")
        .arg(&image)
        .args([
            "--display",
            "none",
            "--smoke",
            "--smp",
            "2",
            "--mem",
            "2048",
        ])
        .args(["--timeout", "600"])
        .arg("--run")
        .arg(&run)
        .output()
        .expect("run ferric-harness");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let report = Report::load(&run).unwrap_or_else(|e| {
        panic!(
            "no report in {}: {e}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            run.display()
        )
    });
    for check in &report.verdict.checks {
        let failed = check.status == CheckStatus::Fail;
        let should_fail = traces && check.name == "mode";
        assert_eq!(
            failed, should_fail,
            "check {} is {:?}: {}\n{stdout}",
            check.name, check.status, check.detail
        );
    }
    if !traces {
        assert_eq!(report.verdict.result, Outcome::Pass, "{stdout}");
        assert!(output.status.success());
    }
    for milestone in ["qemu_start", "kernel", "init", "smoke_ok", "shutdown"] {
        assert!(
            report.milestones.contains_key(milestone),
            "milestone {milestone} missing: {:?}",
            report.milestones
        );
    }
}
