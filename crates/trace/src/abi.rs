//! The `kernel.trace` scheme's interface.
//!
//! The scheme is open to root only for now; a Phase 4 capability replaces the check, because
//! traces leak timing and kernel addresses.
//!
//! | Path | Read | Write |
//! | --- | --- | --- |
//! | [`EVENTS_PATH`] | one [`EventLine`] per event | a [`Command`]: `enable <selection>`, `disable <selection>`, or `filter <event> <filter\|none>` |
//! | [`STATS_PATH`] | one [`StatsLine`] per CPU | — |
//! | [`FILTERS_PATH`] | one [`FilterLine`] per filtered event, written as the command that sets it (a [`FilterBusyLine`] for one that could not be read) | — |
//! | `cpu/<n>` ([`Path::Cpu`]) | drains whole records from CPU n's ring, in the [`record`](crate::record) format; 0 bytes when it is empty | a [`CpuCommand`]: `commit <end>` |
//!
//! A selection is the syntax of [`select`](crate::select()). Lines end with `\n` and their fields
//! are separated by single spaces.
//!
//! # Mapping a CPU's ring
//!
//! Instead of reading, a reader can `mmap` a `cpu/<n>` descriptor: [`CPU_MAP_BYTES`] from
//! offset 0, `PROT_READ` and `MAP_SHARED` only (the kernel refuses `PROT_WRITE`,
//! `MAP_PRIVATE` and fixed addresses). The mapping is the CPU's ring in the
//! [shared layout](crate::ring#storage), which a [`View`](crate::ring::View) reads in place,
//! without a system call per batch. To let the kernel reuse what it has read, the reader
//! writes `commit <end>` to the descriptor, `end` being the [`Peek::end`] of its last copy;
//! the kernel accepts only the end of a record between its own `tail` and `head`, and returns
//! `EINVAL` otherwise and `EAGAIN` while another reader of that CPU is reading.
//!
//! The kernel stays the only writer of the mapped words, and it never takes a value from them
//! that it did not store itself: the ring's size and its counters are kept in the kernel, so
//! nothing a reader sees, or holds on to, can steer it. The reader in turn must treat every
//! word as a snapshot that can change under it, which [`View`](crate::ring::View) does: a
//! read of the same CPU through the kernel can consume records while the mapped reader copies
//! them, so `View::peek` checks `tail` after each copy and starts over if it moved past the
//! copy. Two readers of one CPU can still both see a record; a collector that wants each
//! record once should be the CPU's only reader.
//!
//! [`Peek::end`]: crate::ring::Peek::end

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

/// The file listing the argument filters that are set ([`crate::filter`]).
pub const FILTERS_PATH: &str = "filters";

/// The crash dump the previous boot's panic left ([`crate::crash`]), with `kernel.trace_crash`.
///
/// It exists only when there is a dump: opening it otherwise fails with `ENOENT`. Reads give
/// the dump's bytes, from the offset given. On a kernel with `kernel.trace_crash_test`, writing
/// [`CRASH_TEST_COMMAND`] to it panics the kernel on purpose, so CI can test the dump across a
/// reboot.
pub const CRASH_PATH: &str = "crash";

/// The command that, written to [`CRASH_PATH`], panics a kernel built for testing the crash
/// dump; the kernel resets the machine once the dump is written.
pub const CRASH_TEST_COMMAND: &str = "panic";

/// The longest command, in bytes, one write to [`EVENTS_PATH`] may carry; the kernel refuses
/// longer writes.
pub const MAX_COMMAND_BYTES: usize = 4096;

/// The directory of per-CPU record files, `cpu/<n>`.
///
/// A read of `cpu/<n>` returns whole records ([`crate::record`]) and consumes them; 0 bytes means
/// the ring is empty. A buffer too small for the next record gets `EINVAL` and the record stays:
/// a buffer of [`READ_MIN`] bytes always holds one. While another reader is reading the same
/// CPU, a read gets `EAGAIN`.
pub const CPU_DIR: &str = "cpu";

/// The smallest buffer a read of `cpu/<n>` always succeeds with: the longest record.
pub const READ_MIN: usize = crate::record::MAX_RECORD_WORDS * crate::record::WORD_BYTES;

/// The data words of each CPU's ring: 64 KiB of records.
pub const CPU_RING_WORDS: usize = 8192;

/// The size of a mapping of `cpu/<n>`: the [shared](crate::ring#storage) header and
/// [`CPU_RING_WORDS`] of data.
pub const CPU_MAP_BYTES: usize =
    (crate::ring::SHARED_HEADER_WORDS + CPU_RING_WORDS) * crate::record::WORD_BYTES;

/// A path in the scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// The scheme root.
    Root,
    /// [`EVENTS_PATH`].
    Events,
    /// [`STATS_PATH`].
    Stats,
    /// [`FILTERS_PATH`].
    Filters,
    /// `cpu/<n>`: the records of CPU `n`.
    Cpu(u32),
    /// [`CRASH_PATH`].
    Crash,
}

impl Path {
    /// The path `path` names, ignoring slashes at either end, or `None` for any other path.
    pub fn parse(path: &str) -> Option<Self> {
        match path.trim_matches('/') {
            "" => Some(Path::Root),
            EVENTS_PATH => Some(Path::Events),
            STATS_PATH => Some(Path::Stats),
            FILTERS_PATH => Some(Path::Filters),
            CRASH_PATH => Some(Path::Crash),
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
    /// Set the argument filter of one event, or remove it with
    /// [`filter::NONE`](crate::filter::NONE): `filter <event> <filter>`. The filter text is
    /// parsed against the event's arguments with [`Filter::parse`](crate::filter::Filter::parse).
    Filter {
        /// The event's name.
        event: &'a str,
        /// The filter text.
        filter: &'a str,
    },
}

/// A write to [`EVENTS_PATH`] that is not a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandError {
    /// The text is not UTF-8.
    NotUtf8,
    /// The first word is not a command this file takes.
    UnknownVerb,
    /// The position of a `commit` is missing or not a decimal number.
    Position,
    /// A `filter` without both an event name and a filter.
    FilterEvent,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CommandError::NotUtf8 => "trace command is not UTF-8",
            CommandError::UnknownVerb => {
                "trace command must start with enable, disable or filter (events) or commit (cpu/<n>)"
            }
            CommandError::FilterEvent => "trace filter needs an event name and a filter",
            CommandError::Position => "trace commit needs a decimal position",
        })
    }
}

impl core::error::Error for CommandError {}

/// The first word of `bytes` and the rest, each trimmed.
fn verb(bytes: &[u8]) -> Result<(&str, &str), CommandError> {
    let text = core::str::from_utf8(bytes).map_err(|_| CommandError::NotUtf8)?;
    let text = text.trim();
    let (verb, rest) = text
        .split_once(|c: char| c.is_ascii_whitespace())
        .unwrap_or((text, ""));
    Ok((verb, rest.trim()))
}

impl<'a> Command<'a> {
    /// Parses a command, ignoring surrounding whitespace.
    ///
    /// # Errors
    ///
    /// [`CommandError::NotUtf8`], [`CommandError::UnknownVerb`], or
    /// [`CommandError::FilterEvent`] for a `filter` without an event and a filter.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, CommandError> {
        let (verb, rest) = verb(bytes)?;
        match verb {
            "enable" => Ok(Command::Enable(rest)),
            "disable" => Ok(Command::Disable(rest)),
            "filter" => {
                let (event, filter) = rest
                    .split_once(|c: char| c.is_ascii_whitespace())
                    .ok_or(CommandError::FilterEvent)?;
                Ok(Command::Filter {
                    event,
                    filter: filter.trim(),
                })
            }
            _ => Err(CommandError::UnknownVerb),
        }
    }
}

/// A command written to `cpu/<n>` ([`Path::Cpu`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CpuCommand {
    /// Consume the ring up to this position, the [`Peek::end`](crate::ring::Peek::end) of a
    /// copy from the mapped ring: `commit <end>`.
    Commit(u64),
}

impl CpuCommand {
    /// Parses a command, ignoring surrounding whitespace.
    ///
    /// # Errors
    ///
    /// [`CommandError::NotUtf8`], [`CommandError::UnknownVerb`], or [`CommandError::Position`]
    /// when the position is not a decimal `u64`.
    pub fn parse(bytes: &[u8]) -> Result<Self, CommandError> {
        match verb(bytes)? {
            ("commit", end) if !end.is_empty() && end.bytes().all(|b| b.is_ascii_digit()) => end
                .parse()
                .map(CpuCommand::Commit)
                .map_err(|_| CommandError::Position),
            ("commit", _) => Err(CommandError::Position),
            _ => Err(CommandError::UnknownVerb),
        }
    }
}

impl fmt::Display for CpuCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CpuCommand::Commit(end) => write!(f, "commit {end}"),
        }
    }
}

impl fmt::Display for Command<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Enable(spec) => write!(f, "enable {spec}"),
            Command::Disable(spec) => write!(f, "disable {spec}"),
            Command::Filter { event, filter } => write!(f, "filter {event} {filter}"),
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

/// One line of the [`FILTERS_PATH`] file: the command that sets the filter,
/// `filter <event> <filter>`, which [`Command::parse`] reads back.
#[derive(Clone, Copy, Debug)]
pub struct FilterLine<'a> {
    /// The filtered event.
    pub event: &'a Event,
    /// Its filter.
    pub filter: &'a crate::filter::Filter,
}

impl fmt::Display for FilterLine<'_> {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "filter {} {}",
            self.event.name(),
            self.filter.display(self.event)
        )
    }
}

/// The line of the [`FILTERS_PATH`] file for an event whose filter is set but could not be
/// read because a replacement kept overlapping the read
/// ([`Loaded::Busy`](crate::filter::Loaded::Busy)): `# filter <event> is being replaced`.
/// It starts with `#` so it is not read back as a command. The kernel reads the filters with
/// their writer excluded, so it does not write this line in practice.
#[derive(Clone, Copy, Debug)]
pub struct FilterBusyLine<'a> {
    /// The filtered event.
    pub event: &'a Event,
}

impl fmt::Display for FilterBusyLine<'_> {
    /// The line, with its `\n`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "# filter {} is being replaced", self.event.name())
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
        assert_eq!(Path::parse("filters"), Some(Path::Filters));
        assert_eq!(Path::parse("crash"), Some(Path::Crash));
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
    fn a_busy_filter_line_is_not_a_command() {
        let line = FilterBusyLine {
            event: &SCHED_SWITCH,
        }
        .to_string();
        assert_eq!(line, "# filter sched.switch is being replaced\n");
        assert!(Command::parse(line.as_bytes()).is_err());
    }

    #[test]
    fn filter_lines_read_back_as_commands() {
        use crate::filter::Filter;
        let filter = Filter::parse(&SCHED_SWITCH, "next_pid == 7")
            .expect("parses")
            .expect("set");
        let line = FilterLine {
            event: &SCHED_SWITCH,
            filter: &filter,
        }
        .to_string();
        assert_eq!(line, "filter sched.switch next_pid == 0x7\n");
        let Ok(Command::Filter {
            event,
            filter: text,
        }) = Command::parse(line.as_bytes())
        else {
            panic!("{line:?} is not a filter command");
        };
        assert_eq!(event, "sched.switch");
        assert_eq!(Filter::parse(&SCHED_SWITCH, text), Ok(Some(filter)));
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
        assert_eq!(
            Command::parse(b"filter syscall.enter  nr == 3 && a < 4\n"),
            Ok(Command::Filter {
                event: "syscall.enter",
                filter: "nr == 3 && a < 4"
            })
        );
        assert_eq!(
            Command::parse(b"filter syscall.enter"),
            Err(CommandError::FilterEvent)
        );
        let line = Command::Filter {
            event: "sched.switch",
            filter: "none",
        };
        assert_eq!(Command::parse(line.to_string().as_bytes()), Ok(line));
        assert_eq!(Command::Enable("all").to_string(), "enable all");
    }

    #[test]
    fn cpu_commands() {
        assert_eq!(
            CpuCommand::parse(b"commit 4096\n"),
            Ok(CpuCommand::Commit(4096))
        );
        assert_eq!(CpuCommand::parse(b" commit  0 "), Ok(CpuCommand::Commit(0)));
        for bad in [
            &b"commit"[..],
            b"commit +3",
            b"commit -1",
            b"commit 1 2",
            b"commit x",
        ] {
            assert_eq!(
                CpuCommand::parse(bad),
                Err(CommandError::Position),
                "{bad:?}"
            );
        }
        assert_eq!(
            CpuCommand::parse(b"commit 18446744073709551616"),
            Err(CommandError::Position)
        );
        assert_eq!(
            CpuCommand::parse(b"enable all"),
            Err(CommandError::UnknownVerb)
        );
        assert_eq!(CpuCommand::parse(b"\xff"), Err(CommandError::NotUtf8));
        let text = CpuCommand::Commit(77).to_string();
        assert_eq!(text, "commit 77");
        assert_eq!(
            CpuCommand::parse(text.as_bytes()),
            Ok(CpuCommand::Commit(77))
        );
    }

    #[test]
    fn map_size() {
        assert_eq!(CPU_MAP_BYTES, 17 * 4096);
        assert!(CPU_RING_WORDS.is_power_of_two());
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
