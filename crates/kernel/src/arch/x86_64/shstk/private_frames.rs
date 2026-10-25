//! Boot reservation and pre-enable writes for shadow-stack frames without physmap aliases.
//!
//! Intel SDM Volume 3A sections 2.5 and 4.6 require CR4.CET to be clear before clearing CR0.WP.
//! Initialization below is one interrupt-masked assembly block, with no calls or branches while
//! protection is disabled. It restores CR0, CR4 and RFLAGS before returning. Once this CPU has
//! a live SSP the caller uses WRSS instead; this path must never disable active shadow stacks.

use core::sync::atomic::{AtomicUsize, Ordering};
use ferric_cet::shstk::pool::{PoolRegion, StackFrame, StackPool, POOL_BYTES};
use rmm::{Arch, BumpAllocator, FrameAllocator, FrameCount};
use spin::{Mutex, Once};

use crate::{
    arch::interrupt::InterruptStack,
    memory::{RmmA, VirtualAddress, PAGE_SIZE},
};

static POOL: Once<Mutex<StackPool>> = Once::new();
static PROBE_LOCK: Mutex<()> = Mutex::new(());
static PROBE_TARGET: AtomicUsize = AtomicUsize::new(0);
static PROBE_IP: AtomicUsize = AtomicUsize::new(0);

/// Reserves the pool with the boot allocator, before the kernel builds its physical map.
///
/// # Panics
///
/// If boot memory cannot hold the reservation. Continuing with writable fallback frames would
/// defeat shadow-stack protection. Called only once, on the BSP before AP startup.
pub(crate) fn reserve_pool(allocator: &mut BumpAllocator<RmmA>) {
    let physical = allocator
        .allocate(FrameCount::new(POOL_BYTES as usize / PAGE_SIZE))
        .unwrap_or_else(|| panic!("CET: cannot reserve {POOL_BYTES} bytes for shadow stacks"));
    let pool = StackPool::new(physical.data() as u64)
        .expect("boot allocator returns a page-aligned addressable reservation");
    POOL.call_once(|| Mutex::new(pool));
    crate::info!(
        "CET: reserved {POOL_BYTES} bytes outside the physical memory map for shadow stacks"
    );
}

fn pool() -> &'static Mutex<StackPool> {
    POOL.get()
        .expect("shadow-stack pool is reserved before building the kernel page tables")
}

/// Physical range omitted from the kernel's physical-memory map.
pub(crate) fn pool_region() -> PoolRegion {
    pool().lock().region()
}

/// Takes one exclusive allocation, with no fallback when the pool is exhausted.
pub(super) fn allocate() -> Option<StackFrame> {
    pool().lock().allocate()
}

/// Returns a retired allocation to this boot's pool.
///
/// # Panics
///
/// If an internal caller supplies a token from a different pool.
pub(super) fn release(frame: StackFrame) {
    pool()
        .lock()
        .release(frame)
        .expect("each shadow stack returns its exclusive token to its original pool once");
}

/// Initializes owned, already mapped entries before this CPU enables shadow stacks.
///
/// All page-table entries stay read-only, so no other CPU can cache a writable translation.
/// The bounds checks and the mapped `ShadowStack` guarantee the assembly cannot page-fault.
///
/// # Panics
///
/// If the entries are unaligned, outside the stack, a shadow stack is live on this CPU, or the
/// CPU protection state is not restored exactly.
pub(super) fn initialize(stack: &super::ShadowStack, addr: usize, value: u64, entries: usize) {
    let range = stack.region();
    let end = entries
        .checked_mul(8)
        .and_then(|bytes| addr.checked_add(bytes));
    assert!(
        addr.is_multiple_of(8) && addr >= range.start && end.is_some_and(|end| end <= range.end)
    );
    assert!(
        super::current_ssp().is_none(),
        "CET: pre-enable write with a live shadow stack"
    );
    let (before_flags, after_flags, before_cr0, after_cr0, before_cr4, after_cr4): (
        usize,
        usize,
        usize,
        usize,
        usize,
        usize,
    );
    // SAFETY: the borrowed stack owns the mapped, aligned range checked above. This CPU has no
    // live shadow stack. The straight-line block masks interrupts, disables CET (including IBT)
    // before WP, performs only the bounded stores, and restores WP before CET and then RFLAGS.
    // It creates no writable PTE or alias, touches no other CPU's state and contains no call/ret.
    unsafe {
        core::arch::asm!(
            "pushfq", "pop r8", "cli",
            "mov r9, cr4", "mov r11, r9", "btr r11, 23", "mov cr4, r11",
            "mov r10, cr0", "mov r11, r10", "btr r11, 16", "mov cr0, r11",
            "cld", "rep stosq",
            "mov cr0, r10", "mov cr4, r9", "push r8", "popfq",
            "mov r11, cr0", "mov rax, cr4", "pushfq", "pop rdx",
            inout("rdi") addr => _, inout("rcx") entries => _,
            inout("rax") value => after_cr4,
            out("r8") before_flags, out("rdx") after_flags,
            out("r9") before_cr4, out("r10") before_cr0, out("r11") after_cr0,
        );
    }
    assert_eq!(before_cr0, after_cr0, "CET: initialization changed CR0");
    assert_eq!(before_cr4, after_cr4, "CET: initialization changed CR4");
    assert_eq!(
        before_flags, after_flags,
        "CET: initialization changed RFLAGS"
    );
}

/// One of the three addresses checked by the isolation self-test.
pub(super) enum ProbeAlias {
    /// The read-only shadow-stack window.
    ShadowStack,
    /// The current (possibly randomized) physical-memory map.
    PhysicalMap,
    /// The bootloader's original physical-memory-map slot.
    BootPhysicalMap,
}

/// Proves an ordinary write faults and leaves the owned shadow-stack entry unchanged.
///
/// # Panics
///
/// If the entry is outside the stack, unaligned, writable through the requested address, or
/// changes during the probe.
pub(super) fn check_write_rejected(
    stack: &super::ShadowStack,
    addr: usize,
    alias: ProbeAlias,
) -> u64 {
    assert!(stack.region().contains(&addr) && addr.is_multiple_of(8));
    let physical = stack
        .physical_base()
        .checked_add(addr.saturating_sub(stack.region().start))
        .expect("checked shadow-stack entry belongs to an addressable pool allocation");
    let target = match alias {
        ProbeAlias::ShadowStack => addr,
        ProbeAlias::PhysicalMap => {
            RmmA::phys_to_virt(crate::memory::PhysicalAddress::new(physical)).data()
        }
        ProbeAlias::BootPhysicalMap => RmmA::PHYS_OFFSET
            .checked_add(physical)
            .expect("bootloader physical-map slot covers the reserved frame"),
    };
    let _probe = PROBE_LOCK.lock();
    PROBE_TARGET.store(target, Ordering::Release);
    let (before, after, faulted): (u64, u64, usize);
    // SAFETY: `addr` is an aligned entry of the borrowed, mapped stack. The bounded probe stores
    // its exact instruction address before attempting one ordinary write to that entry or its
    // unmapped physmap aliases. Only the matching supervisor #PF is recovered by zeroing RCX;
    // REP then completes without a store, retaining RIP and the interrupt shadow frame for CET.
    // The probe is serialized, and no CPU accesses PROBE_IP concurrently with this assembly store.
    unsafe {
        before = RmmA::read::<u64>(VirtualAddress::new(addr));
        core::arch::asm!(
            "lea rdx, [rip + 2f]", "mov [{ip}], rdx",
            "mov ecx, 1", "xor edx, edx",
            "2:", "rep stosq",
            ip = in(reg) PROBE_IP.as_ptr(),
            inout("rdi") target => _, in("rax") 0xBAD0_BAD0_BAD0_BAD0u64,
            lateout("rcx") _, out("rdx") faulted,
        );
        after = RmmA::read::<u64>(VirtualAddress::new(addr));
    }
    PROBE_TARGET.store(0, Ordering::Release);
    assert_eq!(
        faulted, 1,
        "CET: ordinary write to a shadow-stack alias succeeded"
    );
    assert_eq!(
        before, after,
        "CET: ordinary write changed a shadow-stack entry"
    );
    before
}

/// Recovers only the self-test's exact one-entry supervisor write, without changing saved RIP.
pub(crate) fn recover_pool_probe(stack: &mut InterruptStack, code: usize, address: usize) -> bool {
    let target = PROBE_TARGET.load(Ordering::Acquire);
    if target == 0
        || address != target
        || stack.scratch.rdi != target
        || stack.scratch.rcx != 1
        || stack.iret.cs & 3 != 0
        || code & 2 == 0
        || code & (4 | 8 | 16) != 0
        || stack.iret.rip != PROBE_IP.load(Ordering::Acquire)
    {
        return false;
    }
    stack.scratch.rcx = 0;
    stack.scratch.rdx = 1;
    true
}
