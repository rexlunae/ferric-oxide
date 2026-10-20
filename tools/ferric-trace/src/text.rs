//! The text view: one line per record, in timestamp order.
//!
//! ```text
//! [000]      1234567: syscall.enter nr=0x21000004 a=0x7e57 b=0x... c=0xc91 d=0x0
//! ```
//!
//! The CPU, the timestamp in TSC cycles since the first record, the event name and its
//! arguments by name, in hexadecimal. A record whose arguments are codes ends with their
//! meaning after ` | ` ([`describe`](crate::describe)):
//!
//! ```text
//! [001]       802211: mm.page_fault addr=0x7f0000001000 ip=0x400a1c flags=0x6 outcome=0x0 | write user, mapped
//! ```

use std::fmt::Write as _;
use std::io::{self, Write};

use crate::capture::{Capture, Entry};
use crate::describe::describe;

/// The line for `entry`, with `base` as timestamp zero, without its newline.
pub fn line(entry: &Entry, base: u64) -> String {
    let mut out = format!(
        "[{:03}] {:>12}: {}",
        entry.cpu,
        entry.timestamp.saturating_sub(base),
        entry.event.name()
    );
    for (name, value) in entry.event.args().iter().zip(&entry.args) {
        // Writing to a String cannot fail.
        let _ = write!(out, " {name}={value:#x}");
    }
    if let Some(meaning) = describe(entry) {
        let _ = write!(out, " | {meaning}");
    }
    out
}

/// Writes every record of `capture` to `out`, one line each.
///
/// # Errors
///
/// The error of a failed write.
pub fn write(capture: &Capture, out: &mut impl Write) -> io::Result<()> {
    let base = capture.entries().first().map_or(0, |e| e.timestamp);
    for entry in capture.entries() {
        writeln!(out, "{}", line(entry, base))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CpuCapture, tests::bytes};
    use ferric_trace::kernel::{MM_PAGE_FAULT, SCHEME_RESPONSE, SYSCALL_EXIT};

    #[test]
    fn lines_name_the_arguments() {
        let b = bytes(&[
            (&SYSCALL_EXIT, 100, &[4, 7]),
            (&SCHEME_RESPONSE, 150, &[2, 9, 16, 33]),
        ]);
        let capture = Capture::from_cpus(vec![CpuCapture::decode(1, &b).expect("valid")]);
        let mut out = Vec::new();
        write(&capture, &mut out).expect("written");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "[001]            0: syscall.exit nr=0x4 ret=0x7\n\
             [001]           50: scheme.response scheme=0x2 tag=0x9 ret=0x10 responder=0x21\n"
        );
    }

    #[test]
    fn coded_arguments_are_described() {
        let b = bytes(&[(&MM_PAGE_FAULT, 100, &[0x1000, 0x40, 0x6, 0])]);
        let capture = Capture::from_cpus(vec![CpuCapture::decode(0, &b).expect("valid")]);
        assert_eq!(
            line(&capture.entries()[0], 100),
            "[000]            0: mm.page_fault addr=0x1000 ip=0x40 flags=0x6 outcome=0x0 | write user, mapped"
        );
    }
}
