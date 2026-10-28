//! Target overrides and configuration recovery through the real Make fragments.

use std::path::Path;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn config(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ferric-config"))
            .current_dir(self.0.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::copy(repo.join("features.toml"), dir.path().join("features.toml")).unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        for name in ["native", "desktop", "base"] {
            std::fs::copy(
                repo.join(format!("config/{name}.toml")),
                dir.path().join(format!("config/{name}.toml")),
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("ferric.toml"), config).unwrap();
        std::fs::write(dir.path().join("Makefile"), format!(
            ".DEFAULT_GOAL := probe\nFERRIC_CONFIG_TOOL = {} --root . --config $(FERRIC_CONFIG) --out $(FERRIC_OUT)\ninclude {}/mk/config.mk\ninclude {}/mk/ferric.mk\nFORCE:\nprobe:\n\t@printf 'RESULT|%s|%s|%s|%s|%s\\n' '$(ARCH)' '$(BOARD)' '$(CONFIG_NAME)' '$(FERRIC_KERNEL_FEATURES)' '$(FERRIC_TARGET_RUSTFLAGS)'\n",
            env!("CARGO_BIN_EXE_ferric-config"), repo.display(), repo.display()
        )).unwrap();
        Self(dir)
    }

    fn make(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new("make");
        cmd.current_dir(self.0.path())
            .arg("--no-print-directory")
            .args(args);
        for name in [
            "ARCH",
            "BOARD",
            "CONFIG_NAME",
            "FILESYSTEM_CONFIG",
            "MAKEFLAGS",
            "MFLAGS",
            "MAKEOVERRIDES",
            "FERRIC_CONFIG",
            "FERRIC_KERNEL_FEATURES",
            "FERRIC_TARGET_RUSTFLAGS",
        ] {
            cmd.env_remove(name);
        }
        cmd.output().unwrap()
    }

    fn probe(&self, args: &[&str]) -> Vec<String> {
        let output = self.make(args);
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("RESULT|"))
            .unwrap()
            .split('|')
            .map(str::to_owned)
            .collect()
    }
}

const INVALID_X86: &str = "[target]\narch='x86_64'\n[image]\nconfig='native'\n[kernel]\ntrace=true\nprofiling=true\npti=true\nregbuf=true\n";

#[test]
fn architecture_override_is_resolved_before_validation_without_editing_saved_choices() {
    let fixture = Fixture::new(INVALID_X86);
    let result = fixture.probe(&["probe", "ARCH=aarch64"]);
    assert_eq!(&result[..3], ["aarch64", "", "native"]);
    let features: Vec<_> = result[3].split(',').collect();
    assert!(features.contains(&"regbuf"));
    for x86 in ["trace", "profiling", "pti", "x86_kvm_pv"] {
        assert!(!features.contains(&x86), "{result:?}");
    }
    assert!(!result[4].contains("x86"));
    assert_eq!(
        std::fs::read_to_string(fixture.0.path().join("ferric.toml")).unwrap(),
        INVALID_X86
    );
    assert!(!fixture.make(&["config-check"]).status.success());
    assert!(
        fixture
            .make(&["config-check", "ARCH=aarch64"])
            .status
            .success()
    );
}

#[test]
fn changing_or_removing_make_target_overrides_never_reuses_stale_generated_values() {
    let fixture =
        Fixture::new("[target]\narch='x86_64'\n[image]\nconfig='native'\n[kernel]\ntrace=true\n");
    assert_eq!(fixture.probe(&[])[0], "x86_64");
    assert_eq!(fixture.probe(&["probe", "ARCH=aarch64"])[0], "aarch64");
    let restored = fixture.probe(&[]);
    assert_eq!(restored[0], "x86_64");
    assert!(restored[3].split(',').any(|s| s == "trace"));
    assert!(restored[4].contains("x86-64-v3"));
    assert_eq!(
        fixture.probe(&["probe", "CONFIG_NAME=desktop"])[2],
        "desktop"
    );
    assert_eq!(fixture.probe(&[])[2], "native");
    let generated = fixture.0.path().join("build/ferric/config.mk");
    let modified = generated.metadata().unwrap().modified().unwrap();
    fixture.probe(&[]);
    assert_eq!(generated.metadata().unwrap().modified().unwrap(), modified);
}

#[test]
fn legacy_dotconfig_target_and_explicit_empty_board_are_resolved_before_generation() {
    let fixture = Fixture::new(INVALID_X86);
    std::fs::write(fixture.0.path().join(".config"), "ARCH=aarch64\nBOARD=\n").unwrap();
    assert_eq!(&fixture.probe(&[])[..2], ["aarch64", ""]);
    assert!(!fixture.make(&["probe", "ARCH=x86_64"]).status.success());
}

#[test]
fn explicit_empty_board_clears_a_saved_board() {
    let fixture =
        Fixture::new("[target]\narch='aarch64'\nboard='raspi3bp'\n[image]\nconfig='native'\n");
    let board = fixture.0.path().join("config/aarch64/raspi3bp");
    std::fs::create_dir_all(&board).unwrap();
    std::fs::write(board.join("native.toml"), "").unwrap();
    assert_eq!(fixture.probe(&[])[1], "raspi3bp");
    assert_eq!(fixture.probe(&["probe", "BOARD="])[1], "");
    assert_eq!(fixture.probe(&[])[1], "raspi3bp");
}

#[test]
fn configuration_commands_can_repair_invalid_or_missing_generated_configuration() {
    let fixture = Fixture::new(INVALID_X86);
    for (goal, verb) in [
        ("menuconfig", "menu"),
        ("config-check", "check"),
        ("config-list", "list"),
        ("config-generate", "generate"),
    ] {
        // A stub keeps the interactive editor test independent of a terminal.
        // Before the fix, Make runs this as `generate` while remaking its include.
        let output = fixture.make(&[goal, "FERRIC_CONFIG_TOOL=printf 'CALLED %s\\n'"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let called: Vec<_> = text.lines().filter(|s| s.starts_with("CALLED ")).collect();
        assert_eq!(called, [format!("CALLED {verb}")]);
        assert!(!fixture.0.path().join("build/ferric/config.mk").exists());
    }
}

#[test]
fn invalid_override_values_are_reported_without_writing_output_or_user_config() {
    let fixture = Fixture::new(INVALID_X86);
    for value in ["target.arch=unknown", "target.arch", "unknown.option=true"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ferric-config"))
            .current_dir(fixture.0.path())
            .args(["generate", "--override", value])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!fixture.0.path().join("build/ferric/config.mk").exists());
        assert_eq!(
            std::fs::read_to_string(fixture.0.path().join("ferric.toml")).unwrap(),
            INVALID_X86
        );
    }
}

#[test]
fn cpu_variants_are_independent_for_both_architectures_and_respect_each_minimum() {
    let fixture = Fixture::new(
        "[target]\narch='aarch64'\n[image]\nconfig='native'\n[cpu]\nx86_64_v3=true\nx86_64_v4=true\naarch64_v8_0=true\naarch64_v8_2=true\naarch64_v9=true\n",
    );
    let plan = || {
        let output = fixture.config(&["variants", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout).unwrap()
    };
    let variants = plan();
    assert_eq!(variants.len(), 5);
    assert_eq!(variants[0]["minimum"], "v3");
    assert_eq!(variants[2]["minimum"], "armv8.0-a");
    assert!(
        fixture
            .config(&["set", "cpu.x86_64_level=v4", "cpu.aarch64_level=armv8.2-a"])
            .status
            .success()
    );
    let variants = plan();
    assert_eq!(
        variants
            .iter()
            .map(|v| v["level"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["v4", "armv8.2-a", "armv9-a"]
    );
    assert_eq!(variants[0]["minimum"], "v4");
    assert_eq!(variants[1]["minimum"], "armv8.2-a");
    // Checkboxes do not mix the two architectures' compiler flags.
    let arm = fixture.probe(&["probe", "ARCH=aarch64", "FERRIC_CPU_VARIANT=armv9-a"]);
    assert!(arm[4].contains("+v9a"));
    assert!(!arm[4].contains("x86"));
    let x86 = fixture.probe(&["probe", "ARCH=x86_64", "FERRIC_CPU_VARIANT=v4"]);
    assert!(x86[4].contains("x86-64-v4"));
    assert!(!x86[4].contains("v9a"));
}

#[test]
fn each_variant_checkbox_can_be_disabled_without_changing_the_other_cpu_type() {
    let fixture = Fixture::new(
        "[cpu]\nx86_64_v3=false\nx86_64_v4=false\naarch64_v8_0=true\naarch64_v8_2=true\n",
    );
    let output = fixture.config(&["variants", "--json"]);
    assert!(output.status.success());
    let variants: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(variants.len(), 2);
    assert!(variants.iter().all(|v| v["arch"] == "aarch64"));
}

/// `host.arch` lets the catalog tell a native build from a cross-compiled one
/// without a new option, so build.build_tools can default to the guest only
/// where the guest can run what it builds.
#[test]
fn build_tools_default_to_the_guest_only_on_a_native_build() {
    let fixture = Fixture::new("[image]\nconfig='native'\n");
    let host = std::env::consts::ARCH;
    let cross = if host == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };

    let native = fixture
        .config(&["list"])
        .stdout
        .split(|b| *b == b'\n')
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<String>();
    assert!(
        native.contains("build.build_tools"),
        "build.build_tools is in the catalog"
    );

    // Each generated configuration keeps the value its own architecture resolves
    // to, so a cross build falls back to the host tools.
    let value = |arch: &str| {
        let output = fixture.config(&[
            "generate",
            "--no-touch",
            "--override",
            &format!("target.arch={arch}"),
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mk = std::fs::read_to_string(fixture.0.path().join("build/ferric/config.mk")).unwrap();
        mk.lines()
            .find_map(|line| line.strip_prefix("export FERRIC_BUILD_TOOLS:="))
            .unwrap()
            .to_owned()
    };
    assert_eq!(value(host), "guest");
    assert_eq!(value(cross), "host");
}

#[test]
fn keeping_build_tools_without_a_guest_environment_is_an_error() {
    let fixture = Fixture::new("[build]\nbuild_tools='host'\nkeep_build_tools=true\n");
    let output = fixture.config(&["check"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("build.keep_build_tools") && stderr.contains("build.build_tools"),
        "{stderr}"
    );
}

#[test]
fn a_cross_build_warns_that_the_tools_come_from_the_host() {
    let fixture = Fixture::new("[build]\nbuild_tools='guest'\n");
    let cross = if std::env::consts::ARCH == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };
    let output = fixture.config(&["check", "--override", &format!("target.arch={cross}")]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("uses host tools instead"),
        "expected the cross-build warning, got {stderr}"
    );
}
