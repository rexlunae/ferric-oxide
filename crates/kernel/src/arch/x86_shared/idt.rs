use core::cell::SyncUnsafeCell;

use alloc::boxed::Box;

use x86::{
    dtables::{self, DescriptorTablePointer},
    segmentation::Descriptor as X86IdtEntry,
};

use crate::{
    arch::{
        gdt::ProcessorControlRegion,
        interrupt::{
            irq::{__generic_interrupts_end, __generic_interrupts_start},
            *,
        },
        ipi::IpiKind,
    },
    context::context::Kstack,
    cpu_set::LogicalCpuId,
};

// Ferric Oxide: page-aligned, so KPTI can map an IDT into user page tables without exposing
// whatever else would share its pages.
#[repr(C, align(4096))]
pub struct Idt {
    pub(crate) entries: [IdtEntry; 256],
    // TODO: track reservations in userspace instead, with a daemon that sends various capabilities
    reservations: [u32; 8],
}

impl Idt {
    const fn new() -> Self {
        Self {
            entries: [IdtEntry::new(); 256],
            reservations: [0; 8],
        }
    }

    #[inline]
    fn is_reserved(&self, index: u8) -> bool {
        let byte_index = index / 32;
        let bit = index % 32;

        self.reservations[usize::from(byte_index)] & (1 << bit) != 0
    }

    #[inline]
    pub(crate) fn set_reserved(&mut self, index: u8, reserved: bool) {
        // Ferric Oxide: this used to shift twice (`1 << (1 << (index % 32))`), so the IPI
        // vectors 0x40-0x43 were left free for user IRQ handles while unrelated
        // vectors were marked reserved.
        let bit = index % 32;
        let word = &mut self.reservations[usize::from(index / 32)];

        if reserved {
            *word |= 1 << bit;
        } else {
            *word &= !(1 << bit);
        }
    }
}

static INIT_BSP_IDT: SyncUnsafeCell<Idt> = SyncUnsafeCell::new(Idt::new());

#[inline]
pub fn is_reserved(cpu_id: LogicalCpuId, index: u8) -> bool {
    if cpu_id == LogicalCpuId::BSP {
        return unsafe { (*INIT_BSP_IDT.get()).is_reserved(index) };
    }

    crate::percpu::get_percpu_block(cpu_id)
        .unwrap()
        .misc_arch_info
        .idt
        .lock()
        .as_ref()
        .unwrap()
        .is_reserved(index)
}

#[inline]
pub fn set_reserved(cpu_id: LogicalCpuId, index: u8, reserved: bool) {
    if cpu_id == LogicalCpuId::BSP {
        unsafe { (*INIT_BSP_IDT.get()).set_reserved(index, reserved) };
        return;
    }

    crate::percpu::get_percpu_block(cpu_id)
        .unwrap()
        .misc_arch_info
        .idt
        .lock()
        .as_mut()
        .unwrap()
        .set_reserved(index, reserved)
}

pub fn available_irqs_iter(cpu_id: LogicalCpuId) -> impl Iterator<Item = u8> + 'static {
    (32..=254).filter(move |&index| !is_reserved(cpu_id, index))
}

fn set_exceptions(idt: &mut [IdtEntry]) {
    // Set up exceptions
    idt[0].set_func(exception::divide_by_zero);
    idt[1].set_func(exception::debug);
    idt[2].set_func(exception::non_maskable);
    idt[3].set_func(exception::breakpoint);
    idt[3].set_flags(IdtFlags::PRESENT | IdtFlags::RING_3 | IdtFlags::INTERRUPT);
    idt[4].set_func(exception::overflow);
    idt[5].set_func(exception::bound_range);
    idt[6].set_func(exception::invalid_opcode);
    idt[7].set_func(exception::device_not_available);
    idt[8].set_func(exception::double_fault);
    // 9 no longer available
    idt[10].set_func(exception::invalid_tss);
    idt[11].set_func(exception::segment_not_present);
    idt[12].set_func(exception::stack_segment);
    idt[13].set_func(exception::protection);
    idt[14].set_func(exception::page);
    // 15 reserved
    idt[16].set_func(exception::fpu_fault);
    idt[17].set_func(exception::alignment_check);
    idt[18].set_func(exception::machine_check);
    idt[19].set_func(exception::simd);
    idt[20].set_func(exception::virtualization);
    // Ferric Oxide (CET): control protection exception, raised by IBT and by shadow stacks, so
    // installed when either is built in. Without it a #CP would escalate to a double fault.
    #[cfg(target_arch = "x86_64")]
    if (crate::arch::ibt::COMPILED || crate::arch::shstk::COMPILED)
        && let Some(entry) = idt.get_mut(21)
    {
        entry.set_func(exception::control_protection);
    }
    // 22 through 29 reserved
    idt[30].set_func(exception::security);
    // 31 reserved
}

/// Initializes a fully functional IDT for use before it be moved into the map. This is ONLY called
/// on the BSP, since the kernel heap is ready for the APs.
pub unsafe fn init_bsp_before_mem() {
    unsafe {
        init_generic(LogicalCpuId::BSP, &mut *INIT_BSP_IDT.get());

        install_idt(&mut *INIT_BSP_IDT.get(), LogicalCpuId::BSP);
    }
}
#[cold]
pub unsafe fn init_bsp_after_mem() {
    let kstack = Kstack::new().expect("failed to allocate backup stack");

    // SAFETY: on the BSP during boot, so `pcr()` is the BSP's PCR, the CPU that loads this IDT;
    // the stack is forgotten below.
    unsafe {
        install_backup_stack(&mut *INIT_BSP_IDT.get(), crate::arch::gdt::pcr(), &kstack);
    }
    core::mem::forget(kstack);
}

// Ferric Oxide: takes the AP's PCR, so its backup stack goes in the AP's TSS rather than the
// running CPU's.
pub fn allocate_and_init_idt(cpu_id: LogicalCpuId, pcr: *mut ProcessorControlRegion) -> *mut Idt {
    let idt = Box::leak(Box::new(Idt::new()));

    let backup_stack = Kstack::new().expect("failed to allocate AP's backup interrupt stack");
    let backup_stack_end = backup_stack.initial_top() as usize;

    init_generic(cpu_id, idt);
    // SAFETY: the caller passes the PCR of the AP that will load this IDT, which has not started
    // yet; the stack is forgotten below.
    unsafe {
        install_backup_stack(idt, pcr, &backup_stack);
    }
    core::mem::forget(backup_stack);
    #[cfg(target_arch = "x86_64")]
    {
        crate::arch::kpti::map_idt(idt as *mut Idt as usize);
        crate::arch::kpti::map_stack(backup_stack_end);
    }

    idt
}

const BACKUP_IST: u8 = 1;

// Ferric Oxide (shadow stacks): IST indices for the three fatal vectors. With shadow stacks in
// use they get separate ISTs, so each has its own stack and its own supervisor shadow-stack
// token; a nested delivery (for example #MC inside an NMI) then lands on a different shadow stack
// rather than finding the shared token already busy and escalating to a triple fault. Without
// shadow stacks all three share IST 1, exactly as before.
const NMI_IST: u8 = 1;
#[cfg(target_arch = "x86_64")]
const DOUBLE_FAULT_IST: u8 = 2;
#[cfg(target_arch = "x86_64")]
const MACHINE_CHECK_IST: u8 = 3;

/// Initializes an IDT for any type of processor.
fn init_generic(cpu_id: LogicalCpuId, idt: &mut Idt) {
    let (current_idt, current_reservations) = (&mut idt.entries, &mut idt.reservations);

    set_exceptions(current_idt);

    // Ferric Oxide: the stub size depends on IBT (see irq.rs).
    assert_eq!(
        __generic_interrupts_end as *const () as usize
            - __generic_interrupts_start as *const () as usize,
        224 * irq::GENERIC_INTERRUPT_STUB_SIZE
    );

    for i in 0..224 {
        current_idt[i + 32].set_addr(
            __generic_interrupts_start as *const () as usize + i * irq::GENERIC_INTERRUPT_STUB_SIZE,
        );
    }

    // reserve bits 31:0, i.e. the first 32 interrupts, which are reserved for exceptions
    current_reservations[0] |= 0x0000_0000_FFFF_FFFF;

    if cpu_id == LogicalCpuId::BSP {
        // Set up IRQs
        current_idt[32].set_func(irq::pit_stack);
        current_idt[33].set_func(irq::keyboard);
        current_idt[34].set_func(irq::cascade);
        current_idt[35].set_func(irq::com2);
        current_idt[36].set_func(irq::com1);
        current_idt[37].set_func(irq::lpt2);
        current_idt[38].set_func(irq::floppy);
        current_idt[39].set_func(irq::lpt1);
        current_idt[40].set_func(irq::rtc);
        current_idt[41].set_func(irq::pci1);
        current_idt[42].set_func(irq::pci2);
        current_idt[43].set_func(irq::pci3);
        current_idt[44].set_func(irq::mouse);
        current_idt[45].set_func(irq::fpu);
        current_idt[46].set_func(irq::ata1);
        current_idt[47].set_func(irq::ata2);
        current_idt[48].set_func(irq::lapic_timer);
        current_idt[49].set_func(irq::lapic_error);

        // reserve bits 49:32, which are for the standard IRQs, and for the local apic timer and error.
        current_reservations[1] |= 0x0003_FFFF;

        // Ferric Oxide: the VT-d fault event interrupt (arch/x86_64/iommu.rs).
        #[cfg(all(target_arch = "x86_64", iommu_enabled))]
        {
            use crate::arch::iommu::{fault_interrupt, FAULT_VECTOR};
            if let Some(entry) = current_idt.get_mut(usize::from(FAULT_VECTOR)) {
                entry.set_func(fault_interrupt);
            }
            if let Some(word) = current_reservations.get_mut(usize::from(FAULT_VECTOR / 32)) {
                *word |= 1 << (FAULT_VECTOR % 32);
            }
        }
    } else {
        // TODO: use_default_irqs! but also the legacy IRQs that are only needed on one CPU
        current_idt[49].set_func(irq::lapic_error);

        // reserve bit 49
        current_reservations[1] |= 1 << 17;
    }

    // Set IPI handlers
    current_idt[IpiKind::Wakeup as usize].set_func(ipi::wakeup);
    current_idt[IpiKind::Switch as usize].set_func(ipi::switch);
    current_idt[IpiKind::Tlb as usize].set_func(ipi::tlb);
    current_idt[IpiKind::Pit as usize].set_func(ipi::pit);
    idt.set_reserved(IpiKind::Wakeup as u8, true);
    idt.set_reserved(IpiKind::Switch as u8, true);
    idt.set_reserved(IpiKind::Tlb as u8, true);
    idt.set_reserved(IpiKind::Pit as u8, true);
    // Ferric Oxide: see arch/x86_64/static_key.rs.
    #[cfg(all(feature = "trace", target_arch = "x86_64"))]
    {
        // The sync IPI must not take over the IOMMU's fault vector on the BSP.
        #[cfg(iommu_enabled)]
        const _: () = assert!(IpiKind::TraceSync as u8 != crate::arch::iommu::FAULT_VECTOR);
        if let Some(entry) = idt.entries.get_mut(IpiKind::TraceSync as usize) {
            entry.set_func(crate::arch::static_key::sync_ipi);
        }
        idt.set_reserved(IpiKind::TraceSync as u8, true);
    }

    #[cfg(target_arch = "x86")]
    {
        let current_idt = &mut idt.entries;
        // Set syscall function
        current_idt[0x80].set_func(syscall::syscall);
        current_idt[0x80].set_flags(IdtFlags::PRESENT | IdtFlags::RING_3 | IdtFlags::INTERRUPT);
        idt.set_reserved(0x80, true);
    }

    #[cfg(feature = "profiling")]
    crate::profiling::maybe_setup_timer(idt, cpu_id);
}

pub unsafe fn install_idt(idt_ptr: *mut Idt, cpu_id: LogicalCpuId) {
    unsafe {
        let idt = &mut *idt_ptr;

        let idtr: DescriptorTablePointer<X86IdtEntry> = DescriptorTablePointer {
            limit: (idt.entries.len() * size_of::<IdtEntry>() - 1) as u16,
            base: idt.entries.as_ptr() as *const X86IdtEntry,
        };

        dtables::lidt(&idtr);
    }
    if cpu_id != LogicalCpuId::BSP {
        let mut idt_guard = crate::percpu::get_percpu_block(cpu_id)
            .unwrap()
            .misc_arch_info
            .idt
            .lock();
        assert!(
            idt_guard.is_none(),
            "already initialized cpu {cpu_id:?}'s IDT"
        );
        *idt_guard = Some(unsafe { &mut *idt_ptr });
    }
}
/// Gives NMIs, double faults and machine checks a dedicated interrupt stack, in `idt` and in the
/// TSS of `pcr`. By default all three share `backup` (IST 1); with CET shadow stacks in use, the
/// double fault and machine check get their own freshly allocated stacks (IST 2 and 3), so each
/// of the three has its own shadow stack and a nested fatal exception cannot find the shared
/// supervisor shadow-stack token busy.
///
/// # Safety
///
/// `pcr` must point to the processor control region of the CPU that will load `idt`, and no
/// other CPU may be using it. `backup` must never be freed (callers forget it).
#[cold]
pub unsafe fn install_backup_stack(
    idt: &mut Idt,
    pcr: *mut ProcessorControlRegion,
    backup: &Kstack,
) {
    // We give Non-Maskable Interrupts, Double Fault, and Machine Check exceptions separate
    // stacks, since these (unless we are going to set up NMI watchdogs like Linux does) are
    // considered the most fatal, especially Double Faults which are caused by errors __when
    // accessing the system IDT__. If that goes wrong, then kernel memory may be partially
    // corrupt, and we want a separate stack.
    //
    // Note that each CPU has its own "backup interrupt stack".

    // Whether the three fatal vectors get separate ISTs. Only with shadow stacks in use; the
    // default build keeps the historical shared IST 1 exactly.
    #[cfg(target_arch = "x86_64")]
    let separate = cfg!(shstk_compiled) && {
        #[cfg(shstk_compiled)]
        {
            crate::arch::shstk::decision()
        }
        #[cfg(not(shstk_compiled))]
        {
            false
        }
    };

    // NMI always uses the passed `backup` stack on IST 1.
    idt.entries[2].set_ist(NMI_IST);

    #[cfg(target_arch = "x86_64")] // TODO: x86
    // SAFETY: the caller guarantees `pcr` is the PCR of the CPU loading `idt` and unused
    // elsewhere.
    unsafe {
        install_one_ist(&mut *pcr, NMI_IST, backup);

        if separate {
            // Double fault and machine check get their own stacks and shadow stacks, each
            // freshly allocated and leaked (never freed, like `backup`). Mapped into the KPTI
            // shadow tables (a no-op without KPTI, and the BSP's are also mapped from its TSS in
            // kpti::init_bsp); the CPU reads an IST stack while the user page table is loaded.
            idt.entries[8].set_ist(DOUBLE_FAULT_IST);
            idt.entries[18].set_ist(MACHINE_CHECK_IST);
            for (ist, vector) in [
                (DOUBLE_FAULT_IST, "double fault"),
                (MACHINE_CHECK_IST, "machine check"),
            ] {
                let stack = Kstack::new()
                    .unwrap_or_else(|_| panic!("failed to allocate the {vector} IST stack"));
                crate::arch::kpti::map_stack(stack.initial_top() as usize);
                install_one_ist(&mut *pcr, ist, &stack);
                core::mem::forget(stack);
            }
        } else {
            idt.entries[8].set_ist(NMI_IST);
            idt.entries[18].set_ist(NMI_IST);
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        idt.entries[8].set_ist(NMI_IST);
        idt.entries[18].set_ist(NMI_IST);
    }
}

/// Points IST index `ist` (1 to 7) at `stack`'s top in `pcr`'s TSS, and, with shadow stacks, at
/// its shadow stack's supervisor token in the interrupt SSP table.
///
/// # Panics
///
/// If `ist` is 0, which is not an IST index (the callers pass the `*_IST` constants).
#[cfg(target_arch = "x86_64")]
fn install_one_ist(pcr: &mut ProcessorControlRegion, ist: u8, stack: &Kstack) {
    let slot = ist.checked_sub(1).expect("IST indices start at 1");
    pcr.tss.ist[usize::from(slot)] = stack.initial_top() as u64;
    #[cfg(shstk_compiled)]
    crate::arch::shstk::set_ist_ssp(&mut pcr.shstk_ist_ssp, ist, stack.shadow_stack());
}

bitflags::bitflags! {
    pub struct IdtFlags: u8 {
        const PRESENT = 1 << 7;
        const RING_0 = 0 << 5;
        const RING_1 = 1 << 5;
        const RING_2 = 2 << 5;
        const RING_3 = 3 << 5;
        const SS = 1 << 4;
        const INTERRUPT = 0xE;
        const TRAP = 0xF;
    }
}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct IdtEntry {
    offsetl: u16,
    selector: u16,
    zero: u8,
    attribute: u8,
    offsetm: u16,
    #[cfg(target_arch = "x86_64")]
    offseth: u32,
    #[cfg(target_arch = "x86_64")]
    _zero2: u32,
}

impl IdtEntry {
    pub const fn new() -> IdtEntry {
        IdtEntry {
            offsetl: 0,
            selector: 0,
            zero: 0,
            attribute: 0,
            offsetm: 0,
            #[cfg(target_arch = "x86_64")]
            offseth: 0,
            #[cfg(target_arch = "x86_64")]
            _zero2: 0,
        }
    }

    pub fn set_flags(&mut self, flags: IdtFlags) {
        self.attribute = flags.bits();
    }

    pub fn set_ist(&mut self, ist: u8) {
        assert_eq!(
            ist & 0x07,
            ist,
            "interrupt stack table must be within 0..=7"
        );
        self.zero &= 0xF8;
        self.zero |= ist;
    }

    pub fn set_offset(&mut self, selector: u16, base: usize) {
        self.selector = selector;
        self.offsetl = base as u16;
        self.offsetm = (base >> 16) as u16;
        #[cfg(target_arch = "x86_64")]
        {
            self.offseth = ((base as u64) >> 32) as u32;
        }
    }

    // A function to set the offset more easily
    pub fn set_func(&mut self, func: unsafe extern "C" fn()) {
        self.set_addr(func as usize);
    }

    /// Ferric Oxide: points a present ring-0 interrupt gate at the kernel code address `addr`,
    /// for entry stubs that are not Rust functions (such as the generic interrupt stubs).
    pub fn set_addr(&mut self, addr: usize) {
        self.set_flags(IdtFlags::PRESENT | IdtFlags::RING_0 | IdtFlags::INTERRUPT);
        self.set_offset((crate::arch::gdt::GDT_KERNEL_CODE as u16) << 3, addr);
    }
}
