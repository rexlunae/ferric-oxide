//! Exercise dependency installation and the real build prescript through `repo cook`.

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn recipe(root: &Path, name: &str, build: &str) {
    let directory = root.join("recipes").join(name);
    fs::create_dir_all(directory.join("source")).unwrap();
    fs::write(directory.join("recipe.toml"), build).unwrap();
}

#[test]
fn guest_built_tools_run_host_dependencies_with_a_target_sysroot() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let binaries = root.join("target/release");
    fs::create_dir_all(&binaries).unwrap();
    // No compiler is needed for these shell fixtures. Only replace the outer
    // compiler-environment wrapper; `repo` still installs signed packages and
    // runs its real prescripts, dependency selection and shim generation.
    executable(
        &binaries.join("cookbook_redoxer"),
        "#!/bin/sh\nexec \"$@\"\n",
    );
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_repo"), binaries.join("repo")).unwrap();
    recipe(
        root,
        "host-helper",
        r#"[build]
template = "custom"
tool-runs-on = "host"
script = '''
mkdir -p "$COOKBOOK_STAGE/usr/bin" "$COOKBOOK_STAGE/usr/lib"
printf '#!/bin/sh\nprintf host-helper-ran\n' > "$COOKBOOK_STAGE/usr/bin/host-helper"
chmod +x "$COOKBOOK_STAGE/usr/bin/host-helper"
printf host-library > "$COOKBOOK_STAGE/usr/lib/placement.txt"
'''
"#,
    );
    recipe(
        root,
        "target-library",
        r#"[build]
template = "custom"
script = '''
mkdir -p "$COOKBOOK_STAGE/usr/lib"
printf target-library > "$COOKBOOK_STAGE/usr/lib/placement.txt"
'''
"#,
    );
    recipe(
        root,
        "nested-generator",
        r#"[build]
template = "custom"
script = '''
mkdir -p "$COOKBOOK_STAGE/usr/bin"
printf '#!/bin/sh\nexit 0\n' > "$COOKBOOK_STAGE/usr/bin/nested-generator"
chmod +x "$COOKBOOK_STAGE/usr/bin/nested-generator"
'''
"#,
    );
    recipe(
        root,
        "generator",
        r#"[build]
template = "custom"
dependencies = ["target:target-library"]
dev-dependencies = ["host:host-helper", "host:nested-generator"]
script = '''
test "$TARGET" = "$COOKBOOK_CROSS_TARGET"
test "$(command -v host-helper)" = "$COOKBOOK_TOOLCHAIN/bin/host-helper"
test "$(host-helper)" = host-helper-ran
test "$(cat "$COOKBOOK_SYSROOT/lib/placement.txt")" = target-library
test "$(cat "$COOKBOOK_TOOLCHAIN/lib/placement.txt")" = host-library
test "$PKG_CONFIG_LIBDIR" = "$COOKBOOK_SYSROOT/lib/pkgconfig"
test "$PKG_CONFIG_SYSROOT_DIR" = "$COOKBOOK_SYSROOT"
test "$(command -v nested-generator)" = "$COOKBOOK_GUEST_TOOLS_SHIMS/nested-generator"
case "$LDFLAGS" in *"-L$COOKBOOK_SYSROOT/lib"*) ;; *) exit 1 ;; esac
case "$LDFLAGS" in *"$COOKBOOK_TOOLCHAIN"*) exit 1 ;; esac
mkdir -p "$COOKBOOK_STAGE/usr/share"
host-helper > "$COOKBOOK_STAGE/usr/share/result"
'''
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_repo"))
        .args(["cook", "host:generator"])
        .current_dir(root)
        .env("CI", "1")
        .env("COOKBOOK_CROSS_TARGET", "x86_64-unknown-redox")
        .env("FERRIC_BUILD_TOOLS", "guest")
        .env("FERRIC_GUEST_TOOLS_DIR", root.join("unused-guest.img"))
        .env("COOKBOOK_OFFLINE", "true")
        .env("COOKBOOK_NOSTRIP", "1")
        .env("COOKBOOK_CLEAN_TARGET", "false")
        .env("COOKBOOK_VERBOSE_CMD", "true")
        .env_remove("COOKBOOK_TOOLCHAIN")
        .env_remove("COOKBOOK_GUEST_TOOLS_SHIMS")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "repo cook failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = root.join(concat!(
        "recipes/generator/target/x86_64-unknown-redox/",
        "x86_64-unknown-redox/stage/usr/share/result"
    ));
    assert_eq!(fs::read_to_string(result).unwrap(), "host-helper-ran");
}
