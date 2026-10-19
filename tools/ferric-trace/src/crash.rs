//! Reading a crash dump: the file `/scheme/kernel.trace/crash` that the kernel keeps after a
//! panic on the previous boot (`kernel.trace_crash`, [`ferric_trace::crash`]).
//!
//! [`CrashReport::load`] reads the panic message and each CPU's newest records as a
//! [`Capture`], which [`text`](crate::text) and [`perfetto`](crate::perfetto) print like any
//! other, and [`CrashReport::save`] writes the records as a capture directory of `cpu<n>` files.
//! The dump was taken while the other CPUs kept running, so a CPU's records can end in a torn
//! one; its records are kept up to the first that does not decode, and the rest are counted in
//! [`CpuCrash::dropped`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ferric_trace::crash::{CrashError, Dump};
use ferric_trace::kernel::CATALOG;
use ferric_trace::record::{Records, WORD_BYTES};

use crate::capture::{Capture, CaptureError, CpuCapture};

/// Why a crash dump could not be read or saved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CrashDumpError {
    /// A file could not be read or written.
    #[error("cannot access {}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is not a complete crash dump.
    #[error("{} is not a crash dump", path.display())]
    Dump {
        /// The path.
        path: PathBuf,
        /// Why.
        #[source]
        source: CrashError,
    },
    /// The records that decode could not be loaded.
    #[error(transparent)]
    Capture(#[from] CaptureError),
}

/// A CPU's part of a dump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuCrash {
    /// The CPU.
    pub cpu: u32,
    /// Its ring's lost-record count when the dump was taken.
    pub lost: u64,
    /// The whole records that decode, as bytes.
    pub records: Vec<u8>,
    /// The bytes after them that do not decode (a torn record and what followed it).
    pub dropped: usize,
}

/// A crash dump, read.
#[derive(Clone, Debug)]
pub struct CrashReport {
    /// The CPU that panicked and its panic message, if the dump has one (invalid UTF-8 is
    /// replaced).
    pub message: Option<(u32, String)>,
    /// Each CPU's records, in the dump's order.
    pub cpus: Vec<CpuCrash>,
}

impl CrashReport {
    /// Reads the dump in `bytes`.
    ///
    /// # Errors
    ///
    /// The [`CrashError`] of a dump that is not complete or intact.
    pub fn parse(bytes: &[u8]) -> Result<Self, CrashError> {
        let dump = Dump::parse(bytes)?;
        let message = dump
            .message()
            .map(|(cpu, text)| (cpu, String::from_utf8_lossy(text).into_owned()));
        let cpus = dump
            .cpus()
            .map(|cpu| {
                let len = decodable_len(cpu.records);
                CpuCrash {
                    cpu: cpu.cpu,
                    lost: cpu.lost,
                    records: cpu.records[..len].to_vec(),
                    dropped: cpu.records.len() - len,
                }
            })
            .collect();
        Ok(Self { message, cpus })
    }

    /// Reads the dump in the file at `path`.
    ///
    /// # Errors
    ///
    /// [`CrashDumpError::Io`] when the file cannot be read, and [`CrashDumpError::Dump`] when
    /// it is not a complete, intact dump.
    pub fn load(path: &Path) -> Result<Self, CrashDumpError> {
        let bytes = fs::read(path).map_err(|source| CrashDumpError::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&bytes).map_err(|source| CrashDumpError::Dump {
            path: path.to_owned(),
            source,
        })
    }

    /// The records as a capture.
    ///
    /// # Errors
    ///
    /// None in practice: only records that decode are kept. A [`CaptureError`] from decoding
    /// them is passed on.
    pub fn capture(&self) -> Result<Capture, CaptureError> {
        let cpus = self
            .cpus
            .iter()
            .map(|cpu| CpuCapture::decode(cpu.cpu, &cpu.records))
            .collect::<Result<_, _>>()?;
        Ok(Capture::from_cpus(cpus))
    }

    /// Writes the records as a capture directory `out` of `cpu<n>` files, which must not
    /// exist yet.
    ///
    /// # Errors
    ///
    /// [`CrashDumpError::Io`] when the directory or a file cannot be created.
    pub fn save(&self, out: &Path) -> Result<(), CrashDumpError> {
        let io = |path: &Path| {
            let path = path.to_owned();
            move |source| CrashDumpError::Io { path, source }
        };
        fs::create_dir(out).map_err(io(out))?;
        for cpu in &self.cpus {
            let path = out.join(format!("cpu{}", cpu.cpu));
            fs::write(&path, &cpu.records).map_err(io(&path))?;
        }
        Ok(())
    }
}

/// The length of the whole records at the start of `bytes` that decode as events of the
/// kernel catalog with their argument count.
fn decodable_len(bytes: &[u8]) -> usize {
    let mut len = 0;
    for record in Records::new(bytes) {
        let Ok(record) = record else {
            break;
        };
        let fits = CATALOG
            .get(record.event())
            .is_some_and(|event| event.args().len() == record.arg_count());
        if !fits {
            break;
        }
        len += (ferric_trace::record::HEADER_WORDS + record.arg_count()) * WORD_BYTES;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};
    use ferric_trace::crash::{REGION_WORDS, Writer};
    use ferric_trace::kernel::{SCHED_SWITCH, SYSCALL_EXIT};
    use ferric_trace::ring::Ring;

    fn ring_words() -> Vec<AtomicU64> {
        (0..64).map(|_| AtomicU64::new(0)).collect()
    }

    /// A dump of `message` and two CPUs, the second with a torn last record.
    fn dump_bytes(message: &[u8]) -> Vec<u8> {
        let words: Vec<AtomicU64> = (0..REGION_WORDS).map(|_| AtomicU64::new(0)).collect();
        let ring0 = Ring::new(ring_words()).expect("valid");
        ring0
            .push(SCHED_SWITCH.id(), 5, &[1, 2, 3, 4])
            .expect("room");
        ring0.push(SYSCALL_EXIT.id(), 9, &[20, 0]).expect("room");
        let ring1 = Ring::new(ring_words()).expect("valid");
        ring1.push(SYSCALL_EXIT.id(), 7, &[1, 2]).expect("room");
        // A record of the wrong argument count, as a torn copy would look.
        ring1.push(SYSCALL_EXIT.id(), 8, &[1]).expect("room");
        let mut writer = Writer::begin(&words).expect("big enough");
        writer.message(1, message);
        writer.cpu(0, &ring0, 1000);
        writer.cpu(1, &ring1, 1000);
        writer.finish();
        let len = Dump::len_in(&words).expect("complete");
        words
            .iter()
            .flat_map(|w| w.load(Ordering::Relaxed).to_le_bytes())
            .take(len)
            .collect()
    }

    #[test]
    fn reads_a_dump_into_a_capture() {
        let report =
            CrashReport::parse(&dump_bytes(b"panicked at x.rs:1:2: oops")).expect("a dump");
        assert_eq!(
            report.message,
            Some((1, "panicked at x.rs:1:2: oops".into()))
        );
        assert_eq!(report.cpus.len(), 2);
        assert_eq!(report.cpus[0].dropped, 0);
        assert_eq!(report.cpus[1].dropped, 24);
        let capture = report.capture().expect("decodes");
        assert_eq!(capture.cpus(), [0, 1]);
        let stamps: Vec<_> = capture.entries().iter().map(|e| e.timestamp).collect();
        assert_eq!(stamps, [5, 7, 9]);
    }

    #[test]
    fn saves_a_capture_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let report = CrashReport::parse(&dump_bytes(b"x")).expect("a dump");
        let out = dir.path().join("crash");
        report.save(&out).expect("saved");
        let capture = Capture::load(std::slice::from_ref(&out)).expect("a capture");
        assert_eq!(capture.entries().len(), 3);
        assert!(matches!(report.save(&out), Err(CrashDumpError::Io { .. })));
    }

    #[test]
    fn refuses_what_is_not_a_dump() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("crash");
        fs::write(&path, [0u8; 64]).expect("written");
        assert!(matches!(
            CrashReport::load(&path),
            Err(CrashDumpError::Dump {
                source: CrashError::NoMagic,
                ..
            })
        ));
        let mut bytes = dump_bytes(b"x");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_eq!(
            CrashReport::parse(&bytes).map(|_| ()),
            Err(CrashError::Checksum)
        );
    }
}
