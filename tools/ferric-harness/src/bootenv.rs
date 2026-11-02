//! The boot environment the harness gives the bootloader.
//!
//! The bootloader passes a block of `KEY=VALUE` lines to the kernel; the harness adds its
//! own lines in one of two ways:
//!
//! - **fw_cfg** (default): the lines are written to `<run>/boot-env` and QEMU exposes the
//!   file to the guest as the fw_cfg file [`FW_CFG_FILE`]. A bootloader built with
//!   `bootloader.fw_cfg_env` merges it before the resolution menu, and with
//!   `FERRIC_BOOT_RESOLUTION=auto` skips the menu. Older bootloaders ignore it.
//! - **keys**: the harness answers the resolution menu with `e`, types each line into the
//!   boot-environment editor followed by Enter, and boots with Enter on the empty line. This
//!   works with any bootloader that has a display, and needs one.
//!
//! `--mode trace` adds `FERRIC_KERNEL=trace` (load `usr/lib/boot/kernel-trace`) and
//! `FERRIC_TRACE_HOST=<KiB per CPU>` (allocate host-visible rings).
//!
//! The key names, the fw_cfg file name, the size limit and the key rule are those of
//! [`ferric_abi::boot_env`], the definitions the bootloader and kernel use.

use ferric_abi::boot_env::{
    self, FERRIC_BOOT_RESOLUTION, FERRIC_KERNEL, FERRIC_TRACE_HOST, Kernel, MAX_PRESET_BYTES,
    Resolution,
};
use thiserror::Error;

use crate::machine::{Display, Mode};

pub use ferric_abi::boot_env::FW_CFG_FILE;

/// How the boot environment reaches the bootloader.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum BootEnvVia {
    /// A fw_cfg file the bootloader reads (bootloaders built with `bootloader.fw_cfg_env`).
    #[default]
    FwCfg,
    /// Typed into the resolution menu's `e` editor (any bootloader, display required).
    Keys,
}

/// A boot environment entry or file the bootloader would not accept.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BootEnvError {
    /// An assignment without `=`.
    #[error("boot environment entry {0:?} is not KEY=VALUE")]
    NotAssignment(String),
    /// A key with characters other than letters, digits and `_`, or starting with a digit.
    #[error("boot environment key {0:?} must be letters, digits and _ (not starting with a digit)")]
    Key(String),
    /// A value with a line break or NUL.
    #[error("the value of boot environment key {0} contains a line break or NUL")]
    Value(String),
    /// The file would be larger than the bootloader reads.
    #[error("the boot environment is {bytes} bytes; the bootloader reads at most {max}")]
    TooLarge {
        /// Size of the rendered file.
        bytes: usize,
        /// The limit.
        max: usize,
    },
    /// `FERRIC_TRACE_HOST` must be a power of two.
    #[error("--trace-ring-kib {0} is not a power of two")]
    RingSize(u64),
    /// The user set `FERRIC_KERNEL` to something other than the mode.
    #[error("--boot-env FERRIC_KERNEL={value} contradicts --mode {mode}")]
    KernelConflict {
        /// The value given.
        value: String,
        /// The mode given.
        mode: Mode,
    },
}

/// An ordered set of `KEY=VALUE` lines; setting a key again replaces its value in place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootEnv {
    entries: Vec<(String, String)>,
}

impl BootEnv {
    /// The environment for a run: the mode's keys, `FERRIC_BOOT_RESOLUTION=auto` when the
    /// run has a display and uses fw_cfg, then the user's entries.
    ///
    /// # Errors
    ///
    /// Any [`BootEnvError`]: a malformed entry, a ring size that is not a power of two, a
    /// `FERRIC_KERNEL` that contradicts the mode, or a file over
    /// [`MAX_PRESET_BYTES`].
    pub fn for_run(
        user: &[(String, String)],
        mode: Mode,
        trace_ring_kib: u64,
        display: Display,
        via: BootEnvVia,
    ) -> Result<Self, BootEnvError> {
        let mut env = Self::default();
        if mode == Mode::Trace {
            if !trace_ring_kib.is_power_of_two() {
                return Err(BootEnvError::RingSize(trace_ring_kib));
            }
            env.set(FERRIC_KERNEL, Kernel::Trace.name())?;
            env.set(FERRIC_TRACE_HOST, &trace_ring_kib.to_string())?;
        }
        if display == Display::Std && via == BootEnvVia::FwCfg {
            env.set(FERRIC_BOOT_RESOLUTION, Resolution::Auto.name())?;
        }
        for (key, value) in user {
            if key == FERRIC_KERNEL && Kernel::parse(value.as_bytes()) != Ok(mode.kernel()) {
                return Err(BootEnvError::KernelConflict {
                    value: value.clone(),
                    mode,
                });
            }
            env.set(key, value)?;
        }
        let bytes = env.render().len();
        if bytes > MAX_PRESET_BYTES {
            return Err(BootEnvError::TooLarge {
                bytes,
                max: MAX_PRESET_BYTES,
            });
        }
        Ok(env)
    }

    /// Sets `key` to `value`.
    ///
    /// # Errors
    ///
    /// [`BootEnvError::Key`] or [`BootEnvError::Value`] for a malformed key or value.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), BootEnvError> {
        validate(key, value)?;
        match self.entries.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value.to_owned(),
            None => self.entries.push((key.to_owned(), value.to_owned())),
        }
        Ok(())
    }

    /// The value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `KEY=VALUE` lines.
    pub fn lines(&self) -> impl Iterator<Item = String> + '_ {
        self.entries.iter().map(|(k, v)| format!("{k}={v}"))
    }

    /// The lines to type into the boot-environment editor: every entry except
    /// `FERRIC_BOOT_RESOLUTION`, which only matters before the menu that typing needs.
    pub fn typed_lines(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(k, _)| k != FERRIC_BOOT_RESOLUTION)
            .map(|(k, v)| format!("{k}={v}"))
            .collect()
    }

    /// The fw_cfg file: one `KEY=VALUE` line per entry.
    pub fn render(&self) -> String {
        self.lines().map(|line| line + "\n").collect()
    }
}

/// Parses a `KEY=VALUE` command-line argument.
///
/// # Errors
///
/// [`BootEnvError::NotAssignment`], [`BootEnvError::Key`] or [`BootEnvError::Value`].
pub fn parse_assignment(text: &str) -> Result<(String, String), BootEnvError> {
    let (key, value) = text
        .split_once('=')
        .ok_or_else(|| BootEnvError::NotAssignment(text.to_owned()))?;
    validate(key, value)?;
    Ok((key.to_owned(), value.to_owned()))
}

/// Checks one entry with the rule the bootloader and kernel read the environment by
/// ([`boot_env::Line::parse`]); a value must also stay on its line.
fn validate(key: &str, value: &str) -> Result<(), BootEnvError> {
    if value.contains(['\n', '\r', '\0']) {
        return Err(BootEnvError::Value(key.to_owned()));
    }
    let line = format!("{key}={value}");
    match boot_env::Line::parse(line.as_bytes()) {
        boot_env::Line::Assignment(entry) if entry.key() == key => Ok(()),
        _ => Err(BootEnvError::Key(key.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    #[test]
    fn trace_mode_keys() {
        let env = BootEnv::for_run(
            &[kv("FERRIC_TRACE", "sched.switch")],
            Mode::Trace,
            4096,
            Display::Std,
            BootEnvVia::FwCfg,
        )
        .expect("valid");
        assert_eq!(
            env.render(),
            "FERRIC_KERNEL=trace\nFERRIC_TRACE_HOST=4096\nFERRIC_BOOT_RESOLUTION=auto\nFERRIC_TRACE=sched.switch\n"
        );
        assert_eq!(
            env.typed_lines(),
            [
                "FERRIC_KERNEL=trace",
                "FERRIC_TRACE_HOST=4096",
                "FERRIC_TRACE=sched.switch"
            ]
        );
    }

    #[test]
    fn normal_headless_is_empty() {
        let env = BootEnv::for_run(&[], Mode::Normal, 4096, Display::None, BootEnvVia::FwCfg)
            .expect("valid");
        assert!(env.is_empty());
        let keys = BootEnv::for_run(&[], Mode::Normal, 4096, Display::Std, BootEnvVia::Keys)
            .expect("valid");
        assert!(keys.is_empty(), "the menu is answered anyway when typing");
    }

    #[test]
    fn user_entries_override_in_place() {
        let env = BootEnv::for_run(
            &[
                kv("FERRIC_BOOT_RESOLUTION", "1024x768"),
                kv("A", "1"),
                kv("A", "2"),
            ],
            Mode::Normal,
            4096,
            Display::Std,
            BootEnvVia::FwCfg,
        )
        .expect("valid");
        assert_eq!(env.render(), "FERRIC_BOOT_RESOLUTION=1024x768\nA=2\n");
        assert_eq!(env.get("A"), Some("2"));
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            parse_assignment("NOEQUALS"),
            Err(BootEnvError::NotAssignment("NOEQUALS".into()))
        );
        assert_eq!(
            parse_assignment("1A=b"),
            Err(BootEnvError::Key("1A".into()))
        );
        assert_eq!(
            parse_assignment("A-B=c"),
            Err(BootEnvError::Key("A-B".into()))
        );
        assert_eq!(parse_assignment("A=b=c"), Ok(kv("A", "b=c")));
        assert!(matches!(
            BootEnv::default().set("A", "x\ny"),
            Err(BootEnvError::Value(_))
        ));
        assert_eq!(
            BootEnv::for_run(&[], Mode::Trace, 3000, Display::None, BootEnvVia::FwCfg),
            Err(BootEnvError::RingSize(3000))
        );
        assert!(matches!(
            BootEnv::for_run(
                &[kv("FERRIC_KERNEL", "normal")],
                Mode::Trace,
                4096,
                Display::None,
                BootEnvVia::FwCfg
            ),
            Err(BootEnvError::KernelConflict { .. })
        ));
        // The kernel is named as the bootloader reads it: blanks around it do not matter,
        // and a name it does not know contradicts any mode.
        assert!(
            BootEnv::for_run(
                &[kv("FERRIC_KERNEL", " trace ")],
                Mode::Trace,
                4096,
                Display::None,
                BootEnvVia::FwCfg
            )
            .is_ok()
        );
        assert!(matches!(
            BootEnv::for_run(
                &[kv("FERRIC_KERNEL", "debug")],
                Mode::Normal,
                4096,
                Display::None,
                BootEnvVia::FwCfg
            ),
            Err(BootEnvError::KernelConflict { .. })
        ));
        let big = vec![kv("BIG", &"x".repeat(5000))];
        assert!(matches!(
            BootEnv::for_run(&big, Mode::Normal, 4096, Display::None, BootEnvVia::FwCfg),
            Err(BootEnvError::TooLarge { .. })
        ));
    }
}
