#![allow(clippy::unwrap_used)] // the build script can panic

use std::{env, path::Path, process::Command};
use toml::Table;

fn parse_kconfig(arch: &str) -> Option<()> {
    println!("cargo:rerun-if-changed=config.toml");

    assert!(Path::new("config.toml.example").try_exists().unwrap());
    if !Path::new("config.toml").try_exists().unwrap() {
        std::fs::copy("config.toml.example", "config.toml").unwrap();
    }
    let config_str = std::fs::read_to_string("config.toml").unwrap();
    let root: Table = toml::from_str(&config_str).unwrap();

    // Ferric Oxide: events the `trace` feature turns on at boot, besides those the boot
    // environment's FERRIC_TRACE names (src/trace/imp.rs), from `trace.boot`.
    if let Some(boot) = root
        .get("trace")
        .and_then(|t| t.as_table().unwrap().get("boot"))
    {
        let boot = boot.as_str().unwrap();
        assert!(
            !boot.contains(['\n', '\r']),
            "trace.boot must be one line"
        );
        println!("cargo:rustc-env=FERRIC_TRACE_BOOT={boot}");
    }

    // Ferric Oxide: the physical address of the crash region (`trace_crash`,
    // src/trace/crash.rs), from `trace.crash_base`: a page-aligned number, as an integer or a
    // "0x..." string.
    if let Some(base) = root
        .get("trace")
        .and_then(|t| t.as_table().unwrap().get("crash_base"))
    {
        let base = match base {
            toml::Value::Integer(n) => u64::try_from(*n).ok(),
            toml::Value::String(s) => match s.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            },
            _ => None,
        };
        let base = base
            .filter(|base| base % 4096 == 0)
            .expect("trace.crash_base must be a page-aligned address");
        println!("cargo:rustc-env=FERRIC_TRACE_CRASH_BASE={base}");
    }

    let arch_table = root.get("arch")?.as_table().unwrap().get(arch)?.as_table().unwrap();

    // Minimum microarchitecture level, checked at boot (src/arch/x86_64/cpu_level.rs).
    println!("cargo::rustc-check-cfg=cfg(x86_64_level, values(\"v1\", \"v2\", \"v3\", \"v4\"))");
    if arch == "x86_64"
        && let Some(level) = arch_table.get("level")
    {
        let level = level.as_str().unwrap();
        assert!(
            matches!(level, "v1" | "v2" | "v3" | "v4"),
            "arch.x86_64.level must be v1, v2, v3 or v4"
        );
        println!("cargo:rustc-cfg=x86_64_level=\"{level}\"");
    }

    // Ferric Oxide: VT-d and AMD-Vi DMA remapping (src/arch/x86_64/iommu.rs) and the iommu
    // scheme (src/scheme/iommu.rs), both built for "auto" and "enforce".
    if arch == "x86_64" {
        let iommu = arch_table
            .get("iommu")
            .map_or("never", |v| v.as_str().unwrap());
        assert!(
            matches!(iommu, "auto" | "enforce" | "never"),
            "arch.x86_64.iommu must be \"auto\", \"enforce\" or \"never\""
        );
        println!("cargo:rustc-cfg=iommu=\"{iommu}\"");
        if iommu != "never" {
            println!("cargo:rustc-cfg=iommu_enabled");
        }
    }

    let altfeatures = arch_table
        .get("features")?
        .as_table()
        .unwrap();

    #[expect(clippy::format_collect)] // TODO: remove once version is bumped
    let mut features_list = altfeatures
        .keys()
        .map(|feat| format!(", {feat:?}"))
        .collect::<String>();
    // Ferric Oxide: the code checks for "ibt", "pcid" and "shstk" even when an older config.toml
    // does not list them.
    for feat in ["ibt", "pcid", "shstk"] {
        if !altfeatures.contains_key(feat) {
            features_list.push_str(&format!(", {feat:?}"));
        }
    }
    println!("cargo::rustc-check-cfg=cfg(cpu_feature_always, values(\"\"{features_list}))");
    println!("cargo::rustc-check-cfg=cfg(cpu_feature_auto, values(\"\"{features_list}))");
    println!("cargo::rustc-check-cfg=cfg(cpu_feature_never, values(\"\"{features_list}))");

    let self_modifying = env::var("CARGO_FEATURE_SELF_MODIFYING").is_ok();

    for (name, value) in altfeatures {
        let mut choice = value.as_str().unwrap();
        assert!(matches!(choice, "always" | "never" | "auto"));

        if !self_modifying && choice == "auto" {
            choice = "never";
        }

        println!("cargo:rustc-cfg=cpu_feature_{choice}=\"{name}\"");

        // Ferric Oxide (shadow stacks): one cfg for "the build has shadow-stack support", so the
        // code outside arch/x86_64/shstk.rs needs no feature-choice logic.
        if name == "shstk" && choice != "never" {
            println!("cargo:rustc-cfg=shstk_compiled");
        }

        // Ferric Oxide (IBT): hand-written entry points get endbr64 from this cfg, but the
        // compiler only adds it to Rust functions (in every crate, core and alloc included) when
        // RUSTFLAGS carries the flags the kernel Makefile adds. Without them the kernel would
        // take a #CP on the first indirect call on CET hardware, so refuse to build.
        if name == "ibt" && choice != "never" {
            println!("cargo:rerun-if-env-changed=RUSTFLAGS");
            println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
            let flags = env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
            let flags: Vec<&str> = flags.split('\x1f').collect();
            let has = |opt: &str, val: &str| {
                flags.windows(2).any(|w| w == [opt, val])
                    || flags.contains(&format!("{opt}{val}").as_str())
            };
            assert!(
                has("-Z", "cf-protection=branch") && has("-C", "jump-tables=no"),
                "arch.x86_64.features.ibt is \"{choice}\" but RUSTFLAGS lacks \
                 `-Zcf-protection=branch -Cjump-tables=no`; build with the kernel Makefile"
            );
        }
    }

    Some(())
}

fn main() {
    println!("cargo::rustc-env=TARGET={}", env::var("TARGET").unwrap());
    println!("cargo::rustc-check-cfg=cfg(dtb)");
    // Ferric Oxide: set by parse_kconfig from arch.x86_64.iommu.
    println!("cargo::rustc-check-cfg=cfg(iommu, values(\"auto\", \"enforce\", \"never\"))");
    println!("cargo::rustc-check-cfg=cfg(iommu_enabled)");
    // Ferric Oxide: set by parse_kconfig unless arch.x86_64.features.shstk is "never".
    println!("cargo::rustc-check-cfg=cfg(shstk_compiled)");

    let out_dir = env::var("OUT_DIR").unwrap();
    let arch_str = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    match &*arch_str {
        "aarch64" => {
            println!("cargo::rustc-cfg=dtb");
        }
        "x86" => {
            println!("cargo::rerun-if-changed=src/asm/x86/trampoline.asm");

            let status = Command::new("nasm")
                .arg("-f")
                .arg("bin")
                .arg("-o")
                .arg(format!("{}/trampoline", out_dir))
                .arg("src/asm/x86/trampoline.asm")
                .status()
                .expect("failed to run nasm");
            if !status.success() {
                panic!("nasm failed with exit status {}", status);
            }
        }
        "x86_64" => {
            println!("cargo::rerun-if-changed=src/asm/x86_64/trampoline.asm");

            let status = Command::new("nasm")
                .arg("-f")
                .arg("bin")
                .arg("-o")
                .arg(format!("{}/trampoline", out_dir))
                .arg("src/asm/x86_64/trampoline.asm")
                .status()
                .expect("failed to run nasm");
            if !status.success() {
                panic!("nasm failed with exit status {}", status);
            }
        }
        "riscv64" => {
            println!("cargo::rustc-cfg=dtb");
        }
        _ => (),
    }

    let _ = parse_kconfig(&arch_str);
}
