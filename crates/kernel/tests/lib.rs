//! Host test harness for kernel algorithms that do not access hardware.
//!
//! Modules are included directly so tests exercise the kernel implementation.
//! This does not emulate context switches, user-copy faults or interrupt entry.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)] // Hardware-facing entry points are not called by host tests.

extern crate alloc;

#[path = "../src/cpu_set.rs"]
mod cpu_set;
#[path = "../src/sync/records.rs"]
mod records;
