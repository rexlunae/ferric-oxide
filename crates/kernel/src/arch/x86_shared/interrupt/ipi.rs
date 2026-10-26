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
    // would, and switch nothing: such a CPU has nothing to switch to. The other vectors need
    // no answer here: the Wakeup, Switch and Pit broadcasts only ask a CPU with contexts to
    // reschedule, `irq:` and the IOMMU refuse to aim device interrupts at this CPU
    // (`percpu::interrupt_target`), and features.toml rejects `kernel.trace` with
    // `kernel.profiling`, whose tracepoint sync waits for every CPU's answer.
    let action = context::idle::acknowledge(
        percpu.switch_internals.try_idle_context(),
        |idle| context::is_current(&idle),
        || percpu.maybe_handle_tlb_shootdown(),
        local_apic::eoi,
    );
    // Ferric Oxide (issue #200): the drain moved the wake-ups into this CPU's local list,
    // which only the next `context::switch` processes. On an idling CPU, switch right here
    // so they run without waiting for the idle loop's next pass. The loop would get there
    // too: `sti; hlt` holds interrupts until the `hlt` has executed (Intel SDM, STI), and an
    // interrupt that ends a halt returns past the `hlt` (SDM, HLT), so this handler's
    // `iret` always lands on the loop's back-edge, never on the `hlt`. What the idle loop's
    // own non-empty-list check covers is wake-ups with no interrupt behind them (a
    // contended switch's re-push, a direct switch back): halting on those would leave them
    // until an interrupt that, on an idle tickless system, may never come.
    //
    // A `Contended` result needs no retry here (#214): spinning in this handler with
    // interrupts off could wait on a CPU that is itself waiting for this one, and the loop
    // it returns to switches again and retries a contended switch with interrupts on.
    if action == context::idle::WakeupAction::Schedule
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
