//! The `kernel.trace` scheme's interface.
//!
//! The scheme is open to root only for now; a Phase 4 capability replaces the check, because
//! traces leak timing and kernel addresses.
//!
//! | Path | Read | Write |
//! | --- | --- | --- |
//! | [`EVENTS_PATH`] | one [`EventLine`] per event | a [`Command`]: `enable <selection>` or `disable <selection>` |
//! | [`STATS_PATH`] | one [`StatsLine`] per CPU | — |
//! | `cpu/<n>` ([`Path::Cpu`]) | drains whole records from CPU n's ring, in the [`record`](crate::record) format; 0 bytes when it is empty | — |
//!
//! A selection is the syntax of [`select`](crate::select()). Lines end with `\n` and their fields
//! are separated by single spaces.

use core::fmt;
use core::str::FromStr;

use crate::{Event, EventId, Level};

/// The scheme's number among the kernel's global schemes (`GlobalSchemes` in the kernel's
/// `redox_syscall`), which bootstrap uses to find it among the kernel scheme capabilities.
pub const SCHEME_ID: u8 = 14;

/// The scheme's name in the namespace: clients open `/scheme/kernel.trace/<path>`.
pub const SCHEME_NAME: &str = "kernel.trace";

/// The event list and control file.
pub const EVENTS_PATH: &str = "events";

/// The per-CPU statistics file.
pub const STATS_PATH: &str = "stats";

/// The directory of per-CPU record files, `cpu/<n>`.
///
/// A read of `cpu/<n>` returns whole records ([`crate::record`]) and consumes them; 0 bytes means
/// the ring is empty. A buffer too small for the next record gets `EINVAL` and the record stays:
/// a buffer of [`READ_MIN`] bytes always holds one. While another reader is reading the same
/// CPU, a read gets `EAGAIN`.
pub const CPU_DIR: &str = "cpu";

/// The smallest buffer a read of `cpu/<n>` always succeeds with: the longest record.
pub const READ_MIN: usize = crate::record::MAX_RECORD_WORDS * crate::record::WORD_BYTES;

/// A path in the scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// The scheme root.
    Root,
    /// [`EVENTS_PATH`].
    Events,
    /// [`STATS_PATH`].
    Stats,
    /// `cpu/<n>`: the records of CPU `n`.
    Cpu(u32),
}

impl Path {
    /// The path `path` names, ignoring slashes at either end, or `None` for any other path.
    pub fn parse(path: &str) -> Option<Self> {
        match path.trim_matches('/') {
            "" => Some(Path::Root),
            EVENTS_PATH => Some(Path::Events),
            STATS_PATH => Some(Path::Stats),
            other => {
                let n = other.strip_prefix(CPU_DIR)?.strip_prefix('/')?;
                // `u32::from_str` takes a leading `+`; a path does not.
                if !n.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                n.parse().ok().map(Path::Cpu)
            }
        }
    }
}

/// A command written to [`EVENTS_PATH`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    /// Turn on the events of a selection.
    Enable(&'a str),
    /// Turn off the events of a selection.
    Disable(&'a str),
}

/// A write to [`EVENTS_PATH`] that is not a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandError {
    /// The text is not UTF-8.
    NotUtf8,
    /// The first word is not `enable` or `disable`.
    UnknownVerb,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CommandError::NotUtf8 => "trace command is not UTF-8",
            CommandError::UnknownVerb => "trace command must start with enable or disable",
        })
    }
}

impl core::error::Error for CommandError {}

impl<'a> Command<'a> {
    /// Parses a command, ignoring surrounding whitespace.
    ///
    /// # Errors
    ///
    /// [`CommandError::NotUtf8`] or [`CommandError::UnknownVerb`].
    pub fn parse(bytes: &'a [u8]) -> Result<Self, CommandError> {
        let text = core::str::from_utf8(bytes).map_err(|_| CommandError::NotUtf8)?;
        let text = text.trim();
        let (verb, rest) = text
            .split_once(|c: char| c.is_ascii_whitespace())
            .unwrap_or((text, ""));
        match verb {
            "enable" => Ok(Command::Enable(rest.trim())),
            "disable" => Ok(Command::Disable(rest.trim())),
            _ => Err(CommandError::UnknownVerb),
        }
    }
}

impl fmt::Display for Command<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Enable(spec) => write!(f, "enable {spec}"),
            Command::Disable(spec) => write!(f, "disable {spec}"),
        }
    }
}

/// A line that does not have the format it should.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineError {
    /// The line has fewer or more fields than the format.
    Fields,
    /// A field that should be a number is not one.
    Number,
    /// The level field is not a level name.
    Level,
    /// The state field is neither `on` nor `off`.
    State,
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LineError::Fields => "wrong number of fields",
            LineError::Number => "a field is not a number",
            LineError::Level => "unknown level",
            LineError::State => "state is not on or off",
        })
    }
}

impl core::error::Error for LineError {}

/// The fields of `line` split at spaces, which must be exactly `N` of them.
fn fields<const N: usize>(line: &str) -> Result<[&str; N], LineError> {
    let mut out = [""; N];
    let mut parts = line.trim_end_matches('\n').split(' ');
    for slot in &mut out {
        *slot = parts.next().ok_or(LineError::Fields)?;
    }
    match parts.next() {
        Some(_) => Err(LineError::Fields),
        None => Ok(out),
    }
}

/// A number field.
fn number<T: FromStr>(field: &str) -> Result<T, LineError> {
    field.parse().map_err(|_| LineError::Number)
}

/// One line of the [`EVENTS_PATH`] listing: `<id> <name> <subsystem> <level> <on|off>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLine<'a> {
    /// The event's id.
    pub id: EventId,
    /// The event's name.
    pub name: &'a str,
    /// The event's subsystem name.
    pub subsystem: &'a str,
    /// The event's level.
    pub level: Level,
    /// Whether the event is on.
    pub enabled: bool,
}

impl<'a> EventLine<'a> {
    /// The line for `event`.
    pub fn new(event: &'a Event, enabled: bool) -> Self {
        Self {
            id: event.id(),
            name: event.name(),
            subsystem: event.subsystem().name(),
            level: event.level(),
            enabled,
        }
    }

    /// Parses a line, with or without its `\n`.
    ///
    /// # Errors
    ///
    /// A [`LineError`] saying which field is wrong.
    pub fn parse(line: &'a str) -> Result<Self, LineError> {
        let [id, name, subsystem, level, state] = fields(line)?;
        Ok(Self {
            id: EventId::new(number(id)?),
            name,
            subsystem,
            level: level.parse().map_err(|_| LineError::Level)?,
            enabled: match state {
                "on" => true,
                "off" => false,
                _ => return Err(LineError::State),
            },
        })
    }
}

impl fmt::Display for EventLine<'_> {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} {} {} {} {}",
            self.id,
            self.name,
            self.subsystem,
            self.level,
            if self.enabled { "on" } else { "off" }
        )
    }
}

/// One line of the [`STATS_PATH`] file: `cpu <n> pending <words> lost <records>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatsLine {
    /// The CPU.
    pub cpu: u32,
    /// Words written to its ring and not read yet.
    pub pending: u64,
    /// Records dropped on it: ring full, or no ring yet.
    pub lost: u64,
}

impl StatsLine {
    /// Parses a line, with or without its `\n`.
    ///
    /// # Errors
    ///
    /// [`LineError::Fields`] or [`LineError::Number`].
    pub fn parse(line: &str) -> Result<Self, LineError> {
        let [cpu_tag, cpu, pending_tag, pending, lost_tag, lost] = fields(line)?;
        if (cpu_tag, pending_tag, lost_tag) != ("cpu", "pending", "lost") {
            return Err(LineError::Fields);
        }
        Ok(Self {
            cpu: number(cpu)?,
            pending: number(pending)?,
            lost: number(lost)?,
        })
    }
}

impl fmt::Display for StatsLine {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "cpu {} pending {} lost {}",
            self.cpu, self.pending, self.lost
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::SCHED_SWITCH;
    use std::string::ToString;

    #[test]
    fn paths() {
        assert_eq!(Path::parse(""), Some(Path::Root));
        assert_eq!(Path::parse("/"), Some(Path::Root));
        assert_eq!(Path::parse("events"), Some(Path::Events));
        assert_eq!(Path::parse("/stats/"), Some(Path::Stats));
        assert_eq!(Path::parse("cpu/0"), Some(Path::Cpu(0)));
        assert_eq!(Path::parse("cpu/17"), Some(Path::Cpu(17)));
        assert_eq!(Path::parse("cpu"), None);
        assert_eq!(Path::parse("cpu/"), None);
        assert_eq!(Path::parse("cpu/+1"), None);
        assert_eq!(Path::parse("cpu/x"), None);
        assert_eq!(Path::parse("cpus/1"), None);
        assert_eq!(Path::parse("other"), None);
    }

    #[test]
    fn commands() {
        assert_eq!(
            Command::parse(b"enable syscall\n"),
            Ok(Command::Enable("syscall"))
        );
        assert_eq!(
            Command::parse(b"  disable  sched.switch, syscall "),
            Ok(Command::Disable("sched.switch, syscall"))
        );
        assert_eq!(Command::parse(b"enable"), Ok(Command::Enable("")));
        assert_eq!(
            Command::parse(b"toggle all"),
            Err(CommandError::UnknownVerb)
        );
        assert_eq!(Command::parse(b"\xff"), Err(CommandError::NotUtf8));
        assert_eq!(Command::Enable("all").to_string(), "enable all");
    }

    #[test]
    fn event_lines_round_trip() {
        let line = EventLine::new(&SCHED_SWITCH, true);
        let text = line.to_string();
        assert_eq!(text, "2 sched.switch sched info on\n");
        assert_eq!(EventLine::parse(&text), Ok(line));
        assert_eq!(
            EventLine::parse("2 sched.switch sched info"),
            Err(LineError::Fields)
        );
        assert_eq!(
            EventLine::parse("x sched.switch sched info on"),
            Err(LineError::Number)
        );
        assert_eq!(
            EventLine::parse("2 sched.switch sched loud on"),
            Err(LineError::Level)
        );
        assert_eq!(
            EventLine::parse("2 sched.switch sched info yes"),
            Err(LineError::State)
        );
    }

    #[test]
    fn stats_lines_round_trip() {
        let line = StatsLine {
            cpu: 3,
            pending: 48,
            lost: 2,
        };
        let text = line.to_string();
        assert_eq!(text, "cpu 3 pending 48 lost 2\n");
        assert_eq!(StatsLine::parse(&text), Ok(line));
        assert_eq!(
            StatsLine::parse("cpu 3 queued 48 lost 2"),
            Err(LineError::Fields)
        );
    }
}
