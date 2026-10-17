//! Verify the host installer CLI against the bundled upstream image fixture.
#![cfg(feature = "installer")]

use std::{path::Path, process::Command};

#[test]
fn bundled_image_configuration_reports_size_and_packages() {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/test.toml");
    let size = Command::new(env!("CARGO_BIN_EXE_redox_installer"))
        .arg(format!("--config={}", config.display()))
        .arg("--filesystem-size")
        .output()
        .unwrap();
    assert!(size.status.success(), "{:?}", size.stderr);
    assert_eq!(String::from_utf8(size.stdout).unwrap().trim(), "196");

    let packages = Command::new(env!("CARGO_BIN_EXE_redox_installer"))
        .arg(format!("--config={}", config.display()))
        .arg("--list-packages")
        .output()
        .unwrap();
    assert!(packages.status.success(), "{:?}", packages.stderr);
    let packages = String::from_utf8(packages.stdout).unwrap();
    assert!(packages.lines().any(|line| line == "kernel"));
    assert!(packages.lines().any(|line| line == "bootloader"));
}
