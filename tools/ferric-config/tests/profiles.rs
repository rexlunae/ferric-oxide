//! Exercise CI image selection through the CLI, Makefile and installer includes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::copy(repo.join("features.toml"), dir.path().join("features.toml")).unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        for name in [
            "desktop",
            "base",
            "minimal",
            "native",
            "ferric-smoke",
            "ferric-fuzz",
            "ferric-hardware",
        ] {
            std::fs::copy(
                repo.join("config").join(format!("{name}.toml")),
                dir.path().join("config").join(format!("{name}.toml")),
            )
            .unwrap();
        }
        Self { dir }
    }

    fn output(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ferric-config"))
            .args(["--root"])
            .arg(self.dir.path())
            .arg("--config")
            .arg(self.dir.path().join("ferric.toml"))
            .args(args)
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn generated(&self, name: &str) -> PathBuf {
        self.dir.path().join("build/ferric").join(name)
    }

    fn make_values(&self, makefile: &Path) -> Vec<String> {
        let mut command = Command::new("make");
        command
            .current_dir(self.dir.path())
            .args(["--no-print-directory", "-f"])
            .arg(makefile)
            .args(["-f", "-", "probe"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in [
            "ARCH",
            "BOARD",
            "CONFIG_NAME",
            "FILESYSTEM_CONFIG",
            "MAKEFLAGS",
            "FERRIC_CONFIG_ACTIVE",
            "FERRIC_KERNEL_FEATURES",
            "FERRIC_KERNEL_CONFIG",
            "FERRIC_TARGET_RUSTFLAGS",
        ] {
            command.env_remove(name);
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(
            b".PHONY: probe\nprobe:\n\t@printf '%s\\n' '$(ARCH)' '$(CONFIG_NAME)' '$(FILESYSTEM_CONFIG)' '$(FERRIC_KERNEL_FEATURES)' '$(FERRIC_KERNEL_CONFIG)' '$(FERRIC_TARGET_RUSTFLAGS)' '$(FERRIC_CONFIG_ACTIVE)'\n",
        ).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn assert_profile(profile: &str) {
    let fixture = Fixture::new();
    fixture.run(&[
        "set",
        "target.arch=x86_64",
        &format!("image.config={profile}"),
        "kernel.regbuf=true",
        "kernel.iommu=auto",
        "kernel.cpu.kpti=always",
        "image.filesystem_size=512",
    ]);
    assert_eq!(
        fixture.run(&["get", "image.config"]).trim(),
        format!("\"{profile}\"")
    );
    fixture.run(&["check"]);
    let schema: serde_json::Value = serde_json::from_str(&fixture.run(&["schema"])).unwrap();
    let option = schema
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "image.config")
        .unwrap();
    assert!(
        option["choices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == profile)
    );
    fixture.run(&["generate"]);

    let values = fixture.make_values(&fixture.generated("config.mk"));
    assert_eq!(values.len(), 7, "{values:?}");
    assert_eq!(values[0], "x86_64");
    assert_eq!(values[1], profile);
    assert_eq!(values[2], "build/ferric/filesystem.toml");
    assert!(values[3].split(',').any(|feature| feature == "regbuf"));
    assert!(values[5].contains("target-cpu=x86-64-v3"));
    assert_eq!(values[6], "1");
    let kernel: toml::Value =
        toml::from_str(&std::fs::read_to_string(&values[4]).unwrap()).unwrap();
    assert_eq!(kernel["arch"]["x86_64"]["iommu"].as_str(), Some("auto"));
    assert_eq!(
        kernel["arch"]["x86_64"]["features"]["kpti"].as_str(),
        Some("always")
    );
    let filesystem: toml::Value =
        toml::from_str(&std::fs::read_to_string(fixture.generated("filesystem.toml")).unwrap())
            .unwrap();
    let include = filesystem["include"][0].as_str().unwrap();
    assert_eq!(
        filesystem["general"]["filesystem_size"].as_integer(),
        Some(512)
    );
    assert_eq!(include, format!("../../config/{profile}.toml"));
    assert!(
        fixture
            .generated("filesystem.toml")
            .parent()
            .unwrap()
            .join(include)
            .is_file()
    );

    // Follow the path consumed by Make into the installer's real include loader.
    let resolved =
        redox_installer::Config::from_file(&fixture.dir.path().join(&values[2])).unwrap();
    assert!(resolved.include.is_empty());
    assert_eq!(resolved.general.filesystem_size, Some(512));
    assert_profile_packages(profile, &resolved);
    assert_profile_scripts(profile, &resolved);
}

fn assert_profile_packages(profile: &str, resolved: &redox_installer::Config) {
    for package in [
        "kernel",
        "relibc",
        "bootloader",
        "redoxfs",
        "base",
        "ferric-xstate-test",
        "ferric-aslr-test",
        "ferric-guard-test",
        "ferric-tlb-test",
        "ferric-futex-test",
        "ferric-syscall-fuzz",
        "ferric-iommu-test",
        "ferric-regbuf-test",
    ] {
        assert!(
            matches!(
                resolved.packages.get(package),
                Some(redox_installer::PackageConfig::Build(rule)) if rule == "source"
            ),
            "{profile}: missing source package {package}"
        );
    }
    // These packages and files come through minimal.toml and base.toml.
    assert!(resolved.packages.contains_key("ion"));
    assert!(resolved.packages.contains_key("libgcc"));
    assert!(
        resolved
            .files
            .iter()
            .any(|file| file.path == "/etc/hostname")
    );
}

fn assert_profile_scripts(profile: &str, resolved: &redox_installer::Config) {
    let last_file = |path: &str| {
        let file = resolved
            .files
            .iter()
            .rev()
            .find(|file| file.path == path)
            .unwrap();
        assert!(!file.append && !file.symlink && !file.directory);
        file.data.as_str()
    };
    assert!(last_file("/usr/lib/init.d/30_console").contains("ion /usr/lib/ferric-smoke.ion"));
    let script = last_file("/usr/lib/ferric-smoke.ion");
    assert!(script.contains("echo FERRIC_SMOKE_BEGIN"));
    assert!(script.contains("echo FERRIC_SMOKE_OK"));
    assert!(script.contains("shutdown"));
    if profile == "ferric-fuzz" {
        assert!(
            script.contains(
                "ferric-syscall-fuzz --seed random --seconds 1200 --procs 8 --calls 20000"
            )
        );
        assert!(!script.contains("ferric-xstate-test"));
        assert!(!script.contains("--seed 1 --seconds 15"));
    } else {
        assert!(script.contains("ferric-xstate-test ||"));
        assert!(script.contains("ferric-regbuf-test ||"));
        assert!(script.contains("ferric-syscall-fuzz --seed 1 --seconds 15"));
        assert!(!script.contains("--seed random"));
    }
}

#[test]
fn smoke_profile_keeps_saved_features_through_make_and_installer() {
    assert_profile("ferric-smoke");
}

#[test]
fn fuzz_profile_keeps_saved_features_through_make_and_installer() {
    assert_profile("ferric-fuzz");
}

#[test]
fn ci_profiles_reject_unsupported_architectures_before_generation() {
    for profile in ["ferric-smoke", "ferric-fuzz"] {
        for arch in ["aarch64", "riscv64gc"] {
            let fixture = Fixture::new();
            let output = fixture.output(&[
                "set",
                &format!("target.arch={arch}"),
                &format!("image.config={profile}"),
            ]);
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("Ferric smoke and fuzz configurations require x86_64")
            );
            for command in ["check", "generate"] {
                assert!(!fixture.output(&[command]).status.success());
            }
            assert!(!fixture.generated("config.mk").exists());
        }
    }
}

#[test]
fn absent_user_config_keeps_upstream_make_path_and_desktop_default() {
    let fixture = Fixture::new();
    assert_eq!(fixture.run(&["get", "image.config"]).trim(), "\"desktop\"");
    let config_makefile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mk/config.mk");
    let values = fixture.make_values(&config_makefile);
    assert_eq!(values[1], "desktop");
    assert!(values[3].is_empty());
    assert!(values[4].is_empty());
    assert!(values[5].is_empty());
    assert!(values[6].is_empty());
    assert!(!fixture.dir.path().join("ferric.toml").exists());
    assert!(!fixture.generated("config.mk").exists());
}

#[test]
fn hardware_profile_keeps_the_experimental_driver_on_its_selected_target() {
    let fixture = Fixture::new();
    fixture.run(&[
        "set",
        "image.config=ferric-hardware",
        "drivers.rtl8125=true",
    ]);
    fixture.run(&["generate"]);
    let mk = std::fs::read_to_string(fixture.generated("config.mk")).unwrap();
    assert!(mk.contains("CONFIG_NAME?=ferric-hardware"));
    assert!(mk.contains("export FERRIC_BASE_DRIVERS:=rtl8125d"));
    let fs = std::fs::read_to_string(fixture.generated("filesystem.toml")).unwrap();
    assert!(fs.contains("ferric-hardware.toml"));
}

#[test]
fn native_profile_rebuilds_fork_components_and_selects_native_tools() {
    let fixture = Fixture::new();
    fixture.run(&["set", "image.config=native", "build.repo_binary=true"]);
    fixture.run(&["generate", "--no-touch"]);
    let mk = std::fs::read_to_string(fixture.generated("config.mk")).unwrap();
    for setting in [
        "PREFIX_TOOLCHAIN?=llvm",
        "PODMAN_BUILD?=0",
        "FSTOOLS_NO_MOUNT?=1",
    ] {
        assert!(mk.contains(setting), "{mk}");
    }
    let config = redox_installer::Config::from_file(&fixture.generated("filesystem.toml")).unwrap();
    for package in ["kernel", "bootloader", "relibc", "base", "redoxfs", "fox"] {
        assert!(
            matches!(config.packages.get(package), Some(redox_installer::PackageConfig::Build(rule)) if rule == "source"),
            "{package}"
        );
    }
    fixture.run(&["set", "build.podman=true"]);
    fixture.run(&["generate", "--no-touch"]);
    let mk = std::fs::read_to_string(fixture.generated("config.mk")).unwrap();
    assert!(mk.contains("PREFIX_TOOLCHAIN?=recipe"), "{mk}");
    assert!(mk.contains("PODMAN_BUILD?=1"), "{mk}");
}
