//! Ferric Oxide: kernel page-table isolation (KPTI) for x86_64.
//!
//! On CPUs affected by Meltdown (rogue data cache load, CVE-2017-5754), user code can read any
//! kernel memory that is mapped in its page table, even without permission. KPTI gives every
//! address space a second top-level table, the *shadow*, that is loaded while user space runs.
//! The shadow maps the same user memory but almost none of the kernel: only what the CPU and the
//! entry code touch before they can switch tables.
//!
//! - The kernel's text, so interrupt and system call entry points can run. Kernel code is public;
//!   it reveals the kernel's address (KASLR), as Linux's entry text does.
//! - Each CPU's processor control region (GDT, TSS, per-CPU block), its IDT, its interrupt stacks
//!   (IST) and a per-CPU *entry stack*.
//!
//! Layout: an address space's kernel table and shadow are one 8 KiB-aligned pair, so the shadow
//! is `CR3 | 0x1000` ([`SHADOW_CR3_BIT`]). `rmm` copies every change to the user half of the
//! kernel table into the shadow ([`rmm::PageMapper::new_with_user_shadow`]). The kernel half of
//! every shadow points at one shared set of tables ([`SHADOW`]) built at boot.
//!
//! Entry and exit (see `interrupt/handler.rs` and `interrupt/syscall.rs`): an interrupt from user
//! space arrives on the entry stack (the TSS's ring-0 stack) with the shadow loaded. The entry code
//! switches to the kernel table and copies the interrupt frame to the context's kernel stack
//! (`kernel_rsp` in the PCR). The return to user space copies the frame back to the entry stack,
//! loads the shadow and returns. System calls switch tables right after saving the user stack.
//! NMIs, machine checks and debug exceptions can arrive at any point, so they save and restore
//! CR3 themselves.
//!
//! Global pages are turned off while KPTI is on, so a table switch flushes the kernel's TLB
//! entries. PCID, which would avoid flushing on every switch, is not used yet.
//!
//! The `kpti` CPU feature (`arch.x86_64.features.kpti` in the kernel config, `kernel.cpu.kpti` in
//! `ferric.toml`) chooses: `never` builds it out, `always` turns it on, `auto` turns it on for
//! Intel CPUs that do not report `RDCL_NO` in `IA32_ARCH_CAPABILITIES`. AMD CPUs are not affected.

use core::sync::atomic::{AtomicBool, Ordering};

use rmm::{Arch, PageFlags, PhysicalAddress, TableKind, VirtualAddress};
use spin::Mutex;
use syscall::NumaMemoryPolicy;

use crate::{
    arch::gdt::ProcessorControlRegion,
    context::context::Kstack,
    memory::{
        allocate_p2frame, Frame, KernelMapper, PageMapper, RmmA, TheFrameAllocator, PAGE_SIZE,
    },
};

/// Set in CR3 while the shadow table is loaded: the shadow is the page after the kernel table.
pub const SHADOW_CR3_BIT: usize = 1 << 12;

/// Frame order of a kernel/shadow table pair (two pages, 8 KiB-aligned).
pub const TABLE_PAIR_ORDER: u32 = 1;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// The kernel half shared by all shadows: a top-level table whose upper entries every shadow
/// copies. Its lower half is unused.
static SHADOW: Mutex<Option<PageMapper>> = Mutex::new(None);

const ENTRIES: usize = RmmA::PAGE_ENTRIES;

/// Whether KPTI was compiled in.
const fn compiled() -> bool {
    cfg!(cpu_feature_always = "kpti") || cfg!(cpu_feature_auto = "kpti")
}

/// Whether KPTI is on.
#[inline]
pub fn enabled() -> bool {
    compiled() && ENABLED.load(Ordering::Relaxed)
}

/// Whether this CPU can leak kernel memory to user space through Meltdown: Intel CPUs, unless
/// they report `RDCL_NO`.
pub fn cpu_vulnerable() -> bool {
    let intel = crate::arch::cpuid::cpuid()
        .get_vendor_info()
        .is_some_and(|v| v.as_str() == "GenuineIntel");
    if !intel {
        return false;
    }
    // CPUID.(EAX=7,ECX=0):EDX[29] says IA32_ARCH_CAPABILITIES exists.
    let edx = core::arch::x86_64::__cpuid_count(7, 0).edx;
    if edx & (1 << 29) == 0 {
        return true;
    }
    const IA32_ARCH_CAPABILITIES: u32 = 0x10A;
    const RDCL_NO: u64 = 1;
    // SAFETY: CPUID says the MSR exists.
    let caps = unsafe { x86::msr::rdmsr(IA32_ARCH_CAPABILITIES) };
    caps & RDCL_NO == 0
}

/// Decides whether KPTI is on, on the bootstrap processor before any other CPU starts. Returns
/// the decision.
pub fn decide() -> bool {
    let on = cfg!(cpu_feature_always = "kpti") || (compiled() && cpu_vulnerable());
    ENABLED.store(on, Ordering::Relaxed);
    on
}

/// Builds the shared shadow kernel half and maps the bootstrap processor's entry structures.
/// Runs on the BSP after [`decide`] and before user space starts.
///
/// # Safety
///
/// Must run once, on the BSP, with its PCR, IDT and interrupt stacks installed.
pub unsafe fn init_bsp() {
    if !enabled() {
        return;
    }
    // Pre-allocates every kernel-half table, so all shadows can share them.
    // SAFETY: a fresh table, not loaded anywhere until a shadow uses its kernel half.
    let mapper = unsafe {
        PageMapper::create(
            TableKind::Kernel,
            TheFrameAllocator(NumaMemoryPolicy::NodeLocalLeniant),
        )
    }
    .expect("KPTI: no memory for the shadow kernel half");
    *SHADOW.lock() = Some(mapper);

    use crate::kernel_executable_offsets::{__text_end, __text_start};
    map(__text_start(), __text_end(), PageFlags::new().execute(true));

    // SAFETY: the caller guarantees the BSP's PCR and IDT are installed, so `pcr()` and SIDT
    // describe live structures.
    unsafe {
        let pcr = crate::arch::gdt::pcr();
        setup_pcr(pcr);
        // The BSP's IDT is a static; map the one that is loaded.
        let mut idtr = x86::dtables::DescriptorTablePointer::<u64> {
            limit: 0,
            base: core::ptr::null(),
        };
        x86::dtables::sidt(&mut idtr);
        map_idt(idtr.base as usize);
        let ists = core::ptr::addr_of!((*pcr).tss.ist).read_unaligned();
        for ist in ists {
            if ist != 0 {
                map_stack(ist as usize);
            }
        }
        init_cpu();
    }
    info!("KPTI: user page tables isolated from the kernel");
}

/// Per-CPU part of the setup: turns off global pages so a switch to the shadow flushes kernel
/// TLB entries. Runs on every CPU.
///
/// # Safety
///
/// Must run on the CPU being set up, before it enters user space.
pub unsafe fn init_cpu() {
    if !enabled() {
        return;
    }
    // SAFETY: global pages are only an optimization; clearing PGE also flushes all global TLB
    // entries, which is what isolation needs.
    unsafe {
        let cr4 = x86::controlregs::cr4();
        x86::controlregs::cr4_write(cr4.difference(x86::controlregs::Cr4::CR4_ENABLE_GLOBAL_PAGES));
    }
}

/// Maps a PCR in the shadow and gives it an entry stack. Call after `init_pcr`.
///
/// # Safety
///
/// `pcr` must point to a PCR that is not in use by another CPU yet, or to the current CPU's.
pub unsafe fn setup_pcr(pcr: *mut ProcessorControlRegion) {
    if !enabled() {
        return;
    }
    let stack = Kstack::new().expect("KPTI: no memory for an entry stack");
    let top = stack.initial_top() as usize;
    core::mem::forget(stack);
    map_stack(top);
    // SAFETY: the caller guarantees `pcr` is valid and not yet used by another CPU.
    unsafe {
        (*pcr).kpti_entry_top = top;
        core::ptr::addr_of_mut!((*pcr).tss.rsp[0]).write_unaligned(top as u64);
    }
    let start = pcr as usize;
    map(
        start,
        start.wrapping_add(size_of::<ProcessorControlRegion>()),
        PageFlags::new().write(true),
    );
}

/// Maps an IDT in the shadow; the CPU reads it while delivering an interrupt.
pub fn map_idt(base: usize) {
    if enabled() {
        let end = base.wrapping_add(size_of::<crate::arch::idt::Idt>());
        map(base, end, PageFlags::new());
    }
}

/// Maps a kernel stack (an IST or entry stack) whose top is `top` in the shadow.
pub fn map_stack(top: usize) {
    if enabled() {
        const KSTACK_LEN: usize = PAGE_SIZE << 4;
        map(
            top.wrapping_sub(KSTACK_LEN),
            top,
            PageFlags::new().write(true),
        );
    }
}

/// Maps the kernel pages covering `start..end` into the shadow kernel half, at the same address.
fn map(start: usize, end: usize, flags: PageFlags<RmmA>) {
    let mut shadow = SHADOW.lock();
    let shadow = shadow.as_mut().expect("KPTI: shadow kernel half not built");
    let kernel = KernelMapper::lock_ro();
    let first = start & !(PAGE_SIZE - 1);
    for page in (first..end).step_by(PAGE_SIZE) {
        let virt = VirtualAddress::new(page);
        let phys = kernel_phys(&kernel, virt);
        // SAFETY: the shadow is never loaded with this page missing, and mapping the page at its
        // kernel address keeps every address the entry code uses valid.
        unsafe {
            shadow
                .map_phys(virt, phys, flags, 0)
                .expect("KPTI: no memory for shadow page tables")
                .ignore();
        }
    }
}

/// Physical address of a kernel page. The heap and the kernel image are mapped with 4 KiB pages,
/// so the page table says; the physical memory map below them uses huge pages, so compute it.
fn kernel_phys(kernel: &KernelMapper<false>, virt: VirtualAddress) -> PhysicalAddress {
    if virt.data() >= crate::arch::consts::kernel_heap_offset() {
        kernel
            .translate(virt)
            .expect("KPTI: kernel page not mapped")
            .0
    } else {
        let base = RmmA::phys_to_virt(PhysicalAddress::new(0)).data();
        PhysicalAddress::new(virt.data().wrapping_sub(base))
    }
}

/// Allocates the table pair for a new user address space: a kernel table with the usual kernel
/// half, followed by its shadow with the isolated one. Both user halves start empty.
///
/// Returns `None` when out of memory.
pub fn new_user_table() -> Option<PageMapper> {
    let shadow = SHADOW.lock();
    let shadow = shadow.as_ref().expect("KPTI: shadow kernel half not built");
    let pair = allocate_p2frame(TABLE_PAIR_ORDER, true)?.base();
    let current = RmmA::phys_to_virt(RmmA::table(TableKind::Kernel)).data() as *const usize;
    let shadow_top = RmmA::phys_to_virt(shadow.table().phys()).data() as *const usize;
    let kernel_page = RmmA::phys_to_virt(pair).data() as *mut usize;
    // SAFETY: the pair was just allocated and is two pages; the sources are live top-level tables.
    unsafe {
        let shadow_page = kernel_page.add(ENTRIES);
        for i in ENTRIES / 2..ENTRIES {
            kernel_page.add(i).write(current.add(i).read());
            shadow_page.add(i).write(shadow_top.add(i).read());
        }
        Some(PageMapper::new_with_user_shadow(
            TableKind::User,
            pair,
            TheFrameAllocator(NumaMemoryPolicy::NodeLocalLeniant),
        ))
    }
}

/// Frees a table pair from [`new_user_table`].
///
/// # Safety
///
/// The pair must not be loaded on any CPU and its user half must be empty.
pub unsafe fn free_user_table(table: PhysicalAddress) {
    // SAFETY: the caller guarantees the pair is unused; it was allocated with this order.
    unsafe { crate::memory::deallocate_p2frame(Frame::containing(table), TABLE_PAIR_ORDER) };
}
