//! Ferric Oxide: Intel VT-d DMA remapping (`kernel.iommu`).
//!
//! This is the first step of the IOMMU work (io/iommu-design.md in the project files): every
//! VT-d unit the DMAR table lists is switched on with every device in one identity domain, so
//! IOVA equals physical address and DMA behaves as before. Per-process domains come later.
//!
//! For each unit the kernel maps the register set, reads CAP/ECAP, builds (once per address
//! width) second-stage tables that identity-map all of physical memory, the reserved ranges
//! and the RMRRs, plus a root table whose 256 buses all share one context table, and then
//! runs the enable sequence: set the root table pointer (VT-d spec section 10.4.6), invalidate
//! the context cache and IOTLB globally with the register interface (6.5.1, 10.4.7, 10.4.8),
//! turn on translation (10.4.4) and turn off protected memory regions left by firmware
//! (10.4.16). Faults raise the fault event interrupt (`FAULT_VECTOR` on the BSP), whose
//! handler logs and clears the fault records (7.3, 10.4.14).
//!
//! The DMAR parsing, register decoding and table encoding are in the host-tested
//! `ferric-vtd` crate (`crates/vtd`).

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use ferric_vtd::{
    dmar::{Dmar, Drhd, Structure},
    fault::{self, FaultRecord, FaultStatus},
    plan::{self, SkipReason, UnitPlan, IDENTITY_DOMAIN},
    regs::{self, ccmd, global, iotlb, pmen, Capability, ExtendedCapability},
    table::{self, Agaw, ContextEntry, MapError, PageSize, RootEntry, Table, TableMemory},
};
use spin::Once;

use crate::{
    arch::device::local_apic,
    memory::{allocate_frame, map_device_memory, PhysicalAddress, RmmA, RmmArch, PAGE_SIZE},
};

/// The IDT vector of the fault event interrupt, reserved on the BSP (see `idt.rs`).
pub const FAULT_VECTOR: u8 = 0x45;

/// How many times to read a status register before giving up on a command.
const POLL_LIMIT: u32 = 10_000_000;

/// The units that were enabled, for the fault handler.
static UNITS: Once<Vec<Unit>> = Once::new();

/// A mapped VT-d register set.
struct Unit {
    base: u64,
    regs: usize,
    cap: Capability,
}

impl Unit {
    /// Reads a 32- or 64-bit register.
    fn read<T: Copy>(&self, offset: usize) -> T {
        // SAFETY: `regs` maps the unit's whole register span as device memory (see `map_unit`)
        // and every offset used is a register inside it, aligned to its width.
        unsafe { read_volatile((self.regs as *const u8).wrapping_add(offset).cast::<T>()) }
    }

    /// Writes a 32- or 64-bit register.
    fn write<T: Copy>(&self, offset: usize, value: T) {
        // SAFETY: as in `read`.
        unsafe {
            write_volatile(
                (self.regs as *mut u8).wrapping_add(offset).cast::<T>(),
                value,
            )
        }
    }

    /// Waits until `done` returns true for the value of the 32- or 64-bit register at `offset`.
    fn wait<T: Copy>(
        &self,
        offset: usize,
        step: Step,
        done: impl Fn(T) -> bool,
    ) -> Result<(), Error> {
        for _ in 0..POLL_LIMIT {
            if done(self.read(offset)) {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(step))
    }

    /// Issues a one-shot or enable command through GCMD and waits for GSTS to show it.
    fn command(&self, bit: u32, step: Step) -> Result<(), Error> {
        let gsts: u32 = self.read(regs::GSTS);
        self.write(regs::GCMD, global::command(gsts, bit));
        self.wait(regs::GSTS, step, |s: u32| s & bit != 0)
    }
}

/// A step of the enable sequence, for timeout messages.
#[derive(Clone, Copy, Debug)]
enum Step {
    RootTablePointer,
    WriteBufferFlush,
    ContextInvalidation,
    IotlbInvalidation,
    TranslationEnable,
    ProtectedMemoryDisable,
}

/// Why a unit was not enabled.
#[derive(Debug)]
enum Error {
    Skip(SkipReason),
    NoMemory,
    Map(MapError<NoMemory>),
    Timeout(Step),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Skip(reason) => write!(f, "{reason}"),
            Self::NoMemory => f.write_str("out of memory for translation tables"),
            Self::Map(err) => write!(f, "{err}"),
            Self::Timeout(step) => write!(f, "hardware did not complete {step:?}"),
        }
    }
}

/// The frame allocator had no page for a translation table.
#[derive(Clone, Copy, Debug)]
struct NoMemory;

impl core::fmt::Display for NoMemory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("out of memory")
    }
}

/// Translation tables in frames from the kernel's allocator, reached through the physical
/// memory map. Remembers every page so they can be flushed from the CPU caches.
#[derive(Default)]
struct KernelTables {
    pages: Vec<u64>,
}

impl TableMemory for KernelTables {
    type Error = NoMemory;

    fn alloc_table(&mut self) -> Result<u64, NoMemory> {
        let frame = allocate_frame(true).ok_or(NoMemory)?;
        let phys = frame.base().data() as u64;
        self.pages.push(phys);
        Ok(phys)
    }

    fn table(&mut self, phys: u64) -> &mut Table {
        let virt = RmmA::phys_to_virt(PhysicalAddress::new(phys as usize)).data();
        // SAFETY: `phys` is a zeroed frame from `alloc_table`, owned by these tables for the
        // life of the kernel and mapped through the physical memory map; the `&mut self`
        // borrow keeps this the only reference.
        unsafe { &mut *(virt as *mut Table) }
    }
}

impl KernelTables {
    /// Writes the tables out of the CPU caches for units whose page walks do not snoop them
    /// (ECAP.C clear).
    fn flush_cpu_caches(&self) {
        for &phys in &self.pages {
            let virt = RmmA::phys_to_virt(PhysicalAddress::new(phys as usize)).data();
            for line in (0..PAGE_SIZE).step_by(64) {
                // SAFETY: `virt + line` is inside a mapped table page; clflush only writes the
                // line back to memory.
                unsafe {
                    core::arch::asm!("clflush [{}]", in(reg) virt.wrapping_add(line), options(nostack, preserves_flags));
                }
            }
        }
        // On x86 this is MFENCE, which orders the CLFLUSHes before the register writes that
        // follow.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }
}

/// A root table for one address width and page size, shared by the units that use it.
struct Domain {
    agaw: Agaw,
    page_size: PageSize,
    root_table: u64,
}

/// Builds the identity domain: second-stage tables for `[0, plan.top)`, one context table
/// whose 256 entries all use them, and a root table whose 256 buses all use that.
fn build_domain(plan: &UnitPlan, tables: &mut KernelTables) -> Result<Domain, Error> {
    let slpt = tables.alloc_table().map_err(|_| Error::NoMemory)?;
    table::map_identity(tables, slpt, plan.agaw, 0, plan.top, plan.page_size)
        .map_err(Error::Map)?;

    let context = ContextEntry::new(
        slpt,
        plan.agaw,
        IDENTITY_DOMAIN,
        table::TranslationType::Untranslated,
    );
    let context_table = tables.alloc_table().map_err(|_| Error::NoMemory)?;
    for pair in tables.table(context_table).chunks_exact_mut(2) {
        pair.copy_from_slice(&context.0);
    }
    let root = RootEntry::new(context_table);
    let root_table = tables.alloc_table().map_err(|_| Error::NoMemory)?;
    for pair in tables.table(root_table).chunks_exact_mut(2) {
        pair.copy_from_slice(&root.0);
    }
    Ok(Domain {
        agaw: plan.agaw,
        page_size: plan.page_size,
        root_table,
    })
}

/// Maps `len` bytes of a unit's register set at `phys` as device memory.
fn map_registers(phys: PhysicalAddress, len: usize) -> usize {
    // SAFETY: `phys` is inside a register set named by a DRHD, which only this module maps,
    // and mapping device memory has no other requirements.
    unsafe { map_device_memory(phys, len) }.data()
}

/// Maps a unit's registers and reads its capabilities.
fn map_unit(drhd: &Drhd<'_>) -> Unit {
    let base = drhd.register_base;
    let regs = map_registers(PhysicalAddress::new(base as usize), PAGE_SIZE);
    let mut unit = Unit {
        base,
        regs,
        cap: Capability(0),
    };
    unit.cap = Capability(unit.read(regs::CAP));
    let ecap = ExtendedCapability(unit.read(regs::ECAP));
    let span = regs::register_span(unit.cap, ecap, drhd.register_pages());
    if let Some(rest) = span.checked_sub(PAGE_SIZE).filter(|&rest| rest > 0) {
        map_registers(PhysicalAddress::new(base as usize).add(PAGE_SIZE), rest);
    }
    unit
}

/// Runs the enable sequence on one unit with the identity domain's root table.
fn enable(unit: &Unit, plan: &UnitPlan, root_table: u64) -> Result<(), Error> {
    unit.write(regs::RTADDR, root_table | regs::RTADDR_TTM_LEGACY);
    unit.command(global::SRTP, Step::RootTablePointer)?;
    if plan.flush_write_buffer {
        let gsts: u32 = unit.read(regs::GSTS);
        unit.write(regs::GCMD, global::command(gsts, global::WBF));
        unit.wait(regs::GSTS, Step::WriteBufferFlush, |s: u32| {
            s & global::WBF == 0
        })?;
    }
    unit.write(regs::CCMD, ccmd::global());
    unit.wait(regs::CCMD, Step::ContextInvalidation, |v: u64| {
        v & ccmd::ICC == 0
    })?;
    let ecap = ExtendedCapability(unit.read(regs::ECAP));
    let iotlb_reg = ecap.iotlb_invalidate_offset();
    unit.write(iotlb_reg, iotlb::global(unit.cap));
    unit.wait(iotlb_reg, Step::IotlbInvalidation, |v: u64| {
        v & iotlb::IVT == 0
    })?;
    unit.command(global::TE, Step::TranslationEnable)?;

    // Protected memory regions block DMA on their own; with translation on they are no longer
    // needed (10.4.16).
    let pmen_reg: u32 = unit.read(regs::PMEN);
    if pmen_reg & pmen::EPM != 0 {
        unit.write(regs::PMEN, pmen_reg & !pmen::EPM);
        unit.wait(regs::PMEN, Step::ProtectedMemoryDisable, |v: u32| {
            v & pmen::PRS == 0
        })?;
    }
    Ok(())
}

/// Routes the unit's fault event interrupt to the BSP, after clearing any old faults.
fn enable_fault_interrupt(unit: &Unit) {
    log_faults(unit);
    let Some(apic_id) = crate::percpu::PercpuBlock::current()
        .misc_arch_info
        .apic_id_opt
        .get()
    else {
        warn!(
            "IOMMU: BSP APIC ID unknown; faults at unit {:#x} are not reported",
            unit.base
        );
        return;
    };
    let apic_id = apic_id.get();
    match u8::try_from(apic_id) {
        Ok(id) => {
            unit.write(regs::FEDATA, u32::from(FAULT_VECTOR));
            unit.write(regs::FEADDR, fault::event_address(id));
            unit.write(regs::FEUADDR, 0u32);
            // Clearing FECTL.IM unmasks the interrupt.
            unit.write(regs::FECTL, 0u32);
        }
        Err(_) => {
            warn!(
                "IOMMU: BSP APIC ID {apic_id} needs interrupt remapping; faults at unit {:#x} are not reported",
                unit.base
            );
        }
    }
}

/// Logs and clears every fault recorded by `unit`.
fn log_faults(unit: &Unit) {
    let status = FaultStatus(unit.read(regs::FSTS));
    if status.pending() {
        for rec in fault::records(unit.cap, status) {
            if let Some(record) = FaultRecord::decode(unit.read(rec.low), unit.read(rec.high)) {
                error!("IOMMU: DMA fault at unit {:#x}: {record}", unit.base);
                // Writing F back as 1 frees the record; the other bits are read-only.
                unit.write(rec.f_dword, fault::RECORD_F_DWORD);
            }
        }
    }
    if status.overflow() {
        error!(
            "IOMMU: unit {:#x} lost faults (fault records full)",
            unit.base
        );
    }
    if status.clear_value() != 0 {
        unit.write(regs::FSTS, status.clear_value());
    }
}

interrupt!(fault_interrupt, || {
    for unit in UNITS.get().into_iter().flatten() {
        log_faults(unit);
    }
    // SAFETY: this runs in an interrupt delivered through the local APIC.
    unsafe { local_apic::the_local_apic().eoi() };
});

/// The highest physical address the identity map must cover: the end of RAM or of any RMRR,
/// whichever is higher. Everything below it, holes included, is mapped.
fn identity_top(dmar: &Dmar<'_>) -> u64 {
    let mut top = crate::startup::memory::ram_end() as u64;
    for rmrr in dmar.structures().filter_map(|s| match s {
        Ok(Structure::Rmrr(rmrr)) => Some(rmrr),
        _ => None,
    }) {
        match rmrr.end() {
            Some(end) => {
                info!(
                    "IOMMU: RMRR {:#x}-{:#x} identity-mapped",
                    rmrr.base, rmrr.limit
                );
                top = top.max(end);
            }
            None => {
                warn!("IOMMU: ignoring empty RMRR at {:#x}", rmrr.base);
            }
        }
    }
    top
}

/// Sets up one unit, reusing a matching domain from `domains` or building one.
fn init_unit(
    drhd: &Drhd<'_>,
    top: u64,
    domains: &mut Vec<Domain>,
    tables: &mut KernelTables,
) -> Result<Unit, (u64, Error)> {
    let unit = map_unit(drhd);
    let fail = |e| (drhd.register_base, e);
    let ecap = ExtendedCapability(unit.read(regs::ECAP));
    let plan = plan::plan_unit(unit.cap, ecap, unit.read(regs::GSTS), top)
        .map_err(|e| fail(Error::Skip(e)))?;
    let existing = domains
        .iter()
        .find(|d| d.agaw == plan.agaw && d.page_size == plan.page_size)
        .map(|d| d.root_table);
    let root_table = match existing {
        Some(root) => root,
        None => {
            let domain = build_domain(&plan, tables).map_err(fail)?;
            let root = domain.root_table;
            domains.push(domain);
            root
        }
    };
    if plan.flush_cpu_caches {
        tables.flush_cpu_caches();
    }
    enable(&unit, &plan, root_table).map_err(fail)?;
    info!(
        "IOMMU: VT-d unit at {:#x} enabled (identity domain)",
        drhd.register_base
    );
    Ok(unit)
}

/// Finds the DMAR table and enables every VT-d unit in it. Must run after ACPI tables are
/// parsed and before any user-space driver starts DMA.
pub fn init() {
    let Some(sdt) = crate::acpi::find_sdt("DMAR").into_iter().next() else {
        info!("IOMMU: no DMAR table, VT-d not enabled");
        return;
    };
    // SAFETY: `get_sdt` mapped the whole table, `length` bytes from its header, and firmware
    // tables stay mapped and unchanged for the life of the kernel.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (sdt as *const crate::acpi::sdt::Sdt).cast::<u8>(),
            sdt.length as usize,
        )
    };
    let dmar = match Dmar::parse(bytes) {
        Ok(dmar) => dmar,
        Err(err) => {
            warn!("IOMMU: DMAR table is invalid ({err}), VT-d not enabled");
            return;
        }
    };
    let top = identity_top(&dmar);
    let mut domains = Vec::new();
    let mut tables = KernelTables::default();
    let mut units = Vec::new();
    for structure in dmar.structures() {
        match structure {
            Ok(Structure::Drhd(drhd)) => match init_unit(&drhd, top, &mut domains, &mut tables) {
                Ok(unit) => units.push(unit),
                Err((base, err)) => {
                    warn!("IOMMU: VT-d unit at {base:#x} skipped: {err}");
                }
            },
            Ok(_) => {}
            Err(err) => {
                warn!("IOMMU: stopped reading the DMAR table: {err}");
                break;
            }
        }
    }
    // The fault handler reads UNITS, so faults are routed only once it is set.
    for unit in UNITS.call_once(|| units) {
        enable_fault_interrupt(unit);
    }
}
