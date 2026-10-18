//! Running Cargo and rustc in a fixed environment.
//!
//! Every command starts from an empty environment. Only what the tools
//! need to find themselves and the user's toolchains is passed through
//! ([`BASE_ENV`]); proxy and certificate settings ([`NETWORK_ENV`]) are
//! added only for `cargo fetch`, the one step that downloads. Cargo gets a
//! private `CARGO_HOME`, so neither the user's Cargo configuration nor
//! `RUSTFLAGS`, `RUSTC_WRAPPER` or `CARGO_BUILD_*` variables can change the
//! build, and `RUSTUP_TOOLCHAIN` is dropped so the source's own
//! `rust-toolchain.toml` picks the toolchain unless the caller names one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::{BuildError, Step};

/// Variables passed to every command.
const BASE_ENV: &[&str] = &["PATH", "HOME", "RUSTUP_HOME", "TMPDIR"];

/// Variables passed to `cargo fetch` only: proxies, certificates and
/// Cargo's own network settings.
const NETWORK_ENV: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "GIT_SSL_CAINFO",
    "SSH_AUTH_SOCK",
    "CARGO_HTTP_CAINFO",
    "CARGO_HTTP_PROXY",
    "CARGO_NET_GIT_FETCH_WITH_CLI",
];

/// Whether a command may use the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Network {
    Allowed,
    Denied,
}

/// What a command's standard error does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stderr {
    /// Shown to the user, for long steps whose progress matters.
    Inherit,
    /// Captured, and kept in the error if the command fails.
    Capture,
}

/// Runs tools in the package directory with the fixed environment.
#[derive(Debug, Clone)]
pub(crate) struct Tools {
    dir: PathBuf,
    cargo_home: PathBuf,
    toolchain: Option<String>,
    extra_env: Vec<(String, OsString)>,
}

/// The toolchain as rustc and Cargo describe themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolchainInfo {
    /// `rust-<release>:<BLAKE3 of rustc -vV and cargo -vV>`.
    pub id: String,
    /// rustc's host triple, the default target.
    pub host: String,
}

impl Tools {
    pub(crate) fn new(dir: &Path, cargo_home: &Path, toolchain: Option<&str>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            cargo_home: cargo_home.to_path_buf(),
            toolchain: toolchain.map(str::to_owned),
            extra_env: Vec::new(),
        }
    }

    /// Adds a variable to every later command.
    pub(crate) fn set_env(&mut self, name: &str, value: impl Into<OsString>) {
        self.extra_env.push((name.to_owned(), value.into()));
    }

    /// A command for `program` with the fixed environment.
    pub(crate) fn command(&self, program: &str, network: Network) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.dir)
            .env_clear()
            .stdin(Stdio::null());
        let passed = match network {
            Network::Allowed => [BASE_ENV, NETWORK_ENV].concat(),
            Network::Denied => BASE_ENV.to_vec(),
        };
        for name in passed {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("CARGO_HOME", &self.cargo_home);
        command.env("CARGO_TERM_COLOR", "never");
        if network == Network::Denied {
            command.env("CARGO_NET_OFFLINE", "true");
        }
        if let Some(toolchain) = &self.toolchain {
            command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        for (name, value) in &self.extra_env {
            command.env(name, value);
        }
        command
    }

    /// Runs `command` for `step` and returns its output.
    ///
    /// # Errors
    ///
    /// [`BuildError::Spawn`] if it cannot start and
    /// [`BuildError::Command`] if it fails.
    pub(crate) fn run(
        &self,
        step: Step,
        mut command: Command,
        stderr: Stderr,
    ) -> Result<Output, BuildError> {
        command.stdout(Stdio::piped());
        command.stderr(match stderr {
            Stderr::Inherit => Stdio::inherit(),
            Stderr::Capture => Stdio::piped(),
        });
        let output = command
            .output()
            .map_err(|source| BuildError::Spawn { step, source })?;
        if !output.status.success() {
            return Err(BuildError::Command {
                step,
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(output)
    }

    /// Identifies the toolchain that builds in this directory. The network
    /// is allowed, so rustup can install the toolchain the source pins.
    ///
    /// # Errors
    ///
    /// The errors of [`Tools::run`], and [`BuildError::ToolchainVersion`] if
    /// rustc's output lacks a release or host.
    pub(crate) fn toolchain_info(&self) -> Result<ToolchainInfo, BuildError> {
        let mut rustc = self.command("rustc", Network::Allowed);
        rustc.arg("-vV");
        let rustc = self.run(Step::ToolchainVersion, rustc, Stderr::Capture)?;
        let mut cargo = self.command("cargo", Network::Allowed);
        cargo.arg("-vV");
        let cargo = self.run(Step::ToolchainVersion, cargo, Stderr::Capture)?;
        parse_toolchain(&rustc.stdout, &cargo.stdout)
    }

    /// Whether the `cargo auditable` subcommand is installed.
    ///
    /// # Errors
    ///
    /// The errors of [`Tools::run`] for `cargo --list`.
    pub(crate) fn has_auditable(&self) -> Result<bool, BuildError> {
        let mut command = self.command("cargo", Network::Denied);
        command.arg("--list");
        let output = self.run(Step::ListCommands, command, Stderr::Capture)?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some("auditable")))
    }
}

fn parse_toolchain(rustc: &[u8], cargo: &[u8]) -> Result<ToolchainInfo, BuildError> {
    let text = String::from_utf8_lossy(rustc);
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|value| value.trim().to_owned())
    };
    let (Some(release), Some(host)) = (field("release:"), field("host:")) else {
        return Err(BuildError::ToolchainVersion {
            output: text.into_owned(),
        });
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(rustc);
    hasher.update(cargo);
    Ok(ToolchainInfo {
        id: format!("rust-{release}:{}", hasher.finalize().to_hex()),
        host,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rustc_version_output() {
        let rustc = b"rustc 1.93.0-nightly (23a3312d9 2026-05-23)\nbinary: rustc\n\
            commit-hash: 23a3312d92a1c4ba0373f1e25277be20ba8bb28c\nhost: x86_64-unknown-linux-gnu\n\
            release: 1.93.0-nightly\nLLVM version: 21.1.0\n";
        let info = parse_toolchain(rustc, b"cargo 1.93.0-nightly\n").unwrap();
        assert_eq!(info.host, "x86_64-unknown-linux-gnu");
        assert!(info.id.starts_with("rust-1.93.0-nightly:"));
        assert_eq!(info.id.len(), "rust-1.93.0-nightly:".len() + 64);
        let other = parse_toolchain(rustc, b"cargo 1.94.0\n").unwrap();
        assert_ne!(info.id, other.id);
        assert!(matches!(
            parse_toolchain(b"garbage", b""),
            Err(BuildError::ToolchainVersion { .. })
        ));
    }

    #[test]
    fn commands_start_from_a_clean_environment() {
        let tools = Tools::new(Path::new("/src"), Path::new("/cargo-home"), Some("stable"));
        let command = tools.command("cargo", Network::Denied);
        let envs: Vec<_> = command
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_owned())))
            .collect();
        let get = |name: &str| {
            envs.iter()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| v.clone())
        };
        assert_eq!(get("CARGO_HOME").unwrap(), "/cargo-home");
        assert_eq!(get("RUSTUP_TOOLCHAIN").unwrap(), "stable");
        assert_eq!(get("CARGO_NET_OFFLINE").unwrap(), "true");
        assert!(get("RUSTFLAGS").is_none());
        assert!(get("HTTPS_PROXY").is_none());
        assert_eq!(command.get_current_dir(), Some(Path::new("/src")));
    }
}
