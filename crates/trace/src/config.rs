//! The boot trace configuration: `trace.toml` in the initfs, which bootstrap applies before
//! `init` starts (feature `config`).
//!
//! The file turns events on and off through the `kernel.trace` scheme as soon as user space
//! exists, so the early boot of user space can be traced without rebuilding the kernel (the
//! kernel's own `trace.boot` default needs a kernel build, and `FERRIC_TRACE=` in the boot
//! environment has to be typed at every boot). `ferric-config` writes it from the
//! `kernel.trace_initfs_*` options, and the base package puts it at [`INITFS_PATH`].
//!
//! ```toml
//! # Selections, as for `enable <selection>` on the scheme's `events` file.
//! enable = ["sched", "scheme.request"]
//! disable = ["sched.switch"]
//! ```
//!
//! Both keys are optional lists of [`select`](crate::select()) selections; any other key is an
//! error, so a misspelt key is not silently ignored. [`BootConfig::commands`] gives the
//! commands to write: every `enable` first, then every `disable`, so `disable` can carve
//! events out of a subsystem `enable` turned on. The format is TOML 1.0, parsed with the
//! `toml` crate, which works without `std`.
//!
//! A file is at most [`MAX_FILE_BYTES`] long and each command at most
//! [`MAX_COMMAND_BYTES`], the limits of bootstrap and of the
//! kernel. [`BootConfig::check_compiled`] also refuses a selection none of whose events the
//! kernel has, which the kernel would refuse.

use alloc::{string::String, string::ToString, vec::Vec};
use core::fmt;

use serde::{Deserialize, Serialize};

use crate::abi::{Command, MAX_COMMAND_BYTES};
use crate::{Catalog, EventSet, select};

/// Where the base package puts the file, relative to the initfs root: bootstrap reads
/// `/scheme/initfs/lib/trace.toml`.
pub const INITFS_PATH: &str = "lib/trace.toml";

/// The longest file, in bytes, that bootstrap reads and [`BootConfig::parse`] accepts.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// A parsed `trace.toml`; see the [module docs](self).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootConfig {
    /// Selections to turn on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,
    /// Selections to turn off, after the `enable`s.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
}

/// Why a `trace.toml` could not be used.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file is longer than [`MAX_FILE_BYTES`].
    TooLarge {
        /// Its length in bytes.
        len: usize,
    },
    /// The file is not UTF-8.
    NotUtf8,
    /// The file is not TOML of this format.
    Parse(toml::de::Error),
    /// A selection names something that is not an event or subsystem of the catalog.
    Unknown {
        /// The selection it is in.
        selection: String,
        /// The unknown name.
        item: String,
    },
    /// A selection's command is longer than [`MAX_COMMAND_BYTES`].
    TooLong {
        /// The selection.
        selection: String,
        /// The length of its command in bytes.
        len: usize,
    },
    /// A selection names events, but none that the kernel has compiled in.
    NotCompiled {
        /// The selection.
        selection: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::TooLarge { len } => write!(
                f,
                "trace.toml is {len} bytes, more than the {MAX_FILE_BYTES} bootstrap reads"
            ),
            ConfigError::NotUtf8 => f.write_str("trace.toml is not UTF-8"),
            ConfigError::Parse(_) => f.write_str("trace.toml is not a valid trace configuration"),
            ConfigError::Unknown { selection, item } => write!(
                f,
                "trace.toml selection {selection:?}: {item:?} is not an event or subsystem"
            ),
            ConfigError::TooLong { selection, len } => write!(
                f,
                "trace.toml selection {selection:?}: its command is {len} bytes, more than the \
                 kernel's {MAX_COMMAND_BYTES}"
            ),
            ConfigError::NotCompiled { selection } => write!(
                f,
                "trace.toml selection {selection:?}: none of its events is compiled into the kernel"
            ),
        }
    }
}

impl core::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            ConfigError::Parse(err) => Some(err),
            _ => None,
        }
    }
}

impl BootConfig {
    /// Parses the bytes of a `trace.toml`.
    ///
    /// # Errors
    ///
    /// [`ConfigError::TooLarge`] past [`MAX_FILE_BYTES`], [`ConfigError::NotUtf8`], or
    /// [`ConfigError::Parse`] when the text is not TOML with only the keys of this format.
    /// Selections are not checked; see [`BootConfig::check`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(ConfigError::TooLarge { len: bytes.len() });
        }
        let text = core::str::from_utf8(bytes).map_err(|_| ConfigError::NotUtf8)?;
        toml::from_str(text).map_err(ConfigError::Parse)
    }

    /// Checks that every selection names events or subsystems of `catalog` and that its
    /// command fits in [`MAX_COMMAND_BYTES`], as the kernel will when the commands are written,
    /// and returns the events left on: those the `enable`s select, less those the `disable`s
    /// select.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Unknown`] or [`ConfigError::TooLong`] for the first selection that has
    /// an unknown name or is too long.
    pub fn check(&self, catalog: &Catalog) -> Result<EventSet, ConfigError> {
        for command in self.commands() {
            let len = command.to_string().len();
            if len > MAX_COMMAND_BYTES {
                let (Command::Enable(selection) | Command::Disable(selection)) = command;
                return Err(ConfigError::TooLong {
                    selection: selection.into(),
                    len,
                });
            }
        }
        let mut on = selected(catalog, &self.enable)?;
        for id in selected(catalog, &self.disable)?.iter() {
            on.remove(id);
        }
        Ok(on)
    }

    /// [`BootConfig::check`], and also that every selection names at least one event in
    /// `compiled`, the events the kernel has (the kernel refuses a selection with none). A
    /// selection with some events that are not compiled in is accepted, as the kernel accepts
    /// it. Returns the compiled-in events left on.
    ///
    /// # Errors
    ///
    /// Those of [`BootConfig::check`], and [`ConfigError::NotCompiled`] for the first selection
    /// with no compiled-in event.
    pub fn check_compiled(
        &self,
        catalog: &Catalog,
        compiled: &EventSet,
    ) -> Result<EventSet, ConfigError> {
        let on = self.check(catalog)?;
        for selection in self.enable.iter().chain(&self.disable) {
            let events = selected(catalog, core::slice::from_ref(selection))?;
            if !events.is_empty() && events.intersection(compiled).is_empty() {
                return Err(ConfigError::NotCompiled {
                    selection: selection.clone(),
                });
            }
        }
        Ok(on.intersection(compiled))
    }

    /// The events the `disable`s select. They are off once the commands are written, since
    /// every `disable` comes after every `enable`.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Unknown`] for the first `disable` with an unknown name.
    pub fn disabled(&self, catalog: &Catalog) -> Result<EventSet, ConfigError> {
        selected(catalog, &self.disable)
    }

    /// The commands to write to the scheme's `events` file, in order: one per selection,
    /// every `enable` before every `disable`.
    pub fn commands(&self) -> impl Iterator<Item = Command<'_>> {
        let enable = self.enable.iter().map(|s| Command::Enable(s));
        let disable = self.disable.iter().map(|s| Command::Disable(s));
        enable.chain(disable)
    }

    /// Whether the file would change nothing.
    pub fn is_empty(&self) -> bool {
        self.enable.is_empty() && self.disable.is_empty()
    }
}

/// The union of the events `selections` select in `catalog`.
fn selected(catalog: &Catalog, selections: &[String]) -> Result<EventSet, ConfigError> {
    selections
        .iter()
        .try_fold(EventSet::new(), |set, selection| {
            select(catalog, selection)
                .map(|events| set.union(&events))
                .map_err(|err| ConfigError::Unknown {
                    selection: selection.clone(),
                    item: err.item().into(),
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::{CATALOG, SCHED_SWITCH, SCHEME_CANCEL, SYSCALL_ENTER, SYSCALL_EXIT};
    use std::string::ToString;
    use std::{format, vec};

    #[test]
    fn parses_both_keys() {
        let config = BootConfig::parse(
            b"# comment\nenable = [\"syscall\", \"sched.switch\"]\ndisable = [\"syscall.exit\"]\n",
        )
        .expect("valid");
        assert_eq!(config.enable, ["syscall", "sched.switch"]);
        assert_eq!(config.disable, ["syscall.exit"]);
        let commands: Vec<_> = config.commands().map(|c| c.to_string()).collect();
        assert_eq!(
            commands,
            [
                "enable syscall",
                "enable sched.switch",
                "disable syscall.exit"
            ]
        );
        let on = config.check(&CATALOG).expect("known names");
        assert!(on.contains(SYSCALL_ENTER.id()));
        assert!(on.contains(SCHED_SWITCH.id()));
        assert!(!on.contains(SYSCALL_EXIT.id()));
        assert_eq!(on.len(), 2);
    }

    #[test]
    fn empty_and_partial_files() {
        let empty = BootConfig::parse(b"").expect("valid");
        assert!(empty.is_empty());
        assert_eq!(empty.commands().count(), 0);
        let only_disable = BootConfig::parse(b"disable = [\"all\"]").expect("valid");
        assert!(only_disable.enable.is_empty());
        assert!(only_disable.check(&CATALOG).expect("known").is_empty());
    }

    #[test]
    fn refuses_other_files() {
        assert!(matches!(
            BootConfig::parse(b"enabled = [\"syscall\"]"),
            Err(ConfigError::Parse(_))
        ));
        assert!(matches!(
            BootConfig::parse(b"enable = \"syscall\""),
            Err(ConfigError::Parse(_))
        ));
        assert!(matches!(
            BootConfig::parse(b"enable = ["),
            Err(ConfigError::Parse(_))
        ));
        assert!(matches!(
            BootConfig::parse(b"\xff"),
            Err(ConfigError::NotUtf8)
        ));
    }

    #[test]
    fn unknown_names_are_reported() {
        let config = BootConfig {
            enable: vec!["syscall, sched.swich".into()],
            disable: vec![],
        };
        match config.check(&CATALOG) {
            Err(ConfigError::Unknown { selection, item }) => {
                assert_eq!(selection, "syscall, sched.swich");
                assert_eq!(item, "sched.swich");
            }
            other => panic!("expected an unknown name, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let config = BootConfig {
            enable: vec!["scheme".into(), "sched.switch".into()],
            disable: vec!["scheme.cancel".into()],
        };
        let text = toml::to_string(&config).expect("serializable");
        assert_eq!(BootConfig::parse(text.as_bytes()).expect("valid"), config);
        assert_eq!(
            toml::to_string(&BootConfig::default()).expect("serializable"),
            ""
        );
    }

    #[test]
    fn refuses_oversize_files_and_commands() {
        let big = vec![b' '; MAX_FILE_BYTES + 1];
        assert!(matches!(
            BootConfig::parse(&big),
            Err(ConfigError::TooLarge { len }) if len == MAX_FILE_BYTES + 1
        ));
        assert!(BootConfig::parse(&big[..MAX_FILE_BYTES]).is_ok());
        // "enable " plus the selection: exactly at the limit is fine, one byte more is not.
        let fits = "syscall,".repeat((MAX_COMMAND_BYTES - 7) / 8);
        let fits = format!("{fits}{}", " ".repeat(MAX_COMMAND_BYTES - 7 - fits.len()));
        let config = BootConfig {
            enable: vec![fits.clone()],
            disable: vec![],
        };
        assert!(config.check(&CATALOG).is_ok());
        let config = BootConfig {
            enable: vec![],
            disable: vec![format!("{fits} ")],
        };
        match config.check(&CATALOG) {
            Err(ConfigError::TooLong { len, .. }) => assert_eq!(len, MAX_COMMAND_BYTES + 2),
            other => panic!("expected a long command, got {other:?}"),
        }
    }

    #[test]
    fn selections_need_a_compiled_event() {
        // A kernel without the scheme subsystem.
        let compiled = CATALOG.compiled_for(|f| f != "trace_scheme" && f != "trace_level_debug");
        assert!(!compiled.contains(SCHEME_CANCEL.id()));
        assert!(compiled.contains(SCHED_SWITCH.id()));
        let only_scheme = BootConfig {
            enable: vec!["sched.switch".into()],
            disable: vec!["scheme.cancel".into()],
        };
        match only_scheme.check_compiled(&CATALOG, &compiled) {
            Err(ConfigError::NotCompiled { selection }) => assert_eq!(selection, "scheme.cancel"),
            other => panic!("expected an uncompiled selection, got {other:?}"),
        }
        // A selection with some compiled events is kept, and only those are reported on.
        let mixed = BootConfig {
            enable: vec!["scheme.cancel sched.switch".into()],
            disable: vec![],
        };
        let on = mixed.check_compiled(&CATALOG, &compiled).expect("mixed");
        assert!(on.contains(SCHED_SWITCH.id()));
        assert_eq!(on.len(), 1);
        // The empty selection selects nothing, which the kernel accepts.
        let empty = BootConfig {
            enable: vec![String::new()],
            disable: vec![],
        };
        assert!(empty.check_compiled(&CATALOG, &compiled).is_ok());
    }

    #[test]
    fn disabled_events() {
        let config = BootConfig {
            enable: vec!["syscall".into()],
            disable: vec!["syscall.exit".into(), "sched".into()],
        };
        let off = config.disabled(&CATALOG).expect("known");
        assert!(off.contains(SYSCALL_EXIT.id()));
        assert!(off.contains(SCHED_SWITCH.id()));
        assert!(!off.contains(SYSCALL_ENTER.id()));
    }
}
