//! Ferric Oxide: static keys, the on/off switch of each compiled-in tracepoint (`crate::trace`).
//!
//! `key` is `asm goto`: a 5-byte NOP, and an entry `{site, target, event id}` in the
//! `.ferric_trace_keys` section, which the linker script keeps between
//! `__ferric_trace_keys_start` and `__ferric_trace_keys_end`. While the event is off the NOP falls
//! through and `key` returns `false`. Turning the event on rewrites every site of its id as
//! `jmp target`, where `target` is the asm block's label, so `key` returns `true`; turning it off
//! writes the NOP back. The bytes are written by `alternative::patch_text`, the routine that
//! also applies the CPU-feature alternatives.
//!
//! Before the other CPUs start (`Mode::Boot`) a site is simply overwritten. Once they run,
//! rewriting code another CPU may be executing is cross-modifying code, which the Intel SDM (vol.
//! 3A, "Handling Self- and Cross-Modifying Code") allows only if each executing CPU serializes
//! before running the new code. `Mode::Live` uses the breakpoint protocol, so a CPU sees the
//! old instruction, an `int3`, or the new instruction, never a mix of their bytes:
//!
//! 1. write `int3` (0xCC) over the first byte of every site, then `sync`;
//! 2. write the last four bytes of the new instruction, then `sync`;
//! 3. write its first byte, then `sync`.
//!
//! An `int3` hit at a site in the meantime resumes after the site (`breakpoint`), as if the
//! event were off. `sync` sends every other CPU the `TraceSync` IPI and waits until each has
//! acknowledged it; the interrupt's `iretq` serializes, and the handler also drops the site's page
//! from its TLB, so no CPU keeps the writable mapping `patch_text` made for a moment. While it
//! waits (and while it waits for the patch lock) a CPU answers TLB shootdowns and sync requests
//! itself, since the CPU it waits for may be waiting for it.

use core::{
    arch::asm,
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use ferric_trace::{EventId, EventSet, MAX_EVENTS};
use rmm::Arch;
use spin::{Mutex, MutexGuard};
use x86::bits64::rflags::{self, RFlags};

use super::alternative::{patch_text, NOPS};
use crate::{
    cpu_set::MAX_CPU_COUNT,
    ipi::{ipi, IpiKind, IpiTarget},
    memory::{RmmA, VirtualAddress, PAGE_SIZE},
    percpu::{all_percpu_blocks, PercpuBlock},
};

/// The length of a site: a 5-byte NOP or `jmp rel32`.
const SITE_LEN: usize = 5;

/// The NOP a site holds while its event is off.
const NOP5: &[u8] = NOPS[SITE_LEN - 1];

/// `int3`, the breakpoint instruction.
const INT3: u8 = 0xcc;

/// The opcode of `jmp rel32`.
const JMP_REL32: u8 = 0xe9;

/// One entry of the `.ferric_trace_keys` section, as [`key`] emits it.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Site {
    /// The address of the 5-byte site.
    code: usize,
    /// Where the site jumps while its event is on.
    target: usize,
    /// The event id.
    key: usize,
}

/// Whether event `ID` is on, as a patched-out jump.
///
/// Costs one 5-byte NOP while the event is off.
#[inline(always)]
pub fn key<const ID: u16>() -> bool {
    // SAFETY: the template is a 5-byte NOP, which uses no register, memory, stack or flag, plus a
    // table entry assembled into a section of its own. Patching may turn the NOP into a jump to
    // `on`, a label the compiler set up as a branch target of this very block.
    unsafe {
        asm!(
            "2: .byte 0x0f, 0x1f, 0x44, 0x00, 0x00",
            ".pushsection .ferric_trace_keys, \"a\"",
            ".balign 8",
            ".quad 2b, {on}, {id}",
            ".popsection",
            id = const ID,
            on = label { return true; },
            options(nostack, preserves_flags),
        );
    }
    false
}

/// Every tracepoint site in the kernel.
fn sites() -> &'static [Site] {
    let start = crate::kernel_executable_offsets::__ferric_trace_keys_start();
    let end = crate::kernel_executable_offsets::__ferric_trace_keys_end();
    let len = end
        .saturating_sub(start)
        .checked_div(size_of::<Site>())
        .unwrap_or(0);
    // SAFETY: the linker script puts the `.ferric_trace_keys` entries, and nothing else, between
    // the two symbols, 8-byte aligned in read-only data. Each entry is the three quads `key`
    // emits, relocated with the image by the bootloader, so it is a valid `Site` for as long as the
    // kernel runs.
    unsafe { core::slice::from_raw_parts(start as *const Site, len) }
}

/// The sites of event `id`.
fn sites_of(id: EventId) -> impl Iterator<Item = &'static Site> {
    sites().iter().filter(move |site| site.key == id.index())
}

/// The instruction a site holds while its event is `on`.
fn instruction(site: &Site, on: bool) -> [u8; SITE_LEN] {
    let mut bytes = [0; SITE_LEN];
    if !on {
        bytes.copy_from_slice(NOP5);
        return bytes;
    }
    let next = site.code.wrapping_add(SITE_LEN);
    let rel = i32::try_from(site.target.wrapping_sub(next) as isize)
        .expect("a site and its target are in the same function, within 2 GiB");
    bytes[0] = JMP_REL32;
    if let Some(imm) = bytes.get_mut(1..) {
        imm.copy_from_slice(&rel.to_le_bytes());
    }
    bytes
}

/// Writes `bytes` at `offset` into `site`.
fn write(site: &Site, offset: usize, bytes: &[u8]) {
    // SAFETY: `site.code` is a 5-byte tracepoint site in kernel text, from the linker-built table,
    // and the closure only writes inside it. What it leaves there is valid for every CPU that can
    // run it: before the APs start no other CPU runs, and afterwards `patch_live` only leaves the
    // old instruction, an `int3` that `breakpoint` handles, or the new instruction.
    unsafe {
        patch_text(site.code as *mut u8, SITE_LEN, |code| {
            if let Some(dst) = offset
                .checked_add(bytes.len())
                .and_then(|end| code.get_mut(offset..end))
            {
                dst.copy_from_slice(bytes);
            }
        });
    }
}

/// When a change applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// On the BSP before the other CPUs start: sites are overwritten directly.
    Boot,
    /// Any other time: sites are rewritten with the breakpoint protocol.
    Live,
}

/// Which events are on.
static ON: [AtomicBool; MAX_EVENTS] = [const { AtomicBool::new(false) }; MAX_EVENTS];

/// Serializes patching.
static PATCH_LOCK: Mutex<()> = Mutex::new(());

/// The event whose sites are being patched live, or `usize::MAX`.
static PATCHING: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);

/// The number of the latest sync request.
static SYNC_GEN: AtomicU64 = AtomicU64::new(0);

/// The latest sync request each CPU has acknowledged.
static ACKED: [AtomicU64; MAX_CPU_COUNT as usize] =
    [const { AtomicU64::new(0) }; MAX_CPU_COUNT as usize];

/// Whether event `id` is on.
pub fn is_on(id: EventId) -> bool {
    ON.get(id.index())
        .is_some_and(|on| on.load(Ordering::Relaxed))
}

/// Turns the events in `events` on or off, and returns how many changed.
///
/// [`Mode::Live`] waits for every other CPU to take an interrupt, so it must not be used from an
/// interrupt handler.
pub fn set(events: &EventSet, on: bool, mode: Mode) -> usize {
    let _guard = lock();
    let mut changed = 0usize;
    for id in events.iter() {
        let Some(state) = ON.get(id.index()) else {
            continue;
        };
        if state.load(Ordering::Relaxed) == on {
            continue;
        }
        match mode {
            Mode::Boot => {
                for site in sites_of(id) {
                    write(site, 0, &instruction(site, on));
                }
            }
            Mode::Live => patch_live(id, on),
        }
        state.store(on, Ordering::Relaxed);
        changed = changed.saturating_add(1);
    }
    changed
}

/// Rewrites the sites of `id` with the breakpoint protocol (see the module documentation).
fn patch_live(id: EventId, on: bool) {
    PATCHING.store(id.index(), Ordering::SeqCst);
    for site in sites_of(id) {
        write(site, 0, &[INT3]);
    }
    sync();
    for site in sites_of(id) {
        let new = instruction(site, on);
        write(site, 1, new.get(1..).unwrap_or_default());
    }
    sync();
    for site in sites_of(id) {
        let new = instruction(site, on);
        write(site, 0, new.get(..1).unwrap_or_default());
    }
    sync();
    PATCHING.store(usize::MAX, Ordering::SeqCst);
}

/// Takes the patch lock, answering TLB shootdowns and sync requests while it waits.
fn lock() -> MutexGuard<'static, ()> {
    loop {
        if let Some(guard) = PATCH_LOCK.try_lock() {
            return guard;
        }
        poll();
        core::hint::spin_loop();
    }
}

/// Makes every other CPU acknowledge a new sync request (see the module documentation).
fn sync() {
    if cfg!(not(feature = "multi_core")) {
        // The other CPUs, if any, halt with interrupts off and never run a tracepoint.
        return;
    }
    let generation = SYNC_GEN.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    ack();
    ipi(IpiKind::TraceSync, IpiTarget::Other);
    let me = crate::cpu_id();
    for block in all_percpu_blocks() {
        if block.cpu_id == me {
            continue;
        }
        let Some(acked) = ACKED.get(block.cpu_id.get() as usize) else {
            continue;
        };
        while acked.load(Ordering::Acquire) < generation {
            poll();
            core::hint::spin_loop();
        }
    }
}

/// Acknowledges the latest sync request on this CPU, after dropping the pages of the sites being
/// patched from its TLB. Called from the `TraceSync` IPI and from [`poll`].
fn ack() -> bool {
    let generation = SYNC_GEN.load(Ordering::Acquire);
    let Some(acked) = ACKED.get(crate::cpu_id().get() as usize) else {
        return false;
    };
    if acked.load(Ordering::Relaxed) >= generation {
        return false;
    }
    let patching = PATCHING.load(Ordering::SeqCst);
    for site in sites().iter().filter(|site| site.key == patching) {
        RmmA::invalidate(VirtualAddress::new(site.code & !(PAGE_SIZE - 1)));
    }
    acked.store(generation, Ordering::Release);
    true
}

/// Answers what another CPU may be waiting for while this one spins.
fn poll() {
    PercpuBlock::current().maybe_handle_tlb_shootdown();
    if ack() {
        // The interrupt path serializes with `iretq`; here CPUID does, before this CPU goes on
        // to code that may have been patched.
        let _ = raw_cpuid::native_cpuid::cpuid_count(0, 0);
    }
}

crate::interrupt!(sync_ipi, || {
    ack();
    crate::arch::device::local_apic::eoi();
});

/// Where to resume after a kernel-mode `int3` at `rip - 1` (the CPU reports the address after the
/// instruction), if that is a tracepoint site being patched: after the site, as if its event were
/// off. `None` for every other breakpoint.
pub fn breakpoint(rip: usize, cs: usize) -> Option<usize> {
    if cs & 3 != 0 {
        return None;
    }
    let addr = rip.checked_sub(1)?;
    sites()
        .iter()
        .find(|site| site.code == addr)
        .map(|site| site.code.wrapping_add(SITE_LEN))
}

/// Whether interrupts are enabled on this CPU.
pub fn interrupts_enabled() -> bool {
    rflags::read().contains(RFlags::FLAGS_IF)
}
