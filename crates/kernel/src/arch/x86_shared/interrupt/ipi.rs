use crate::{
    arch::device::local_apic::the_local_apic,
    context::{self, switch::drain_ipi_context_wakeups},
    percpu::PercpuBlock,
    sync::CleanLockToken,
};

/// Ferric Oxide: acknowledges the IPI being handled at the local APIC. Private to these handlers,
/// which all run with interrupts off on the CPU whose APIC they acknowledge.
fn eoi() {
    // SAFETY: only called from the IPI handlers below, with interrupts disabled, so nothing else
    // on this CPU holds the local APIC reference while it is used; writing EOI touches only the
    // APIC's EOI register (or the x2APIC EOI MSR).
    unsafe { the_local_apic().eoi() };
}

interrupt!(wakeup, || {
    let mut token = unsafe { CleanLockToken::new() };
    drain_ipi_context_wakeups(&mut token);
    eoi();
});

interrupt!(tlb, || {
    PercpuBlock::current().maybe_handle_tlb_shootdown();

    eoi();
});

interrupt!(switch, || {
    eoi();

    let mut token = unsafe { CleanLockToken::new() };
    let _ = context::switch(&mut token);
});

interrupt!(pit, || {
    eoi();

    // Switch after a sufficient amount of time since the last switch.
    let mut token = unsafe { CleanLockToken::new() };
    context::switch::tick(&mut token);
});
