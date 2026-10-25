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
//! is `CR3 | 0x1000` ([`SHADOW_CR3_BIT`](crate::arch::kpti::SHADOW_CR3_BIT)). `rmm` copies every change to the user half of the
//! kernel table into the shadow ([`rmm::PageMapper::new_with_user_shadow`]). The kernel half of
//! every shadow points at one shared set of tables (`SHADOW`) built at boot.
//!
//! Entry and exit (see `interrupt/handler.rs` and `interrupt/syscall.rs`): an interrupt from user
//! space arrives on the entry stack (the TSS's ring-0 stack) with the shadow loaded. The entry code
//! switches to the kernel table and copies the interrupt frame to the context's kernel stack
//! (`kernel_rsp` in the PCR). The return to user space copies the frame back to the entry stack,
//! loads the shadow and returns. System calls switch tables right after saving the user stack.
//! NMIs, machine checks and debug exceptions can arrive at any point, so they save and restore
//! CR3 themselves.
//!
//! Global pages are turned off while KPTI is on: a global kernel entry would be visible to user
//! space under any table. Without PCID every table switch therefore flushes the whole TLB, twice
//! per system call or interrupt.
//!
//! The `kpti` CPU feature (`arch.x86_64.features.kpti` in the kernel config, `kernel.cpu.kpti` in
//! `ferric.toml`) chooses: `never` builds it out, `always` turns it on, `auto` turns it on for
//! Intel CPUs that do not report `RDCL_NO` in `IA32_ARCH_CAPABILITIES`. AMD CPUs are not affected.
//!
//! # PCID
//!
//! With process-context identifiers (CR4.PCIDE), TLB entries are tagged with the PCID in
//! `CR3[11:0]`, and a CR3 write with bit 63 set keeps them. KPTI then uses one fixed pair of PCIDs
//! on every CPU: 0 for every kernel table and [`rmm::x86_64::USER_PCID`] (`0x800`) for every
//! shadow. The entry and exit code switch between the two with bit 63 set, so system calls and
//! interrupts no longer flush the TLB. The CR3 bits they OR in come from the PCR
//! (`kpti_cr3_noflush`, `kpti_cr3_user`), which [`setup_pcr`](crate::arch::kpti::setup_pcr) fills in, so the same code runs with
//! or without PCID.
//!
//! Keeping the user PCID's entries is only correct if every invalidation reaches them too, since
//! `invlpg` and a CR3 write only act on the current PCID, which in the kernel is always 0.
//! `rmm`'s `X8664Arch` does this when [`rmm::x86_64::X8664Arch::set_user_pcid`] is on:
//!
//! - `invalidate` (one page, every `PageFlush::flush`) adds `INVPCID` type 0 for the user PCID.
//! - `set_table` (address-space switches, and `invalidate_all`, which every `PageFlushAll`, the
//!   kernel's `Flusher` and the TLB shootdown IPI handler use) adds `INVPCID` type 1, a flush of
//!   the whole user PCID.
//!
//! The kernel never writes CR3 or runs `invlpg` other than through those two functions (the AP
//! trampoline, [`init_cpu`](crate::arch::kpti::init_cpu) and the entry code aside, none of which needs to invalidate), so no
//! flush site is missed and no flush is ever deferred. The paranoid NMI, #MC and #DB paths can
//! therefore also switch back without flushing. Since `INVPCID` with a non-zero PCID faults
//! while CR4.PCIDE is clear, an AP runs [`init_cpu`](crate::arch::kpti::init_cpu) before anything that could invalidate.
//!
//! Address-space switches still flush, both PCIDs, because every address space shares the same
//! pair. This is the simple design: Linux instead gives each CPU a small cache of PCID pairs with
//! a generation counter, so switching back to a recently run process keeps its entries. That
//! needs per-CPU tracking of which address spaces' pairs are stale, reached from every flush
//! site and the shootdown code, and gets wrong-PCID bugs that are security holes. A fixed pair
//! gives the main gain, no flushes on entry and exit within a process, with no extra state: a
//! switch flushes exactly what it did before. Per-address-space PCIDs can come later, with
//! benchmarks to justify them.
//!
//! The `pcid` CPU feature (`arch.x86_64.features.pcid`, `kernel.cpu.pcid`) chooses: `never` keeps
//! the flushing switches, `auto` uses PCID when KPTI is on and the CPU reports both PCID
//! (CPUID.1:ECX\[17\]) and INVPCID (CPUID.(7,0):EBX\[10\]), and `always` also panics at boot when
//! KPTI is on and the CPU lacks them.

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

/// Whether KPTI uses PCIDs; decided in [`init_bsp`].
static PCID: AtomicBool = AtomicBool::new(false);

/// CR3 bit 63 with CR4.PCIDE: keep the TLB entries of the PCID being loaded.
const CR3_NOFLUSH: usize = 1 << 63;

/// The PCID bits of CR3.
const CR3_PCID_MASK: usize = 0xFFF;

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

/// Whether KPTI with PCID was compiled in.
const fn pcid_compiled() -> bool {
    cfg!(cpu_feature_always = "pcid") || cfg!(cpu_feature_auto = "pcid")
}

/// Whether KPTI uses PCIDs, so entry and exit switch tables without flushing.
#[inline]
pub fn pcid_enabled() -> bool {
    pcid_compiled() && PCID.load(Ordering::Relaxed)
}

/// Whether this CPU has PCID (CPUID.1:ECX\[17\]) and INVPCID (CPUID.(7,0):EBX\[10\]).
fn cpu_has_pcid() -> bool {
    crate::arch::cpuid::feature_info().has_pcid()
        && crate::arch::cpuid::has_ext_feat(|f| f.has_invpcid())
}

/// Decides whether KPTI uses PCIDs, on the BSP once KPTI is known to be on.
///
/// # Panics
///
/// When the `pcid` feature is `always` and the CPU lacks PCID or INVPCID.
fn decide_pcid() -> bool {
    if !pcid_compiled() {
        return false;
    }
    let has = cpu_has_pcid();
    if cfg!(cpu_feature_always = "pcid") && !has {
        panic!(
            "KPTI: the CPU lacks PCID or INVPCID; rebuild with kernel.cpu.pcid = \"auto\" or \"never\""
        );
    }
    PCID.store(has, Ordering::Relaxed);
    has
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
    let pcid = decide_pcid();
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
    // describe live structures; see below for the user PCID.
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
        // `init_cpu` just set CR4.PCIDE on the BSP, which has INVPCID, and every AP sets it in
        // `init_cpu` before it can invalidate a TLB entry.
        if pcid {
            RmmA::set_user_pcid(true);
        }
    }
    crate::info!("KPTI: user page tables isolated from the kernel");
    if pcid {
        crate::info!("KPTI: PCID enabled, no-flush CR3 switches");
    }
}

/// Per-CPU part of the setup: turns off global pages so a switch to the shadow flushes kernel
/// TLB entries and, with PCID, sets CR4.PCIDE. Runs on every CPU.
///
/// # Safety
///
/// Must run on the CPU being set up, before it enters user space, before it invalidates any TLB
/// entry once the BSP has turned PCID on (an AP runs it right after installing its GDT and IDT),
/// and before CR4.CET is set (`x86::controlregs::cr4` drops that bit; see `misc::init`).
///
/// # Panics
///
/// When KPTI uses PCID and this CPU, unlike the BSP, lacks PCID or INVPCID.
pub unsafe fn init_cpu() {
    if !enabled() {
        return;
    }
    let pcid = pcid_enabled();
    assert!(
        !pcid || cpu_has_pcid(),
        "KPTI: this CPU lacks PCID or INVPCID, unlike the BSP"
    );
    use x86::controlregs::{cr3, cr3_write, cr4, cr4_write, Cr4};
    // SAFETY: global pages are only an optimization; clearing PGE also flushes all global TLB
    // entries, which is what isolation needs. CR4.PCIDE may only be set while CR3[11:0] is 0: the
    // kernel table's PCID bits are 0, but clear them first in case firmware left PWT/PCD set
    // (directly, since `RmmA::set_table` may already use INVPCID). Every TLB entry so far is
    // tagged PCID 0, the kernel's, so nothing stale survives into the user PCID.
    unsafe {
        let cr4 = cr4().difference(Cr4::CR4_ENABLE_GLOBAL_PAGES);
        cr4_write(cr4);
        if pcid {
            let table = cr3();
            if table & CR3_PCID_MASK as u64 != 0 {
                cr3_write(table & !(CR3_PCID_MASK as u64));
            }
            cr4_write(cr4 | Cr4::CR4_ENABLE_PCID);
        }
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
    let (noflush, user_pcid) = if pcid_enabled() {
        (CR3_NOFLUSH, rmm::x86_64::USER_PCID)
    } else {
        (0, 0)
    };
    // SAFETY: the caller guarantees `pcr` is valid and not yet used by another CPU.
    unsafe {
        (*pcr).kpti_cr3_noflush = noflush;
        (*pcr).kpti_cr3_user = SHADOW_CR3_BIT | user_pcid | noflush;
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
