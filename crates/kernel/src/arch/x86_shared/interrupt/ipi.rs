use crate::{
    // Ferric Oxide: the safe EOI.
    arch::device::local_apic,
    context::{self, switch::drain_ipi_context_wakeups},
    percpu::PercpuBlock,
    sync::CleanLockToken,
};

crate::interrupt!(wakeup, || {
    let mut token = unsafe { CleanLockToken::new() };
    drain_ipi_context_wakeups(&mut token);
    let percpu = PercpuBlock::current();
    // Issue #214: `kernel.profiling`'s profiler CPU points every vector from 33 up at this
    // handler, the TLB shootdown vector included, and never sets up contexts, so it has no
    // idle context. Asking it for one panicked; and a shootdown it does not answer leaves its
    // `wants_tlb_shootdown` set, so the next broadcast (`shootdown_tlb_ipi(None)`, used when
    // a shadow stack is freed) spins on it forever. Answer the shootdown, as the `tlb` handler
    // would, and switch nothing: such a CPU has nothing to switch to.
    let Some(idling) = percpu
        .switch_internals
        .try_idle_context()
        .map(|idle| context::is_current(&idle))
    else {
        percpu.maybe_handle_tlb_shootdown();
        local_apic::eoi();
        return;
    };
    local_apic::eoi();
    // Ferric Oxide (issue #200): the drain moved the wake-ups into this CPU's local list,
    // which only the next `context::switch` processes. When this IPI is delivered at the
    // idle loop's `sti` — after the `switch` that found every context idle already drained
    // that list — the loop runs `hlt` with the wake-ups still in it, and on an otherwise
    // idle (tickless) system nothing may interrupt the halt again: the woken contexts stay
    // `Runnable` in no run queue, requests are never served, and every CPU idles forever
    // (the boot-smoke hang). If this CPU is idling, do the switch right here instead of
    // returning into the halt.
    //
    // A `Contended` result needs no retry here (#214): spinning in this handler with
    // interrupts off could wait on a CPU that is itself waiting for this one. The handler
    // returns to the idle loop past its `hlt`, and the loop switches again and, with
    // interrupts disabled, re-checks the local list before it halts.
    if idling
        && !percpu
            .switch_internals
            .local_wakeup_list
            .borrow()
            .is_empty()
    {
        let _ = context::switch(&mut token);
    }
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
    local_apic::eoi();

    // Switch after a sufficient amount of time since the last switch.
    let mut token = unsafe { CleanLockToken::new() };
    context::switch::tick(&mut token);
});
