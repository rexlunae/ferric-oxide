//! Native build entry point: rustup, Clang and the existing recipe/image engine.
//!
//! `cargo xtask setup` prepares Rust tools without invoking a system package
//! manager. `cargo xtask build` builds an image, and `cargo xtask run` boots it.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::env;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

use clap::{Parser, Subcommand, ValueEnum};
use thiserror::Error;

mod variants;

#[derive(Parser)]
#[command(about, version)]
struct Cli {
    /// Guest architecture (defaults to this host's architecture).
    #[arg(long, global = true, value_enum)]
    arch: Option<Architecture>,
    /// Use the Linux recipe toolchain inside an existing Podman installation.
    #[arg(long, global = true)]
    podman: bool,
    /// Print commands without executing them or creating configuration files.
    #[arg(long, global = true)]
    dry_run: bool,
    #[command(subcommand)]
    task: Task,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Architecture {
    #[value(name = "x86_64", alias = "amd64")]
    X86_64,
    #[value(name = "aarch64", alias = "arm64")]
    Aarch64,
}

impl Architecture {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "x86_64" | "amd64" => Ok(Self::X86_64),
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            _ => Err(BuildError::Architecture(name.to_owned())),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    fn qemu(self) -> &'static str {
        match self {
            Self::X86_64 => "qemu-system-x86_64",
            Self::Aarch64 => "qemu-system-aarch64",
        }
    }

    fn needs_prebuilt_std(self) -> bool {
        self == Self::X86_64
    }
}

fn selected_architecture(
    requested: Option<Architecture>,
    args: &[OsString],
) -> Result<Architecture> {
    if let Some(name) = args
        .iter()
        .rev()
        .find_map(|arg| arg.to_str()?.strip_prefix("ARCH="))
    {
        return Architecture::parse(name);
    }
    requested.map_or_else(|| Architecture::parse(env::consts::ARCH), Ok)
}

#[derive(Subcommand)]
enum Task {
    /// Allocate a sparse image file using the host filesystem.
    #[command(hide = true)]
    SparseFile {
        path: PathBuf,
        #[arg(long)]
        mib: u32,
    },
    /// Check native build dependencies (does not install anything).
    Doctor,
    /// Prepare rustup components, Cargo utilities and an initial native config.
    Setup,
    /// Build an image; pass extra Make assignments/targets after --.
    Build {
        /// Build every enabled CPU variant into a separate image directory.
        #[arg(long, conflicts_with = "cpu_variant")]
        all_variants: bool,
        /// Build one enabled CPU variant instead of the architecture's baseline.
        #[arg(long)]
        cpu_variant: Option<String>,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Build and boot in QEMU; pass extra Make assignments after --.
    Run {
        /// Boot one enabled CPU variant's separate image.
        #[arg(long)]
        cpu_variant: Option<String>,
        /// Boot the existing disk without rebuilding it or resetting guest changes.
        #[arg(long)]
        no_build: bool,
        /// Show the serial console without opening a display window.
        #[arg(long)]
        headless: bool,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
}

#[derive(Debug, Error)]
enum BuildError {
    #[error("unsupported architecture {0}; use --arch aarch64 or --arch x86_64")]
    Architecture(String),
    #[error("{program:?} could not be started")]
    Start {
        program: OsString,
        #[source]
        source: io::Error,
    },
    #[error("{program:?} failed with {status}")]
    Failed {
        program: OsString,
        status: ExitStatus,
    },
    #[error("cannot access {path:?}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "missing required tool {0}; install it using your preferred method, then rerun cargo xtask setup"
    )]
    Missing(&'static str),
    #[error(
        "missing required build-machine programs: {}; run ./native_bootstrap.sh to install them (or see Prerequisites in docs/src/building-redox.md)",
        .0.join(", ")
    )]
    MissingPrograms(Vec<&'static str>),
    #[error("cannot decode ferric-config's CPU variant plan")]
    VariantPlan {
        #[source]
        source: serde_json::Error,
    },
    #[error("CPU variant {level} is not enabled for {arch}; select it in make menuconfig")]
    DisabledVariant { arch: &'static str, level: String },
    #[error("no enabled CPU variants match the requested architecture")]
    NoVariants,
    #[error(
        "BUILD and FERRIC_CPU_VARIANT are assigned by the CPU variant command; remove the conflicting Make assignment"
    )]
    VariantAssignment,
}

type Result<T> = std::result::Result<T, BuildError>;

const CARGO_UTILITIES: [(&str, &str); 2] = [("cbindgen", "0.29.0"), ("just", "1.50.0")];

fn initial_config(arch: Architecture) -> String {
    format!(
        "\
# Native development profile; edit this file or run make menuconfig.\n\
[target]\narch = \"{}\"\n\n\
[image]\nconfig = \"native\"\n\n\
[build]\npodman = false\ntoolchain = \"llvm\"\nrepo_binary = true\n",
        arch.name()
    )
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn find_tool(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn execute(command: &mut Command, dry_run: bool) -> Result<()> {
    eprintln!("+ {command:?}");
    if dry_run {
        return Ok(());
    }
    let program = command.get_program().to_owned();
    let status = command.status().map_err(|source| BuildError::Start {
        program: program.clone(),
        source,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(BuildError::Failed { program, status })
    }
}

fn rust_channel() -> &'static str {
    include_str!("../../../rust-toolchain.toml")
        .lines()
        .find_map(|line| {
            line.strip_prefix("channel = \"")
                .and_then(|value| value.strip_suffix('"'))
        })
        .expect("the repository toolchain file defines its pinned channel")
}

fn cargo(root: &Path) -> Command {
    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(root)
        .env("RUSTUP_TOOLCHAIN", rust_channel())
        .env_remove("TARGET")
        .env_remove("CC");
    command
}

/// Programs that recipes built from source run on the build machine. Without
/// a package server every application recipe builds from source, so the
/// documented build needs all of them.
const SOURCE_BUILD_PROGRAMS: [&str; 14] = [
    "autoconf",
    "automake",
    "autopoint",
    "bison",
    "cmake",
    "flex",
    "m4",
    "makeinfo",
    "meson",
    "ninja",
    "patch",
    "perl",
    "pkg-config",
    "python3",
];

fn doctor(arch: Architecture) -> Result<()> {
    eprintln!("Guest architecture: {}", arch.name());
    let nasm = (arch == Architecture::X86_64).then_some("nasm");
    let required = [
        "rustup", "cargo", "clang", "clang++", "git", "curl", "tar", "make",
    ]
    .into_iter()
    .chain(nasm)
    .chain(SOURCE_BUILD_PROGRAMS);
    let mut missing = Vec::new();
    for tool in required {
        match find_tool(tool) {
            Some(path) => eprintln!("{tool}: {}", path.display()),
            None => {
                eprintln!("{tool}: MISSING");
                missing.push(tool);
            }
        }
    }
    for tool in ["cbindgen", "just", arch.qemu()] {
        match find_tool(tool) {
            Some(path) => eprintln!("{tool}: {}", path.display()),
            None => eprintln!(
                "{tool}: absent ({})",
                if tool.starts_with("qemu") {
                    "only needed to run the image"
                } else {
                    "cargo xtask setup installs this Cargo utility"
                }
            ),
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(BuildError::MissingPrograms(missing))
    }
}

fn initialize(root: &Path, arch: Architecture) -> Result<()> {
    let path = root.join("ferric.toml");
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            eprintln!("Keeping existing {}", path.display());
            return Ok(());
        }
        Err(source) => return Err(BuildError::Io { path, source }),
    };
    file.write_all(initial_config(arch).as_bytes())
        .map_err(|source| BuildError::Io { path, source })
}

fn version_satisfies(output: &[u8], minimum: &str) -> bool {
    let version = std::str::from_utf8(output)
        .ok()
        .and_then(|text| text.split_whitespace().nth(1))
        .and_then(|version| semver::Version::parse(version).ok());
    let minimum =
        semver::Version::parse(minimum).expect("Cargo utility version constants are valid SemVer");
    version.is_some_and(|version| version >= minimum)
}

fn utility_ready(tool: &str, minimum: &str) -> Result<bool> {
    let Some(path) = find_tool(tool) else {
        return Ok(false);
    };
    let output = Command::new(&path)
        .arg("--version")
        .output()
        .map_err(|source| BuildError::Start {
            program: path.into_os_string(),
            source,
        })?;
    Ok(output.status.success() && version_satisfies(&output.stdout, minimum))
}

fn setup(root: &Path, arch: Architecture, dry_run: bool) -> Result<()> {
    doctor(arch)?;
    let mut rustup = Command::new("rustup");
    rustup
        .current_dir(root)
        .env_remove("RUSTUP_TOOLCHAIN")
        .args(["component", "add", "rust-src", "llvm-tools-preview"]);
    execute(&mut rustup, dry_run)?;
    if arch.needs_prebuilt_std() {
        let mut rustup = Command::new("rustup");
        rustup
            .current_dir(root)
            .env_remove("RUSTUP_TOOLCHAIN")
            .args(["target", "add", "x86_64-unknown-redox"]);
        execute(&mut rustup, dry_run)?;
    }
    for (tool, version) in CARGO_UTILITIES {
        if !utility_ready(tool, version)? {
            execute(
                cargo(root).args(["install", "--locked", "--version", version, tool]),
                dry_run,
            )?;
        }
    }
    execute(
        Command::new("git").current_dir(root).args([
            "submodule",
            "update",
            "--init",
            "--recursive",
        ]),
        dry_run,
    )?;
    if !dry_run {
        initialize(root, arch)?;
    }
    Ok(())
}

fn make_command(
    root: &Path,
    target: &str,
    arch: Architecture,
    podman: bool,
    args: &[OsString],
) -> Command {
    let mut command = Command::new(env::var_os("MAKE").unwrap_or_else(|| "make".into()));
    command.current_dir(root).env_remove("RUSTUP_TOOLCHAIN");
    // These select execution tools, while OS features still come from ferric.toml.
    command.args([target, "FSTOOLS_NO_MOUNT=1", "FSTOOLS_IN_PODMAN=0"]);
    command.arg(format!("ARCH={}", arch.name()));
    if podman {
        command.args(["PODMAN_BUILD=1", "PREFIX_TOOLCHAIN=recipe"]);
        if !Path::new("/sys/fs/selinux").exists() {
            command.arg("USE_SELINUX=0");
        }
    } else {
        command.args(["PODMAN_BUILD=0", "PREFIX_TOOLCHAIN=llvm"]);
    }
    if !io::stdout().is_terminal() {
        command.env("CI", "1");
    }
    // Architecture was resolved before selecting Make and QEMU tools. Keep a
    // legacy ARCH=arm64 alias from overriding the normalized ARCH=aarch64.
    command.args(
        args.iter()
            .filter(|arg| !arg.to_str().is_some_and(|arg| arg.starts_with("ARCH="))),
    );
    command
}

fn run(cli: Cli) -> Result<()> {
    let root = root();
    match cli.task {
        Task::SparseFile { path, mib } => {
            if cli.dry_run {
                eprintln!("Would allocate {mib} MiB at {}", path.display());
                return Ok(());
            }
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .map_err(|source| BuildError::Io {
                    path: path.clone(),
                    source,
                })?;
            file.set_len(u64::from(mib) * 1024 * 1024)
                .map_err(|source| BuildError::Io { path, source })
        }
        Task::Doctor => doctor(selected_architecture(cli.arch, &[])?),
        Task::Setup => setup(&root, selected_architecture(cli.arch, &[])?, cli.dry_run),
        Task::Build {
            args,
            all_variants,
            cpu_variant,
        } => {
            if all_variants {
                let filter = if cli.arch.is_some()
                    || args
                        .iter()
                        .any(|arg| arg.to_str().is_some_and(|arg| arg.starts_with("ARCH=")))
                {
                    Some(selected_architecture(cli.arch, &args)?)
                } else {
                    None
                };
                variants::build_all(&root, filter, cli.podman, &args, cli.dry_run)
            } else if let Some(level) = cpu_variant {
                variants::run_one(
                    &root,
                    "qemu-image",
                    selected_architecture(cli.arch, &args)?,
                    &level,
                    cli.podman,
                    &args,
                    cli.dry_run,
                )
            } else {
                execute(
                    &mut make_command(
                        &root,
                        "qemu-image",
                        selected_architecture(cli.arch, &args)?,
                        cli.podman,
                        &args,
                    ),
                    cli.dry_run,
                )
            }
        }
        Task::Run {
            headless,
            no_build,
            mut args,
            cpu_variant,
        } => {
            let arch = selected_architecture(cli.arch, &args)?;
            if !cli.dry_run && find_tool(arch.qemu()).is_none() {
                return Err(BuildError::Missing(arch.qemu()));
            }
            if headless {
                args.push("gpu=no".into());
            }
            if let Some(level) = cpu_variant {
                return variants::run_one(
                    &root,
                    if no_build { "qemu-run" } else { "qemu" },
                    arch,
                    &level,
                    cli.podman,
                    &args,
                    cli.dry_run,
                );
            }
            execute(
                &mut make_command(
                    &root,
                    if no_build { "qemu-run" } else { "qemu" },
                    arch,
                    cli.podman,
                    &args,
                ),
                cli.dry_run,
            )
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ferric-build: {error}");
            let mut cause = std::error::Error::source(&error);
            while let Some(error) = cause {
                eprintln!("  caused by: {error}");
                cause = error.source();
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, fs};

    #[test]
    fn architecture_defaults_to_host_and_explicit_selection_overrides_it() {
        assert_eq!(
            selected_architecture(None, &[]).unwrap(),
            Architecture::parse(env::consts::ARCH).unwrap()
        );
        assert_eq!(Architecture::parse("arm64").unwrap(), Architecture::Aarch64);
        for (name, arch) in [
            ("aarch64", Architecture::Aarch64),
            ("x86_64", Architecture::X86_64),
        ] {
            let cli = Cli::try_parse_from(["xtask", "run", "--arch", name]).unwrap();
            assert_eq!(selected_architecture(cli.arch, &[]).unwrap(), arch);
            let command = make_command(Path::new("/example"), "qemu", arch, false, &[]);
            assert!(
                command
                    .get_args()
                    .any(|arg| arg == OsStr::new(&format!("ARCH={name}")))
            );
            assert_eq!(arch.qemu(), format!("qemu-system-{name}"));
        }
        assert_eq!(
            selected_architecture(Some(Architecture::Aarch64), &["ARCH=x86_64".into()]).unwrap(),
            Architecture::X86_64
        );
        assert!(selected_architecture(None, &["ARCH=unknown".into()]).is_err());
        assert!(!Architecture::Aarch64.needs_prebuilt_std());
        let args = ["ARCH=arm64".into()];
        let arch = selected_architecture(None, &args).unwrap();
        let command = make_command(Path::new("/example"), "qemu", arch, false, &args);
        assert!(command.get_args().any(|arg| arg == "ARCH=aarch64"));
        assert!(!command.get_args().any(|arg| arg == "ARCH=arm64"));
    }
    #[test]
    fn setup_preserves_existing_configuration() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ferric.toml"), "# user choices\n").unwrap();
        initialize(dir.path(), Architecture::Aarch64).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("ferric.toml")).unwrap(),
            "# user choices\n"
        );
    }
    #[test]
    fn setup_creates_native_profile() {
        for arch in [Architecture::Aarch64, Architecture::X86_64] {
            let dir = tempfile::tempdir().unwrap();
            initialize(dir.path(), arch).unwrap();
            let config = fs::read_to_string(dir.path().join("ferric.toml")).unwrap();
            assert_eq!(config, initial_config(arch));
            assert!(config.contains(&format!("arch = \"{}\"", arch.name())));
        }
    }
    #[test]
    fn native_and_container_builds_are_explicit() {
        let root = Path::new("/example");
        let native = make_command(root, "all", Architecture::Aarch64, false, &[]);
        let args: Vec<_> = native.get_args().collect();
        assert!(args.contains(&OsStr::new("PODMAN_BUILD=0")));
        assert!(args.contains(&OsStr::new("PREFIX_TOOLCHAIN=llvm")));
        let container = make_command(root, "all", Architecture::X86_64, true, &[]);
        let args: Vec<_> = container.get_args().collect();
        assert!(args.contains(&OsStr::new("PODMAN_BUILD=1")));
        assert!(args.contains(&OsStr::new("PREFIX_TOOLCHAIN=recipe")));
    }
    #[test]
    fn make_arguments_remain_separate_and_override_defaults() {
        let command = make_command(
            Path::new("/example"),
            "qemu",
            Architecture::X86_64,
            false,
            &["QEMU=/path with spaces/qemu".into(), "QEMU_MEM=4096".into()],
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args[args.len() - 2],
            OsStr::new("QEMU=/path with spaces/qemu")
        );
        assert_eq!(args.last(), Some(&OsStr::new("QEMU_MEM=4096")));
    }
    #[test]
    fn cargo_utility_versions_reject_old_and_accept_newer_tools() {
        assert!(!version_satisfies(b"cbindgen 0.26.0\n", "0.29.0"));
        assert!(version_satisfies(b"cbindgen 0.29.0\n", "0.29.0"));
        assert!(version_satisfies(b"cbindgen 0.30.0\n", "0.29.0"));
        assert!(!version_satisfies(b"unexpected output", "0.29.0"));
    }

    #[test]
    fn failed_command_propagates_exit_status() {
        assert!(matches!(
            execute(Command::new("sh").args(["-c", "exit 9"]), false),
            Err(BuildError::Failed { .. })
        ));
    }
}
