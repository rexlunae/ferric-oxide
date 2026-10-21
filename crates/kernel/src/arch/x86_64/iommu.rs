//! Ferric Oxide: IOMMU DMA remapping (`kernel.iommu`): Intel VT-d, and AMD-Vi in `amdvi`.
//!
//! [`init`](crate::arch::iommu::init) picks the vendor by which ACPI table firmware provides: the DMAR table means VT-d
//! (this file), the IVRS table AMD-Vi (`amdvi.rs`). Both put devices in the same kinds of
//! domain, so [`domain`](crate::arch::iommu::domain), [`dma`](crate::arch::iommu::dma) and the `iommu` scheme work the same on either; only
//! interrupt remapping ([`interrupt`](mod@crate::arch::iommu::interrupt)) is VT-d-only so far.
//!
//! Every VT-d unit the DMAR table lists is switched on with every device in one identity
//! domain, so IOVA equals physical address and DMA behaves as before (io/iommu-design.md in
//! the project files, step 1). [`domain`](crate::arch::iommu::domain) adds DMA domains with their own tables that
//! devices can be moved into and out of (step 2), which the boot self-test (`selftest`)
//! checks and user space gets through the `iommu` scheme (`scheme/iommu.rs`, step 3).
//!
//! For each unit the kernel maps the register set, reads CAP/ECAP, builds (once per address
//! width) second-stage tables that identity-map all of physical memory, the reserved ranges
//! and the RMRRs, plus one context table whose 256 entries all use them. Each unit gets its
//! own root table, whose 256 buses all start out pointing at that shared context table; a
//! bus gets a context table of its own the first time a device on it changes domain. The
//! enable sequence sets the root table pointer (VT-d spec section 10.4.6), switches to queued
//! invalidation when the unit has it (ECAP.QI, 6.5.2), invalidates the context cache and
//! IOTLB globally, turns on translation (10.4.4) and turns off protected memory regions left
//! by firmware (10.4.16). Units without queued invalidation use the register interface
//! (6.5.1, 10.4.7, 10.4.8). Faults raise the fault event interrupt (`FAULT_VECTOR` on the
//! BSP), whose handler logs and clears the fault records (7.3, 10.4.14).
//!
//! Units that support it also remap interrupts ([`interrupt`](mod@crate::arch::iommu::interrupt), step 5): MSI and MSI-X get
//! remappable-format messages through the `iommu` scheme, and compatibility-format interrupt
//! requests are blocked.
//!
//! The DMAR parsing, register decoding, table and descriptor encoding and the IOVA allocator
//! are in the host-tested `ferric-vtd` crate (`crates/vtd`).

mod amdvi;
pub mod dma;
pub mod domain;
pub mod interrupt;
mod selftest;
mod volatile;

use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{fence, Ordering};

use ferric_vtd::{
    dmar::{Dmar, DmarFlags, Drhd, Structure},
    fault::{self, FaultRecord, FaultStatus},
    invalidation::{Descriptor, Queue, QueueError, QueueFull, QueueSize, RegisterCommand, Request},
    iova::DomainIds,
    plan::{self, DomainLayout, SkipReason, UnitPlan, FIRST_DMA_DOMAIN, IDENTITY_DOMAIN},
    regs::{self, global, pmen, Capability, ExtendedCapability},
    table::{self, Agaw, ContextEntry, MapError, PageSize, RootEntry, Table, TableMemory},
    Bdf,
};
use spin::{Mutex, Once};

use self::volatile::{Volatile, Word};

use crate::{
    arch::device::local_apic,
    memory::{
        allocate_frame, deallocate_frame, map_device_memory, Frame, PhysicalAddress, RmmA, RmmArch,
        PAGE_SIZE,
    },
};

/// The IDT vector of the fault event interrupt, reserved on the BSP (see `idt.rs`).
pub const FAULT_VECTOR: u8 = 0x45;

/// How many times to read a status register before giving up on a command.
const POLL_LIMIT: u32 = 10_000_000;

/// The units that were enabled, for the fault handler and DMA domains.
static UNITS: Once<Vec<Unit>> = Once::new();

/// The DMAR table, for finding the unit that serves a device.
static DMAR: Once<Dmar<'static>> = Once::new();

/// The table layout DMA domains use, common to every enabled unit.
static LAYOUT: Once<DomainLayout> = Once::new();

/// Domain IDs for DMA domains.
static DOMAIN_IDS: Mutex<Option<DomainIds>> = Mutex::new(None);

/// A mapped VT-d register set.
struct Regs {
    base: u64,
    mmio: Volatile,
    cap: Capability,
    ecap: ExtendedCapability,
}

impl Regs {
    /// Reads a 32- or 64-bit register.
    fn read<T: Word>(&self, offset: usize) -> T {
        self.mmio.read(offset)
    }

    /// Writes a 32- or 64-bit register.
    fn write<T: Word>(&self, offset: usize, value: T) {
        self.mmio.write(offset, value);
    }

    /// Waits until `done` returns true for the value of the 32- or 64-bit register at `offset`.
    fn wait<T: Word>(
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

    /// Flushes the unit's write buffer (GCMD.WBF), for units that need it (CAP.RWBF).
    fn flush_write_buffer(&self) -> Result<(), Error> {
        if !self.cap.required_write_buffer_flush() {
            return Ok(());
        }
        let gsts: u32 = self.read(regs::GSTS);
        self.write(regs::GCMD, global::command(gsts, global::WBF));
        self.wait(regs::GSTS, Step::WriteBufferFlush, |s: u32| {
            s & global::WBF == 0
        })
    }

    /// Performs `requests` in order through the register interface.
    fn invalidate_registers(&self, requests: &[Request]) -> Result<(), Error> {
        for request in requests {
            match request.register_command(self.cap) {
                Some(RegisterCommand::Context(command)) => {
                    self.write(regs::CCMD, command);
                    self.wait(regs::CCMD, Step::ContextInvalidation, |v: u64| {
                        v & regs::ccmd::ICC == 0
                    })?;
                }
                Some(RegisterCommand::Iotlb { iva, command }) => {
                    if let Some(iva) = iva {
                        self.write(self.ecap.iotlb_offset(), iva);
                    }
                    let reg = self.ecap.iotlb_invalidate_offset();
                    self.write(reg, command);
                    self.wait(reg, Step::IotlbInvalidation, |v: u64| {
                        v & regs::iotlb::IVT == 0
                    })?;
                }
                None => return Err(Error::NeedsQueue),
            }
        }
        Ok(())
    }
}

/// An enabled unit.
struct Unit {
    regs: Regs,
    plan: UnitPlan,
    state: Mutex<UnitState>,
}

/// The parts of a unit that change after it is enabled.
struct UnitState {
    /// The unit's root table and the context tables of buses split off the shared one.
    tables: KernelTables,
    root_table: u64,
    /// The identity domain's context table, shared by every bus not in `split_buses`.
    shared_context: u64,
    /// The identity domain's context entry, which every device starts with.
    identity: ContextEntry,
    /// Buses with a context table of their own, and that table.
    split_buses: BTreeMap<u8, u64>,
    invalidator: Invalidator,
    /// The interrupt remapping table, when the unit remaps interrupts.
    interrupts: Option<interrupt::RemapTable>,
}

impl Unit {
    /// Performs `requests` in order and waits until all are done.
    fn invalidate(&self, state: &mut UnitState, requests: &[Request]) -> Result<(), Error> {
        self.regs.flush_write_buffer()?;
        match &mut state.invalidator {
            Invalidator::Registers => self.regs.invalidate_registers(requests),
            Invalidator::Queued(queue) => queue.submit(&self.regs, requests),
        }
    }

    /// Writes table lines out of the CPU caches when this unit's page walks do not snoop.
    fn flush_lines(&self, phys: u64, len: usize) {
        if self.plan.flush_cpu_caches {
            clflush(phys, len);
        }
    }
}

/// How a unit is told to drop cached entries.
enum Invalidator {
    /// The register interface (no ECAP.QI).
    Registers,
    /// The invalidation queue.
    Queued(QueuedInvalidation),
}

/// An enabled invalidation queue: one page of descriptors plus a page whose first word wait
/// descriptors write.
struct QueuedInvalidation {
    queue: Queue,
    pages: KernelTables,
    ring: u64,
    status: u64,
    /// The status value the next wait descriptor writes, so a late write from an earlier
    /// batch is never mistaken for this one.
    sequence: u32,
    /// Coherent with the CPU caches (ECAP.C).
    coherent: bool,
    /// Set after a queue error: hardware stopped at a descriptor and the queue is not used
    /// again.
    broken: bool,
}

impl QueuedInvalidation {
    /// Allocates the queue pages and switches `regs`'s unit to queued invalidation (6.5.2,
    /// 10.4.23, 10.4.24).
    fn enable(regs: &Regs) -> Result<Self, Error> {
        let mut pages = KernelTables::default();
        let ring = pages.alloc_table().map_err(|_| Error::NoMemory)?;
        let status = pages.alloc_table().map_err(|_| Error::NoMemory)?;
        let queue = Queue::new(QueueSize::ONE_PAGE);
        if !regs.ecap.coherent() {
            pages.flush_cpu_caches();
        }
        regs.write(regs::IQT, 0u64);
        regs.write(regs::IQA, queue.iqa(ring));
        regs.command(global::QIE, Step::QueueEnable)?;
        Ok(Self {
            queue,
            pages,
            ring,
            status,
            sequence: 0,
            coherent: regs.ecap.coherent(),
            broken: false,
        })
    }

    /// Queues `requests` followed by a wait descriptor, and waits for its status write.
    fn submit(&mut self, regs: &Regs, requests: &[Request]) -> Result<(), Error> {
        if self.broken {
            return Err(Error::QueueBroken);
        }
        self.sequence = self.sequence.wrapping_add(1).max(1);
        let wait = Descriptor::wait(self.status, self.sequence);
        let descriptors = requests
            .iter()
            .map(|r| r.descriptor(regs.cap))
            .chain([wait]);
        let count = u32::try_from(requests.len().saturating_add(1)).unwrap_or(u32::MAX);
        let slots = self
            .queue
            .reserve(regs.read(regs::IQH), count)
            .map_err(Error::QueueFull)?;
        if let Some(word) = self.pages.table(self.status).first_mut() {
            *word = 0;
        }
        for (slot, descriptor) in slots.zip(descriptors) {
            let slot = slot as usize;
            if let Some(entry) = self.pages.table(self.ring).chunks_exact_mut(2).nth(slot) {
                entry.copy_from_slice(&descriptor.0);
            }
            if !self.coherent {
                clflush(self.ring.wrapping_add((slot as u64).wrapping_mul(16)), 16);
            }
        }
        if !self.coherent {
            clflush(self.status, 8);
        }
        // The descriptors must reach memory before hardware sees the new tail.
        fence(Ordering::SeqCst);
        regs.write(regs::IQT, self.queue.tail_register());
        self.wait_status(regs)
    }

    /// Polls the status word until the wait descriptor has written it.
    fn wait_status(&mut self, regs: &Regs) -> Result<(), Error> {
        // The status word is the first of a table page this queue owns; hardware writes it.
        let status = Volatile::frames(self.status, 8);
        for _ in 0..POLL_LIMIT {
            let word: u64 = status.read(0);
            if word as u32 == self.sequence {
                return Ok(());
            }
            let fsts: u32 = regs.read(regs::FSTS);
            if let Some(err) = QueueError::from_fault_status(fsts) {
                self.broken = true;
                regs.write(regs::FSTS, fsts & QueueError::FSTS_BITS);
                return Err(Error::Queue(err));
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(Step::InvalidationWait))
    }
}

/// A step of the enable sequence or of an invalidation, for timeout messages.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    RootTablePointer,
    WriteBufferFlush,
    QueueEnable,
    ContextInvalidation,
    IotlbInvalidation,
    InvalidationWait,
    TranslationEnable,
    ProtectedMemoryDisable,
    InterruptTablePointer,
    InterruptRemappingEnable,
    CompatibilityBlock,
    /// An AMD-Vi COMPLETION_WAIT.
    CompletionWait,
}

/// Why a unit was not enabled, or an invalidation failed.
#[derive(Debug)]
pub enum Error {
    Skip(SkipReason),
    NoMemory,
    Map(MapError<NoMemory>),
    Timeout(Step),
    Queue(QueueError),
    QueueFull(QueueFull),
    QueueBroken,
    NeedsQueue,
    /// Why an AMD-Vi unit was not enabled.
    AmdViSkip(ferric_amdvi::plan::SkipReason),
    /// An AMD-Vi command buffer had too few free slots.
    CommandsFull(ferric_iommu::ring::RingFull),
    /// An AMD-Vi command buffer stopped (CmdBufRun clear) after an illegal command or a
    /// hardware error; the unit takes no more commands.
    CommandsStopped,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Skip(reason) => write!(f, "{reason}"),
            Self::NoMemory => f.write_str("out of memory for translation tables"),
            Self::Map(err) => write!(f, "{err}"),
            Self::Timeout(step) => write!(f, "hardware did not complete {step:?}"),
            Self::Queue(err) => write!(f, "invalidation queue error: {err}"),
            Self::QueueFull(err) => write!(f, "{err}"),
            Self::QueueBroken => f.write_str("invalidation queue stopped after an error"),
            Self::NeedsQueue => f.write_str("invalidation needs queued invalidation"),
            Self::AmdViSkip(reason) => write!(f, "{reason}"),
            Self::CommandsFull(err) => write!(f, "command buffer: {err}"),
            Self::CommandsStopped => f.write_str("the command buffer stopped after an error"),
        }
    }
}

/// The frame allocator had no page for a translation table.
#[derive(Clone, Copy, Debug)]
pub struct NoMemory;

impl core::fmt::Display for NoMemory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("out of memory")
    }
}

/// Writes `len` bytes from physical address `phys` out of the CPU caches, for units whose
/// page walks do not snoop them (ECAP.C clear), then fences so the flushes are ordered
/// before the register writes that follow.
fn clflush(phys: u64, len: usize) {
    let virt = RmmA::phys_to_virt(PhysicalAddress::new(phys as usize)).data();
    let first = virt & !63;
    let end = virt.saturating_add(len);
    for line in (first..end).step_by(64) {
        // SAFETY: `line` is inside a mapped table page; clflush only writes the line back to
        // memory.
        unsafe {
            core::arch::asm!("clflush [{}]", in(reg) line, options(nostack, preserves_flags));
        }
    }
    // On x86 this is MFENCE.
    fence(Ordering::SeqCst);
}

/// Translation tables in frames from the kernel's allocator, reached through the physical
/// memory map. Remembers every page so they can be flushed from the CPU caches and freed.
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
        // SAFETY: `phys` is a zeroed frame from `alloc_table`, owned by these tables (or, for
        // the identity tables, by the kernel) and mapped through the physical memory map; the
        // `&mut self` borrow keeps this the only reference.
        unsafe { &mut *(virt as *mut Table) }
    }
}

impl KernelTables {
    /// Writes every page out of the CPU caches.
    fn flush_cpu_caches(&self) {
        for &phys in &self.pages {
            clflush(phys, PAGE_SIZE);
        }
    }

    /// Frees one page of these tables. The caller makes sure no unit can still walk it.
    fn free(&mut self, phys: u64) {
        if let Some(i) = self.pages.iter().position(|&p| p == phys) {
            self.pages.swap_remove(i);
            free_frame(phys);
        }
    }

    /// Frees every page. The caller makes sure no unit can still walk them.
    fn free_all(&mut self) {
        for phys in self.pages.drain(..) {
            free_frame(phys);
        }
    }
}

/// Gives a table page back to the frame allocator.
fn free_frame(phys: u64) {
    let frame = Frame::containing(PhysicalAddress::new(phys as usize));
    // SAFETY: `phys` came from `allocate_frame` in `KernelTables::alloc_table` and was
    // removed from its tables, which were its only owner; no unit walks it any more.
    unsafe { deallocate_frame(frame) };
}

/// The identity domain for one address width and page size: second-stage tables and one
/// context table whose 256 entries all use them.
struct IdentityTables {
    agaw: Agaw,
    page_size: PageSize,
    context_table: u64,
    entry: ContextEntry,
}

/// Builds the identity domain's tables for `[0, plan.top)`.
fn build_identity(plan: &UnitPlan, tables: &mut KernelTables) -> Result<IdentityTables, Error> {
    let slpt = tables.alloc_table().map_err(|_| Error::NoMemory)?;
    table::map_identity(tables, slpt, plan.agaw, 0, plan.top, plan.page_size)
        .map_err(Error::Map)?;

    // Under `enforce`, devices no driver has attached to a domain of its own are blocked
    // instead: their context entries stay not present, so all their DMA faults.
    let entry = if cfg!(iommu = "enforce") {
        ContextEntry::BLOCKED
    } else {
        ContextEntry::new(
            slpt,
            plan.agaw,
            IDENTITY_DOMAIN,
            table::TranslationType::Untranslated,
        )
    };
    let context_table = tables.alloc_table().map_err(|_| Error::NoMemory)?;
    for pair in tables.table(context_table).chunks_exact_mut(2) {
        pair.copy_from_slice(&entry.0);
    }
    Ok(IdentityTables {
        agaw: plan.agaw,
        page_size: plan.page_size,
        context_table,
        entry,
    })
}

/// Maps `len` bytes of a unit's register set at `phys` as device memory.
fn map_registers(phys: PhysicalAddress, len: usize) -> usize {
    // SAFETY: `phys` is inside a register set named by a DRHD, which only this module maps,
    // and mapping device memory has no other requirements.
    unsafe { map_device_memory(phys, len) }.data()
}

/// Maps a unit's registers and reads its capabilities.
fn map_unit(drhd: &Drhd<'_>) -> Regs {
    let base = drhd.register_base;
    let mut regs = Regs {
        base,
        mmio: Volatile::registers(PhysicalAddress::new(base as usize), PAGE_SIZE),
        cap: Capability(0),
        ecap: ExtendedCapability(0),
    };
    regs.cap = Capability(regs.read(regs::CAP));
    regs.ecap = ExtendedCapability(regs.read(regs::ECAP));
    let span = regs::register_span(regs.cap, regs.ecap, drhd.register_pages());
    if let Some(rest) = span.checked_sub(PAGE_SIZE).filter(|&rest| rest > 0) {
        map_registers(PhysicalAddress::new(base as usize).add(PAGE_SIZE), rest);
        regs.mmio = regs.mmio.with_len(span);
    }
    regs
}

/// Runs the enable sequence on one unit with its root table, and returns how it will be
/// invalidated from now on.
fn enable(regs: &Regs, plan: &UnitPlan, root_table: u64) -> Result<Invalidator, Error> {
    regs.write(regs::RTADDR, root_table | regs::RTADDR_TTM_LEGACY);
    regs.command(global::SRTP, Step::RootTablePointer)?;
    let mut invalidator = if plan.queued_invalidation {
        Invalidator::Queued(QueuedInvalidation::enable(regs)?)
    } else {
        Invalidator::Registers
    };
    regs.flush_write_buffer()?;
    let global = [Request::ContextGlobal, Request::IotlbGlobal];
    match &mut invalidator {
        Invalidator::Registers => regs.invalidate_registers(&global)?,
        Invalidator::Queued(queue) => queue.submit(regs, &global)?,
    }
    regs.command(global::TE, Step::TranslationEnable)?;

    // Protected memory regions block DMA on their own; with translation on they are no longer
    // needed (10.4.16).
    let pmen_reg: u32 = regs.read(regs::PMEN);
    if pmen_reg & pmen::EPM != 0 {
        regs.write(regs::PMEN, pmen_reg & !pmen::EPM);
        regs.wait(regs::PMEN, Step::ProtectedMemoryDisable, |v: u32| {
            v & pmen::PRS == 0
        })?;
    }
    Ok(invalidator)
}

/// The APIC ID of the CPU this runs on (the BSP, at boot), if known.
fn bsp_apic_id() -> Option<u32> {
    crate::percpu::PercpuBlock::current()
        .misc_arch_info
        .apic_id_opt
        .get()
        .map(|id| id.get())
}

/// Reads a 32-bit register of `bdf`'s configuration space in segment 0 through the legacy
/// configuration mechanism (ports 0xCF8/0xCFC). Only for boot, on the BSP, before user space
/// (pcid) starts using configuration space.
fn pci_config_read(bdf: Bdf, offset: u8) -> u32 {
    let address = 0x8000_0000 | (u32::from(bdf.source_id()) << 8) | u32::from(offset & 0xfc);
    // SAFETY: this runs at boot on the BSP before user space, so nothing else uses the
    // configuration ports; callers read only registers without read side effects.
    unsafe {
        x86::io::outl(0xcf8, address);
        x86::io::inl(0xcfc)
    }
}

/// Writes a 32-bit register of `bdf`'s configuration space in segment 0, under the same
/// conditions as [`pci_config_read`].
fn pci_config_write(bdf: Bdf, offset: u8, value: u32) {
    let address = 0x8000_0000 | (u32::from(bdf.source_id()) << 8) | u32::from(offset & 0xfc);
    // SAFETY: as in `pci_config_read`; callers write only the IOMMU function's own MSI
    // capability, which no other code programs.
    unsafe {
        x86::io::outl(0xcf8, address);
        x86::io::outl(0xcfc, value);
    }
}

/// The ACPI table with `signature`, as bytes, if firmware has one.
fn acpi_table(signature: &str) -> Option<&'static [u8]> {
    let sdt = crate::acpi::find_sdt(signature).into_iter().next()?;
    // SAFETY: `get_sdt` mapped the whole table, `length` bytes from its header, and firmware
    // tables stay mapped and unchanged for the life of the kernel.
    Some(unsafe {
        core::slice::from_raw_parts(
            (sdt as *const crate::acpi::sdt::Sdt).cast::<u8>(),
            sdt.length as usize,
        )
    })
}

/// Logs what `kernel.iommu = "enforce"` means, once DMA domains are available.
fn announce_enforce() {
    if cfg!(iommu = "enforce") {
        info!(
            "IOMMU: enforce mode: only drivers with a bound domain get DMA addresses, and only devices attached to a domain can DMA"
        );
    }
}

/// Routes the unit's fault event interrupt to the BSP, after clearing any old faults.
fn enable_fault_interrupt(regs: &Regs) {
    log_faults(regs);
    let Some(apic_id) = bsp_apic_id() else {
        warn!(
            "IOMMU: BSP APIC ID unknown; faults at unit {:#x} are not reported",
            regs.base
        );
        return;
    };
    match u8::try_from(apic_id) {
        Ok(id) => {
            regs.write(regs::FEDATA, u32::from(FAULT_VECTOR));
            regs.write(regs::FEADDR, fault::event_address(id));
            regs.write(regs::FEUADDR, 0u32);
            // Clearing FECTL.IM unmasks the interrupt.
            regs.write(regs::FECTL, 0u32);
        }
        Err(_) => {
            warn!(
                "IOMMU: BSP APIC ID {apic_id} needs interrupt remapping; faults at unit {:#x} are not reported",
                regs.base
            );
        }
    }
}

/// Logs and clears every fault recorded by a unit.
fn log_faults(regs: &Regs) {
    let status = FaultStatus(regs.read(regs::FSTS));
    if status.pending() {
        for rec in fault::records(regs.cap, status) {
            let (lo, hi) = (regs.read(rec.low), regs.read(rec.high));
            if let Some(record) = FaultRecord::decode(lo, hi) {
                trace_event!(
                    ferric_trace::kernel::IOMMU_FAULT,
                    regs.base,
                    ferric_trace::kernel::IommuFormat::VtdFaultRecord.raw(),
                    lo,
                    hi,
                    1
                );
                let what = if record.reason.is_interrupt_remapping() {
                    "interrupt remapping"
                } else {
                    "DMA"
                };
                error!("IOMMU: {what} fault at unit {:#x}: {record}", regs.base);
                // Writing F back as 1 frees the record; the other bits are read-only.
                regs.write(rec.f_dword, fault::RECORD_F_DWORD);
            }
        }
    }
    if status.overflow() {
        error!(
            "IOMMU: unit {:#x} lost faults (fault records full)",
            regs.base
        );
    }
    if let Some(err) = QueueError::from_fault_status(status.0) {
        error!("IOMMU: unit {:#x}: {err}", regs.base);
    }
    if status.clear_value() != 0 {
        regs.write(regs::FSTS, status.clear_value());
    }
}

interrupt!(fault_interrupt, || {
    for unit in UNITS.get().into_iter().flatten() {
        log_faults(&unit.regs);
    }
    amdvi::on_fault_vector();
    interrupt::on_fault_vector();
    local_apic::eoi();
});

/// The RMRRs in the DMAR table, logged once at boot.
fn rmrrs<'a>(dmar: &Dmar<'a>) -> impl Iterator<Item = ferric_vtd::dmar::Rmrr<'a>> {
    dmar.structures().filter_map(|s| match s {
        Ok(Structure::Rmrr(rmrr)) => Some(rmrr),
        _ => None,
    })
}

/// The highest physical address the identity map must cover: the end of RAM or of any RMRR,
/// whichever is higher. Everything below it, holes included, is mapped.
fn identity_top(dmar: &Dmar<'_>) -> u64 {
    let mut top = crate::startup::memory::ram_end() as u64;
    for rmrr in rmrrs(dmar) {
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

/// Sets up one unit, reusing matching identity tables from `identities` or building them.
fn init_unit(
    drhd: &Drhd<'_>,
    flags: DmarFlags,
    top: u64,
    identities: &mut Vec<IdentityTables>,
    tables: &mut KernelTables,
) -> Result<Unit, Error> {
    let regs = map_unit(drhd);
    let plan =
        plan::plan_unit(regs.cap, regs.ecap, regs.read(regs::GSTS), top).map_err(Error::Skip)?;
    let existing = identities
        .iter()
        .position(|d| d.agaw == plan.agaw && d.page_size == plan.page_size);
    let index = match existing {
        Some(index) => index,
        None => {
            identities.push(build_identity(&plan, tables)?);
            identities.len().saturating_sub(1)
        }
    };
    let identity = identities.get(index).ok_or(Error::NoMemory)?;
    let mut own = KernelTables::default();
    let root_table = own.alloc_table().map_err(|_| Error::NoMemory)?;
    let root = RootEntry::new(identity.context_table);
    for pair in own.table(root_table).chunks_exact_mut(2) {
        pair.copy_from_slice(&root.0);
    }
    if plan.flush_cpu_caches {
        tables.flush_cpu_caches();
        own.flush_cpu_caches();
    }
    let mut invalidator = enable(&regs, &plan, root_table)?;
    let interrupts = interrupt::enable(&regs, &mut invalidator, flags);
    let how = match invalidator {
        Invalidator::Queued(_) => "queued",
        Invalidator::Registers => "register",
    };
    let unattached = if cfg!(iommu = "enforce") {
        "unattached devices blocked"
    } else {
        "identity domain"
    };
    info!(
        "IOMMU: VT-d unit at {:#x} enabled ({unattached})",
        drhd.register_base
    );
    info!(
        "IOMMU: unit {:#x} uses {how} invalidation",
        drhd.register_base
    );
    Ok(Unit {
        regs,
        plan,
        state: Mutex::new(UnitState {
            tables: own,
            root_table,
            shared_context: identity.context_table,
            identity: identity.entry,
            split_buses: BTreeMap::new(),
            invalidator,
            interrupts,
        }),
    })
}

/// Finds the DMAR or IVRS table and enables every VT-d or AMD-Vi unit in it. Must run after
/// ACPI tables are parsed and before any user-space driver starts DMA.
pub fn init() {
    if let Some(bytes) = acpi_table("DMAR") {
        init_vtd(bytes);
    } else if let Some(bytes) = acpi_table("IVRS") {
        amdvi::init(bytes);
    } else {
        info!("IOMMU: no DMAR or IVRS table, IOMMU not enabled");
    }
}

/// Enables every VT-d unit in the DMAR table `bytes`.
fn init_vtd(bytes: &'static [u8]) {
    let dmar = match Dmar::parse(bytes) {
        Ok(dmar) => DMAR.call_once(|| dmar),
        Err(err) => {
            warn!("IOMMU: DMAR table is invalid ({err}), VT-d not enabled");
            return;
        }
    };
    let top = identity_top(dmar);
    let mut identities = Vec::new();
    let mut tables = KernelTables::default();
    let mut units = Vec::new();
    for structure in dmar.structures() {
        match structure {
            Ok(Structure::Drhd(drhd)) => {
                match init_unit(&drhd, dmar.flags(), top, &mut identities, &mut tables) {
                    Ok(unit) => units.push(unit),
                    Err(err) => {
                        warn!(
                            "IOMMU: VT-d unit at {:#x} skipped: {err}",
                            drhd.register_base
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(err) => {
                warn!("IOMMU: stopped reading the DMAR table: {err}");
                break;
            }
        }
    }
    // The fault handler reads UNITS, so faults are routed only once it is set.
    let units = UNITS.call_once(|| units);
    for unit in units {
        enable_fault_interrupt(&unit.regs);
    }
    interrupt::setup_ioapics();
    if let Some(layout) = plan::domain_layout(units.iter().map(|u| (u.regs.cap, u.regs.ecap))) {
        LAYOUT.call_once(|| layout);
        *DOMAIN_IDS.lock() = Some(DomainIds::new(FIRST_DMA_DOMAIN, layout.domain_id_limit));
        selftest::run();
        announce_enforce();
    } else if !units.is_empty() {
        warn!("IOMMU: the units share no table layout; DMA domains are unavailable");
    }
}
