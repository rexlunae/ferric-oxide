//! Host test harness for kernel algorithms that do not access hardware.
//!
//! Modules are included directly so tests exercise the kernel implementation.
//! This does not emulate context switches, user-copy faults or interrupt entry.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)] // Hardware-facing entry points are not called by host tests.

extern crate alloc;

#[path = "../src/arch/x86_shared/device/apic_id.rs"]
mod apic_id;
#[path = "../src/scheme/chown.rs"]
mod chown;
#[path = "../src/cpu_set.rs"]
mod cpu_set;
#[path = "../src/context/fmap_reply.rs"]
mod fmap_reply;
#[path = "../src/percpu/global_flush.rs"]
mod global_flush;
#[path = "../src/context/idle.rs"]
mod idle;
#[path = "../src/sync/records.rs"]
mod records;
#[path = "../src/sync/spin_wait.rs"]
mod spin_wait;
#[path = "../src/memory/tlb.rs"]
mod tlb;

#[cfg(test)]
mod tlb_ack;
#[cfg(test)]
mod wait_support;
#[cfg(test)]
use wait_support::{context, percpu};
#[cfg(test)]
#[path = "../src/sync/ordered.rs"]
mod ordered;
#[cfg(test)]
#[path = "../src/sync/wait_condition.rs"]
pub(crate) mod wait_condition;
#[cfg(test)]
mod sync {
    pub(crate) use crate::ordered::*;
}

#[test]
fn empty_wait_condition_destruction_does_not_acquire_a_lock_or_recreate_a_token() {
    // SAFETY: This test starts without locks and keeps only this clean token.
    let mut token = unsafe { sync::CleanLockToken::new() };
    let outer = sync::Mutex::<sync::L2, _>::new(());
    let _guard = outer.lock(token.token());
    let accesses = percpu::accesses();
    drop(wait_condition::WaitCondition::new());
    assert_eq!(percpu::accesses(), accesses);
}
