//! Event levels.

use core::fmt;
use core::str::FromStr;

/// How verbose an event is, from [`Level::Error`] (least) to [`Level::Trace`] (most).
///
/// The names and their order are those of `tracing-core`'s `Level`. Levels compare by verbosity:
/// `Level::Error < Level::Trace`, so "compiled in" means `event.level() <= max_level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Level {
    /// Something went wrong.
    Error = 1,
    /// Something unusual happened.
    Warn = 2,
    /// Ordinary, low-rate events: system calls, context switches.
    Info = 3,
    /// Detailed events for debugging a subsystem.
    Debug = 4,
    /// Very high-rate events.
    Trace = 5,
}

impl Level {
    /// Every level, from the least to the most verbose.
    pub const ALL: [Level; 5] = [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ];

    /// The level's lower-case name, as `features.toml` and the scheme listing spell it.
    pub const fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }

    /// Whether an event of this level is within `max`: `self <= max`, usable in `const` code.
    pub const fn within(self, max: Level) -> bool {
        self as u8 <= max as u8
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A string that is not one of the level names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseLevelError;

impl fmt::Display for ParseLevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a level: expected error, warn, info, debug or trace")
    }
}

impl core::error::Error for ParseLevelError {}

impl FromStr for Level {
    type Err = ParseLevelError;

    /// Parses a level name, ignoring ASCII case.
    ///
    /// # Errors
    ///
    /// [`ParseLevelError`] when `s` is not a level name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Level::ALL
            .into_iter()
            .find(|level| level.name().eq_ignore_ascii_case(s))
            .ok_or(ParseLevelError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    #[test]
    fn order_is_verbosity() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Debug < Level::Trace);
        assert!(Level::Info.within(Level::Info));
        assert!(Level::Info.within(Level::Trace));
        assert!(!Level::Debug.within(Level::Info));
    }

    #[test]
    fn parse_round_trips() {
        for level in Level::ALL {
            assert_eq!(level.name().parse(), Ok(level));
            assert_eq!(level.to_string().to_uppercase().parse(), Ok(level));
        }
        assert_eq!("verbose".parse::<Level>(), Err(ParseLevelError));
    }
}
