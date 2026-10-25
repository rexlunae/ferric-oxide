use crate::{
    // Ferric Oxide: the safe EOI.
    arch::device::local_apic,
    context::{self, switch::drain_ipi_context_wakeups},
    percpu::PercpuBlock,
    sync::CleanLockToken,
};

crate::interrupt!(wakeup, || {
    // Ferric Oxide: the sched.ipi_wake tracepoint (`kernel.trace`): how many
    // contexts this CPU finds on the cross-CPU wakeup list when a Wakeup IPI
    // arrives (the ones the next tick would otherwise have to wait for).
    let mut token = unsafe { CleanLockToken::new() };
    crate::trace::trace_event!(
        ferric_trace::kernel::SCHED_IPI_WAKE,
        PercpuBlock::current()
            .switch_internals
            .ipi_context_wakeup_list
            .lock(token.token())
            .len(),
    );
    drain_ipi_context_wakeups(&mut token);
    local_apic::eoi();
});

crate::interrupt!(tlb, || {
    PercpuBlock::current().maybe_handle_tlb_shootdown();

    local_apic::eoi();
});

crate::interrupt!(switch, || {
    local_apic::eoi();

    let mut token = unsafe { CleanLockToken::new() };
    let _ = context::switch(&mut token);
});

crate::interrupt!(pit, || {
    // Ferric Oxide: the sched.ipi_pit tracepoint (`kernel.trace`): one record per
    // periodic Pit broadcast IPI this CPU receives.
    crate::trace::trace_event!(ferric_trace::kernel::SCHED_IPI_PIT);
    local_apic::eoi();

    // Switch after a sufficient amount of time since the last switch.
    let mut token = unsafe { CleanLockToken::new() };
    context::switch::tick(&mut token);
});
