//! `ferric-ready`: tells the process that started a daemon that the daemon now serves its
//! scheme.
//!
//! A starter that wants the handshake (`ferric-unlock`, at boot) sets [`ENV`] to [`STDOUT`] in
//! the daemon's environment and gives it a pipe as its stdout. Once the daemon has registered
//! its scheme, it calls [`notify`], which writes [`LINE`] and a newline to stdout; the starter
//! reads it with [`read_ready`]. If the daemon exits first (its scheme name was already taken,
//! a key was wrong), the starter reads end-of-file instead.
//!
//! This proves more than seeing `/scheme/NAME` appear would: a scheme of that name that some
//! other process already served would also appear, but only the daemon that registered the
//! name itself sends the line. Without [`ENV`] set, [`notify`] writes nothing, so a daemon run
//! by hand prints nothing extra.
//!
//! A daemon that notifies must not write anything else to stdout: once the starter has read
//! the line it may close its end of the pipe. Diagnostics go to stderr.
//!
//! # Examples
//!
//! ```
//! // The daemon's side, after registering its scheme (a no-op here: ENV is not set).
//! assert!(!ferric_ready::notify()?);
//! // The starter's side, on what the daemon wrote.
//! assert!(ferric_ready::read_ready(&mut &b"ready\n"[..])?);
//! assert!(!ferric_ready::read_ready(&mut &b""[..])?);
//! # Ok::<(), std::io::Error>(())
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::io::{self, BufRead, Write};

/// The environment variable a starter sets to ask for the handshake.
pub const ENV: &str = "FERRIC_READY";
/// The value of [`ENV`] that asks for the line on stdout, the only channel so far.
pub const STDOUT: &str = "stdout";
/// The line a daemon writes once it serves its scheme.
pub const LINE: &str = "ready";

/// Whether the starter asked this process for the handshake.
pub fn requested() -> bool {
    std::env::var_os(ENV).is_some_and(|value| value == STDOUT)
}

/// Writes [`LINE`] to stdout if the starter asked for it ([`requested`]); returns whether it
/// did.
///
/// # Errors
///
/// When stdout cannot be written or flushed.
pub fn notify() -> io::Result<bool> {
    if !requested() {
        return Ok(false);
    }
    write_line(&mut io::stdout().lock())?;
    Ok(true)
}

fn write_line(out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "{LINE}")?;
    out.flush()
}

/// Reads the daemon's stdout up to its first line: `true` if that is [`LINE`], `false` at
/// end-of-file (the daemon exited, or closed stdout, without notifying) or on any other line.
///
/// # Errors
///
/// When reading fails.
pub fn read_ready(from: &mut impl BufRead) -> io::Result<bool> {
    let mut line = String::new();
    if from.read_line(&mut line)? == 0 {
        return Ok(false);
    }
    Ok(line.trim_end_matches(['\n', '\r']) == LINE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_reads_back_as_ready() {
        let mut out = Vec::new();
        write_line(&mut out).expect("written");
        assert_eq!(out, b"ready\n");
        assert!(read_ready(&mut out.as_slice()).expect("read"));
    }

    #[test]
    fn anything_else_is_not_ready() {
        for text in [&b""[..], b"\n", b"read", b"ready?\n", b"not ready\nready\n"] {
            assert!(!read_ready(&mut &text[..]).expect("read"), "{text:?}");
        }
        assert!(read_ready(&mut &b"ready\r\n"[..]).expect("read"));
    }
}
