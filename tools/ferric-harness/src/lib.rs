//! Boots Ferric Oxide images in QEMU and drives them from the host.
//!
//! The harness starts QEMU under a supervisor that owns the guest's QMP connection and
//! serial console, so a test or a person can look at the guest and act on it without
//! adding any code to the guest:
//!
//! - **console**: every serial line is logged raw and with host timestamps; `expect` waits
//!   for a regular expression and `serial send` types on it ([`serial`]);
//! - **screen**: QMP screenshots as PNG, and exact text recognition of the text consoles,
//!   whose font is fixed ([`screen`]);
//! - **input**: keyboard (US layout, key combinations) and an absolute pointer through QMP
//!   `input-send-event` ([`input`]);
//! - **boot control**: a boot environment through QEMU fw_cfg, or typed into the
//!   bootloader's editor, and answers to its resolution menu ([`bootenv`]);
//! - **trace mode**: the trace kernel with guest RAM in a shared file that
//!   `ferric-trace follow` reads from the host ([`trace`]); a normal-mode run fails if the
//!   kernel prints any `trace:` line;
//! - **measurement**: boot milestones, test markers and `FERRIC_BENCH` records, host thread
//!   CPU time and KVM statistics, a verdict, and `report.json` ([`observe`], [`markers`],
//!   [`stats`], [`verdict`], [`report`]), with paired A/B comparison across runs.
//!
//! [`session::Session`] is a running guest; [`control`] serves it to the command-line
//! client from a supervisor process; [`scenario`] runs scripted steps against it.
//!
//! # Examples
//!
//! Reading the text of a console screenshot:
//!
//! ```
//! use ferric_harness::screen::{read_text, render_text};
//!
//! let image = render_text(&["redox login:"], 40, 2, 0xc0c0c0, 0x000000, None);
//! assert_eq!(read_text(&image).text(), "redox login:");
//! ```
//!
//! Booting an image and logging in on the serial console:
//!
//! ```no_run
//! use std::time::Duration;
//! use ferric_harness::machine::LaunchConfig;
//! use ferric_harness::serial::{Eol, Secrecy};
//! use ferric_harness::session::{Session, Since};
//!
//! let config = LaunchConfig::new("build/x86_64/desktop/harddrive.img".into(),
//!                                std::env::current_dir()?.join("build/harness/example"));
//! let session = Session::launch(config)?;
//! session.expect("login: $", &Since::Cursor, Duration::from_secs(300))?;
//! session.serial_send("root", Eol::Cr, Secrecy::Logged)?;
//! session.expect("assword", &Since::Cursor, Duration::from_secs(10))?;
//! // A secret is sent as given and logged as `<redacted 8 bytes>`.
//! session.serial_send("password", Eol::Cr, Secrecy::Secret)?;
//! let report = session.down(None);
//! println!("{:?}", report.verdict.map(|v| v.result));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod bootenv;
pub mod clock;
pub mod control;
pub mod error;
pub mod input;
pub mod machine;
pub mod markers;
pub mod observe;
pub mod qmp;
pub mod report;
pub mod rundir;
pub mod scenario;
pub mod screen;
pub mod serial;
pub mod session;
pub mod stats;
pub mod trace;
pub mod verdict;

pub use error::Error;
pub use machine::LaunchConfig;
pub use session::Session;
