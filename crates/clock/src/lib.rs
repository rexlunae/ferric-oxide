//! Clocksource and clockevent arithmetic for the tickless kernel (issue #186).
//!
//! The kernel keeps two timer responsibilities apart, in the clocksource/event-timer split
//! FreeBSD's `eventtimers(4)` describes:
//!
//! - A **clocksource** is a free-running counter read to learn the time: on x86-64 the TSC.
//!   [`Scale`] converts its ticks to nanoseconds and back, and [`mod@calibrate`] measures its
//!   rate against a reference counter of known period (the HPET or the PIT).
//! - A **clockevent** interrupts a CPU at a chosen time: on x86-64 the local APIC timer, in
//!   TSC-deadline mode or as a calibrated one-shot countdown (Intel SDM, Vol. 3A,
//!   section 11.5.4, "APIC Timer"). [`oneshot`] turns an absolute deadline into the value
//!   to program and classifies each interrupt as due or early.
//!
//! [`selftest`] is the boot check the kernel runs when `kernel.oneshot_timer` is on: a
//! fixed series of one-shot deadlines per CPU, with how late each fired.
//!
//! Nothing here touches hardware, so all of it is tested on the host; the kernel reads the
//! counters and writes the registers (`crates/kernel/src/arch/x86_shared/device/oneshot.rs`).
//!
//! # Examples
//!
//! ```
//! use ferric_clock::{Hz, Scale};
//!
//! let tsc = Scale::new(Hz::new(3_000_000_000).unwrap());
//! assert_eq!(tsc.ticks_to_ns(3_000_000_000), 1_000_000_000);
//! // A deadline converted to ticks never comes early.
//! assert_eq!(tsc.ns_to_ticks_ceil(1), 3);
//! ```

#![no_std]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod calibrate;
pub mod oneshot;
mod scale;
pub mod selftest;

pub use calibrate::{Calibrated, CalibrationError, Sample, WrappingCounter, calibrate};
pub use scale::{Hz, NANOS_PER_SEC, Scale};
