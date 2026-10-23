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

    #[test]
    fn futex_records_keep_the_shared_key_context_and_result() {
        use ferric_trace::kernel::{SCHED_FUTEX_RESUME, SCHED_FUTEX_WAIT, SCHED_FUTEX_WAKE};

        // The waker uses another virtual address for the same physical word. The waiter
        // resumes on another CPU; neither the key nor the context may change in decoding.
        let waiter = bytes(&[(
            &SCHED_FUTEX_WAIT,
            100,
            &[0x8000, 0x4000, 27, 64, u64::MAX, 1],
        )]);
        let waker = bytes(&[(&SCHED_FUTEX_WAKE, 120, &[0x8000, 0x9000, 1, 1])]);
        let resumed = bytes(&[(&SCHED_FUTEX_RESUME, 140, &[0x8000, 0x4000, 27, 0])]);
        let capture = Capture::from_cpus(vec![
            CpuCapture::decode(0, &waiter).unwrap(),
            CpuCapture::decode(1, &waker).unwrap(),
            CpuCapture::decode(2, &resumed).unwrap(),
        ]);
        let mut out = Vec::new();
        write(&capture, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "[000]            0: sched.futex_wait key=0x8000 addr=0x4000 context=0x1b bits=0x40 expected=0xffffffffffffffff timed=0x1\n\
             [001]           20: sched.futex_wake key=0x8000 addr=0x9000 requested=0x1 woken=0x1\n\
             [002]           40: sched.futex_resume key=0x8000 addr=0x4000 context=0x1b ret=0x0\n"
        );
    }

    #[test]
    fn context_lifecycle_preserves_identity_and_raw_exception_fields() {
        use ferric_trace::kernel::{SCHED_CONTEXT_CREATE, SCHED_CONTEXT_EXIT, SCHED_CONTEXT_START};
        let b = bytes(&[
            (&SCHED_CONTEXT_CREATE, 10, &[27, 1]),
            (&SCHED_CONTEXT_START, 20, &[42, 27]),
            (
                &SCHED_CONTEXT_EXIT,
                30,
                &[42, 27, 1, 14, 7, 0xffff_ffff_1234_5000],
            ),
            (&SCHED_CONTEXT_EXIT, 40, &[43, 28, 0, 0, 0, 0]),
        ]);
        let capture = Capture::from_cpus(vec![CpuCapture::decode(2, &b).unwrap()]);
        let mut out = Vec::new();
        write(&capture, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("sched.context_create context=0x1b user_mode=0x1"));
        assert!(text.contains("sched.context_start pid=0x2a context=0x1b"));
        assert!(text.contains("sched.context_exit pid=0x2a context=0x1b exception=0x1 kind=0xe code=0x7 addr=0xffffffff12345000"));
        assert!(text.contains(
            "sched.context_exit pid=0x2b context=0x1c exception=0x0 kind=0x0 code=0x0 addr=0x0"
        ));
    }

    #[test]
    fn mapping_operations_preserve_space_requests_and_failures() {
        use ferric_trace::kernel::{MM_MAP, MM_PROTECT, MM_UNMAP};
        let b = bytes(&[
            (&MM_MAP, 10, &[0x1000, 0, 2, 3, 0x8000]),
            (&MM_MAP, 20, &[0x1000, 0x8000, 2, 0x100003, u64::MAX - 16]),
            (&MM_PROTECT, 30, &[0x1000, 0x8000, 1, 1, 0]),
            (&MM_UNMAP, 40, &[0x1000, 0x8000, 2, 0, 0]),
        ]);
        let capture = Capture::from_cpus(vec![CpuCapture::decode(2, &b).unwrap()]);
        let mut out = Vec::new();
        write(&capture, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("mm.map space=0x1000 addr=0x0 pages=0x2 flags=0x3 ret=0x8000"));
        assert!(text.contains(
            "mm.map space=0x1000 addr=0x8000 pages=0x2 flags=0x100003 ret=0xffffffffffffffef"
        ));
        assert!(text.contains("mm.protect space=0x1000 addr=0x8000 pages=0x1 flags=0x1 ret=0x0"));
        assert!(text.contains("mm.unmap space=0x1000 addr=0x8000 pages=0x2 unpin=0x0 ret=0x0"));
    }

    #[test]
    fn dma_operations_preserve_target_and_permissions() {
        use ferric_trace::kernel::{IOMMU_MAP, IOMMU_PROTECT, IOMMU_UNMAP};
        let b = bytes(&[
            (&IOMMU_MAP, 10, &[5, 0x1000, 0x2000, 0x3000, 0x1000, 1, 1]),
            (&IOMMU_PROTECT, 20, &[5, 0x1000, 0x2000, 0x1000, 1, 0]),
            (&IOMMU_UNMAP, 30, &[5, 0x1000, 0x2000, 0x1000]),
        ]);
        let capture = Capture::from_cpus(vec![CpuCapture::decode(2, &b).unwrap()]);
        let mut out = Vec::new();
        write(&capture, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("iommu.map domain=0x5 root=0x1000 iova=0x2000 phys=0x3000 bytes=0x1000 read=0x1 write=0x1"));
        assert!(text.contains(
            "iommu.protect domain=0x5 root=0x1000 iova=0x2000 bytes=0x1000 read=0x1 write=0x0"
        ));
        assert!(text.contains("iommu.unmap domain=0x5 root=0x1000 iova=0x2000 bytes=0x1000"));
    }
}
