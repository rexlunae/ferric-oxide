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
//!   works with any bootloader that has a display, and needs one. Every typed character must
//!   be a printable character with a US-layout key ([`input::char_keys`], the mapping the
//!   harness types with): anything else is refused before the guest boots
//!   ([`BootEnvError::Untypeable`]) rather than leaving the guest in the editor.
//!
//! `--mode trace` adds `FERRIC_KERNEL=trace` (load `usr/lib/boot/kernel-trace`) and
//! `FERRIC_TRACE_HOST=<KiB per CPU>` (allocate host-visible rings). A `FERRIC_TRACE_HOST`
//! value, the mode's or the user's, must be a size the kernel accepts
//! ([`RingSize::parse`], a power of two from 4 to 1048576 KiB): the kernel would otherwise
//! boot without host rings.
//!
//! The key names, the fw_cfg file name, the size limit and the key rule are those of
//! [`ferric_abi::boot_env`], the definitions the bootloader and kernel use.

use ferric_abi::boot_env::{
    self, FERRIC_BOOT_RESOLUTION, FERRIC_KERNEL, FERRIC_TRACE_HOST, Kernel, MAX_PRESET_BYTES,
    Resolution,
};
use ferric_trace::host::{RingSize, RingSizeError};
use thiserror::Error;

use crate::input;
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
    /// A `FERRIC_TRACE_HOST` value (from `--trace-ring-kib` or `--boot-env`) that is not a
    /// ring size the kernel accepts.
    #[error("FERRIC_TRACE_HOST={value} is not a host ring size the kernel accepts")]
    RingSize {
        /// The value given.
        value: String,
        /// Why the kernel would refuse it.
        #[source]
        source: RingSizeError,
    },
    /// The user set `FERRIC_KERNEL` to something other than the mode.
    #[error("--boot-env FERRIC_KERNEL={value} contradicts --mode {mode}")]
    KernelConflict {
        /// The value given.
        value: String,
        /// The mode given.
        mode: Mode,
    },
    /// With `--boot-env-via keys`, an entry with a character the harness cannot type into
    /// the bootloader's editor: one without a US-layout key, or a control character the
    /// editor would act on instead of inserting.
    #[error(
        "boot environment entry {key} contains {character:?}, which cannot be typed into the bootloader editor (printable US-layout characters only); use --boot-env-via fw-cfg"
    )]
    Untypeable {
        /// The entry's key.
        key: String,
        /// The first character that cannot be typed.
        character: char,
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
    /// Any [`BootEnvError`]: a malformed entry, a `FERRIC_TRACE_HOST` the kernel would
    /// refuse, a `FERRIC_KERNEL` that contradicts the mode, a file over
    /// [`MAX_PRESET_BYTES`], or, when `via` is [`BootEnvVia::Keys`], an entry that cannot be
    /// typed ([`BootEnvError::Untypeable`]).
    pub fn for_run(
        user: &[(String, String)],
        mode: Mode,
        trace_ring_kib: u32,
        display: Display,
        via: BootEnvVia,
    ) -> Result<Self, BootEnvError> {
        let mut env = Self::default();
        if mode == Mode::Trace {
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
        if let Some(value) = env.get(FERRIC_TRACE_HOST) {
            RingSize::parse(value).map_err(|source| BootEnvError::RingSize {
                value: value.to_owned(),
                source,
            })?;
        }
        let bytes = env.render().len();
        if bytes > MAX_PRESET_BYTES {
            return Err(BootEnvError::TooLarge {
                bytes,
                max: MAX_PRESET_BYTES,
            });
        }
        if via == BootEnvVia::Keys {
            env.check_typeable()?;
        }
        Ok(env)
    }

    /// Checks that every line [`typed_lines`](Self::typed_lines) gives can be typed.
    fn check_typeable(&self) -> Result<(), BootEnvError> {
        for (key, value) in self
            .entries
            .iter()
            .filter(|(k, _)| k != FERRIC_BOOT_RESOLUTION)
        {
            let line = format!("{key}={value}");
            if let Some(character) = line.chars().find(|&c| !typeable(c)) {
                return Err(BootEnvError::Untypeable {
                    key: key.clone(),
                    character,
                });
            }
        }
        Ok(())
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

/// Parses `--trace-ring-kib`: a host ring size the kernel accepts, in KiB.
///
/// # Errors
///
/// [`RingSizeError`] as [`RingSize::parse`] gives it.
pub fn parse_ring_kib(text: &str) -> Result<u32, RingSizeError> {
    RingSize::parse(text).map(RingSize::kib)
}

/// Whether the editor receives `c` as itself when the harness types it: a printable
/// character with a key in the mapping [`input::type_events`] uses.
fn typeable(c: char) -> bool {
    !c.is_control() && input::char_keys(c).is_ok()
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

    /// The ring sizes are the kernel's: a power of two from 4 KiB to 1 GiB, whether they
    /// come from `--trace-ring-kib` or a `FERRIC_TRACE_HOST` entry.
    #[test]
    fn ring_sizes_the_kernel_accepts() {
        let trace = |kib, user: &[(String, String)]| {
            BootEnv::for_run(user, Mode::Trace, kib, Display::None, BootEnvVia::FwCfg)
        };
        for kib in [4, 4096, 1 << 20] {
            assert!(trace(kib, &[]).is_ok(), "{kib} KiB");
        }
        for (kib, source) in [
            (1, RingSizeError::TooSmall { kib: 1 }),
            (2, RingSizeError::TooSmall { kib: 2 }),
            (3000, RingSizeError::NotPowerOfTwo { kib: 3000 }),
            (1 << 21, RingSizeError::TooLarge { kib: 1 << 21 }),
        ] {
            assert_eq!(
                trace(kib, &[]),
                Err(BootEnvError::RingSize {
                    value: kib.to_string(),
                    source
                })
            );
        }
        // A user's entry replaces the mode's and is held to the same rule, in any mode.
        assert_eq!(
            trace(4096, &[kv("FERRIC_TRACE_HOST", "4M")]),
            Err(BootEnvError::RingSize {
                value: "4M".into(),
                source: RingSizeError::NotNumber
            })
        );
        assert!(matches!(
            BootEnv::for_run(
                &[kv("FERRIC_TRACE_HOST", "2")],
                Mode::Normal,
                4096,
                Display::None,
                BootEnvVia::FwCfg
            ),
            Err(BootEnvError::RingSize { .. })
        ));
        let env = trace(4096, &[kv("FERRIC_TRACE_HOST", "65536")]).expect("valid");
        assert_eq!(env.get("FERRIC_TRACE_HOST"), Some("65536"));
        assert_eq!(parse_ring_kib("65536"), Ok(65536));
        assert_eq!(parse_ring_kib("1"), Err(RingSizeError::TooSmall { kib: 1 }));
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

    /// Typed entries must be printable US-layout text; fw_cfg carries any valid value.
    #[test]
    fn typed_entries_must_be_typeable() {
        let via = |user: &[(String, String)], via| {
            BootEnv::for_run(user, Mode::Normal, 4096, Display::Std, via)
        };
        for (value, character) in [
            ("caf\u{e9}", '\u{e9}'),
            ("a\tb", '\t'),
            ("x\u{1b}", '\u{1b}'),
        ] {
            assert_eq!(
                via(&[kv("A", "ok"), kv("PROBE", value)], BootEnvVia::Keys),
                Err(BootEnvError::Untypeable {
                    key: "PROBE".into(),
                    character
                }),
                "{value:?}"
            );
            assert!(
                via(&[kv("PROBE", value)], BootEnvVia::FwCfg).is_ok(),
                "{value:?}"
            );
        }
        let printable: String = (' '..='~').collect();
        let env = via(&[kv("ALL", &printable)], BootEnvVia::Keys).expect("printable ASCII");
        for line in env.typed_lines() {
            input::type_events(&line).expect("the session types every accepted line");
        }
        // FERRIC_BOOT_RESOLUTION is not typed, so it is not checked.
        assert!(via(&[kv("FERRIC_BOOT_RESOLUTION", "\u{e9}")], BootEnvVia::Keys).is_ok());
    }
}
