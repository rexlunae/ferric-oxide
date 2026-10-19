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

use alloc::{string::String, vec::Vec};
use core::fmt;

use serde::{Deserialize, Serialize};

use crate::abi::Command;
use crate::{Catalog, EventSet, select};

/// Where the base package puts the file, relative to the initfs root: bootstrap reads
/// `/scheme/initfs/lib/trace.toml`.
pub const INITFS_PATH: &str = "lib/trace.toml";

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
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NotUtf8 => f.write_str("trace.toml is not UTF-8"),
            ConfigError::Parse(_) => f.write_str("trace.toml is not a valid trace configuration"),
            ConfigError::Unknown { selection, item } => write!(
                f,
                "trace.toml selection {selection:?}: {item:?} is not an event or subsystem"
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
    /// [`ConfigError::NotUtf8`], or [`ConfigError::Parse`] when the text is not TOML with
    /// only the keys of this format. Selections are not checked; see [`BootConfig::check`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
        let text = core::str::from_utf8(bytes).map_err(|_| ConfigError::NotUtf8)?;
        toml::from_str(text).map_err(ConfigError::Parse)
    }

    /// Checks that every selection names events or subsystems of `catalog`, as the kernel
    /// will when the commands are written, and returns the events left on: those the
    /// `enable`s select, less those the `disable`s select.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Unknown`] for the first selection with an unknown name.
    pub fn check(&self, catalog: &Catalog) -> Result<EventSet, ConfigError> {
        let set = |selections: &[String]| {
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
        };
        let mut on = set(&self.enable)?;
        for id in set(&self.disable)?.iter() {
            on.remove(id);
        }
        Ok(on)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::{CATALOG, SCHED_SWITCH, SYSCALL_ENTER, SYSCALL_EXIT};
    use std::string::ToString;
    use std::vec;

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
}
