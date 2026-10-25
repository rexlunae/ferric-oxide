//! Interrupt instructions

pub use crate::arch::x86_shared::interrupt::*;

pub mod handler;

pub mod syscall;

pub use self::handler::InterruptStack;
