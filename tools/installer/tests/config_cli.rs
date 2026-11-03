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

#[test]
fn config_option_accepts_every_value_form() {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/test.toml");
    let config = config.to_str().unwrap();
    for args in [
        vec!["--config", config],
        vec![&format!("--config={config}")],
        vec!["-c", config],
    ] {
        let size = Command::new(env!("CARGO_BIN_EXE_redox_installer"))
            .args(&args)
            .arg("--filesystem-size")
            .output()
            .unwrap();
        assert!(size.status.success(), "{args:?}: {:?}", size.stderr);
        assert_eq!(String::from_utf8(size.stdout).unwrap().trim(), "196");
    }
}

#[test]
fn unknown_option_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_redox_installer"))
        .args(["--no-such-option", "--list-packages"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--no-such-option"));
}
