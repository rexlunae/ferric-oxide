//! Host-side reader of Ferric Oxide kernel trace captures.
//!
//! A *capture* is what the kernel's `kernel.trace` scheme hands out: for each CPU, the bytes
//! read from `/scheme/kernel.trace/cpu/<n>`, a sequence of whole records in the format of
//! [`ferric_trace::record`]. On the target, `ferric-trace capture <dir>` (as root) saves one
//! as a directory of `cpu<n>` files, which this crate reads back on the host.
//!
//! - [`drain`] saves a capture on the target: it drains every CPU the scheme's `stats` lists
//!   into a directory (the `ferric-trace capture` command).
//! - [`capture`] loads and decodes the files against the kernel's event catalog
//!   ([`ferric_trace::kernel::CATALOG`]) and merges them in timestamp order.
//! - [`crash`] reads the crash dump a kernel panic left for the next boot
//!   (`/scheme/kernel.trace/crash`, the `ferric-trace crash` command) as a capture.
//! - [`text`] prints one line per record, like the kernel's `ftrace` text output.
//! - [`perfetto`] converts a capture to the Chrome trace event JSON format, which the Perfetto
//!   UI (<https://ui.perfetto.dev>) and `chrome://tracing` open. The format is described in
//!   Google's "Trace Event Format" document; Perfetto's JSON importer accepts the subset used
//!   here.
//!
//! # Examples
//!
//! ```
//! use ferric_trace::kernel::{SYSCALL_ENTER, SYSCALL_EXIT};
//! use ferric_trace_cli::capture::{Capture, CpuCapture};
//!
//! let mut bytes = Vec::new();
//! for (event, ts, args) in [(&SYSCALL_ENTER, 10, &[3u64, 0, 0, 0, 0][..]), (&SYSCALL_EXIT, 20, &[3, 0])] {
//!     let mut words = [0u64; ferric_trace::record::MAX_RECORD_WORDS];
//!     let n = ferric_trace::record::encode(&mut words, event.id(), ts, args).expect("fits");
//!     bytes.extend(words[..n].iter().flat_map(|w| w.to_le_bytes()));
//! }
//! let capture = Capture::from_cpus(vec![CpuCapture::decode(0, &bytes).expect("valid")]);
//! assert_eq!(capture.entries().len(), 2);
//! let tsc = ferric_trace_cli::perfetto::TscMhz::new(1000.0).expect("a valid frequency");
//! let json = ferric_trace_cli::perfetto::to_json(&capture, tsc);
//! assert!(json.contains("\"ph\":\"B\""));
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod capture;
pub mod crash;
pub mod drain;
pub mod perfetto;
pub mod text;
