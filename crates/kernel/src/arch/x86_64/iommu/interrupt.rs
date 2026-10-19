//! Ferric Oxide: VT-d interrupt remapping (io/iommu-design.md, section 3 and step 5).
//!
//! Every unit that can remap interrupts (ECAP.IR, the DMAR table's INTR_REMAP flag and queued
//! invalidation, which the interrupt entry cache needs) gets an interrupt remapping table of
//! [`TableSize::DEFAULT`] entries. [`enable`] points IRTA at it (with EIME when the local APICs
//! run in x2APIC mode and the unit has ECAP.EIM), sets GCMD.SIRTP, invalidates the interrupt
//! entry cache globally, sets GCMD.IRE and makes sure GCMD.CFI is clear. From then on the unit
//! blocks every interrupt request in compatibility format (fault 25h), under `auto` and
//! `enforce` alike: every in-tree interrupt source is converted. MSI and MSI-X go through
//! [`alloc_msi`], legacy IRQs reach the CPU through the 8259 PIC's virtual wire, which is not
//! subject to remapping, and the kernel programs no I/O APIC or HPET message in compatibility
//! format.
//!
//! - **MSI and MSI-X.** pcid asks for an entry for a function, a logical CPU and a vector
//!   through the attach capability (`Request::AllocIrte` in `scheme/iommu.rs`) when a driver
//!   asks it for an MSI message. The entry delivers that vector to that CPU's APIC and
//!   accepts requests only from the function's requester ID (or, behind a PCI Express to PCI
//!   bridge, its bus range), so a driver that writes another address into its device can at
//!   most name an entry that refuses its device (fault 26h).
//! - **I/O APICs.** [`setup_ioapics`] finds each I/O APIC a unit's DRHD scopes name (by its
//!   APIC ID in the MADT), masks any redirection entry firmware left unmasked in
//!   compatibility format, and checks the remappable path: the pin that carries ISA IRQ 0 (the
//!   system timer, also wired to the 8259) gets a remappable entry pointing at an interrupt
//!   remapping entry that delivers the IOMMU's own vector ([`FAULT_VECTOR`]) to the BSP and
//!   accepts only the I/O APIC's source-id from its scope. After a few timer ticks have come
//!   through, the fault handler masks the pin again and logs it. The entry stays allocated for
//!   that pin.
//! - **HPET.** The kernel drives the HPET in legacy replacement mode, whose interrupts travel
//!   the ISA IRQ lines, so no HPET message (FSB) needs an entry.
//!
//! Interrupt remapping faults are logged by the fault handler with the DMA faults
//! (`log_faults` in `iommu.rs`), decoded by `ferric-vtd`.
//!
//! The table entry, message and register encodings are in `ferric_vtd::interrupt`
//! (`crates/vtd`), host-tested.

use alloc::vec::Vec;
use core::fmt;

use ferric_vtd::{
    dmar::{DmarFlags, InterruptSourceKind, Structure},
    interrupt::{
        self, ioapic_rte, irta, remappable_msi, Destination, Irte, IrteAllocator, IrteFreeError,
        MsiMessage, SourceValidation, TableSize, TriggerMode, RTE_MASKED, RTE_REMAPPABLE,
    },
    invalidation::Request,
    regs::{self, global},
    Bdf,
};
use spin::Mutex;

use super::{
    clflush,
    domain::{self, Device, DomainError},
    map_registers, Error, Invalidator, Regs, Step, Unit, DMAR, FAULT_VECTOR, UNITS,
};
use crate::{
    acpi::madt::{self, MadtEntry},
    arch::device::ioapic::IoApic,
    cpu_set::LogicalCpuId,
    memory::{allocate_p2frame, PhysicalAddress, RmmA, RmmArch, PAGE_SIZE},
};

/// The table's frames as a power of two: [`TableSize::DEFAULT`] entries of 16 bytes are 16
/// pages.
const TABLE_ORDER: u32 = 4;

/// How many timer interrupts the I/O APIC check waits for before it masks the pin again.
const PIN_CHECK_TICKS: u32 = 4;

/// The vectors user space must not route a device's interrupt to on logical CPU `cpu`: the
/// exceptions, the legacy IRQs 0-15 (0x20-0x2f) the 8259 and I/O APIC deliver on the BSP, the
/// kernel's IPIs and IOMMU fault vector (0x40-0x45), the system call gate and the spurious
/// vector.
///
/// Ferric Oxide: with `irq-percpu`, 0x20-0x2f on an AP are ordinary vectors that the irq
/// scheme reserves per CPU (`irq:cpu-XX/N` for N below 16) and delivers to that CPU's handles,
/// so they are the driver's to route there. Without it they stay the kernel's everywhere, as
/// an interrupt on them would count for the BSP's legacy handles.
fn kernel_vector(vector: u8, cpu: u32) -> bool {
    let legacy = (0x20..0x30).contains(&vector);
    let ap_vector = cfg!(feature = "irq-percpu") && cpu != LogicalCpuId::BSP.get();
    vector < 0x20
        || (legacy && !ap_vector)
        || (0x40..=FAULT_VECTOR).contains(&vector)
        || vector == 0x80
        || vector == 0xff
}

/// A unit's interrupt remapping table and the entries in use.
pub(super) struct RemapTable {
    phys: u64,
    size: TableSize,
    /// 32-bit x2APIC destinations (IRTA.EIME).
    extended: bool,
    /// The unit's page walks snoop the CPU caches (ECAP.C).
    coherent: bool,
    entries: IrteAllocator,
}

impl RemapTable {
    /// The table as 64-bit words, two per entry.
    fn words(&mut self) -> &mut [u64] {
        let virt = RmmA::phys_to_virt(PhysicalAddress::new(self.phys as usize)).data();
        let len = self.size.bytes() / 8;
        // SAFETY: `phys` is the start of the 2^TABLE_ORDER frames `alloc_table` took for this
        // table, which hold exactly `size.bytes()` bytes, are reached through the physical
        // memory map, and are never freed or shared; `&mut self` makes this the only borrow.
        unsafe { core::slice::from_raw_parts_mut(virt as *mut u64, len) }
    }

    /// Writes entry `index`: the high word first and the low word (with P) last, so the unit
    /// never sees a present entry with half of it written.
    fn write(&mut self, index: u16, [lo, hi]: [u64; 2]) {
        let at = usize::from(index).saturating_mul(2);
        let words = self.words();
        if let Some([low, high]) = words.get_mut(at..at.saturating_add(2)) {
            *low = 0;
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            *high = hi;
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            *low = lo;
        }
        if !self.coherent {
            let offset = u64::from(index).saturating_mul(16);
            clflush(self.phys.saturating_add(offset), 16);
        }
    }
}

/// Why interrupt remapping could not be set up, or an entry could not be made or freed.
#[derive(Debug)]
pub enum IrqError {
    /// The unit was skipped, so interrupt remapping stays off.
    Skip(interrupt::InterruptSkip),
    /// No memory for the table.
    NoMemory,
    /// The unit did not complete a command or an invalidation.
    Hardware(Error),
    /// The unit that serves the device has no interrupt remapping.
    NotEnabled,
    /// No enabled unit serves the device.
    Unit(DomainError),
    /// The table is full.
    Full,
    /// The CPU does not exist or has not started.
    NoCpu {
        /// The logical CPU number.
        cpu: u32,
    },
    /// The CPU's APIC ID needs 32-bit destinations, which the unit does not use.
    Destination {
        /// The APIC ID.
        apic_id: u32,
    },
    /// The vector belongs to the kernel.
    Vector {
        /// The vector.
        vector: u8,
    },
    /// The entry ID names no entry in use.
    NotAllocated(IrteFreeError),
    /// The entry ID names no unit.
    BadId {
        /// The ID.
        id: u32,
    },
}

impl fmt::Display for IrqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skip(reason) => write!(f, "{reason}"),
            Self::NoMemory => f.write_str("out of memory for the interrupt remapping table"),
            Self::Hardware(err) => write!(f, "{err}"),
            Self::NotEnabled => f.write_str("the device's unit does not remap interrupts"),
            Self::Unit(err) => write!(f, "{err}"),
            Self::Full => f.write_str("the interrupt remapping table is full"),
            Self::NoCpu { cpu } => write!(f, "CPU {cpu} is not running"),
            Self::Destination { apic_id } => {
                write!(f, "APIC ID {apic_id} needs extended interrupt mode")
            }
            Self::Vector { vector } => write!(f, "vector {vector:#x} belongs to the kernel"),
            Self::NotAllocated(err) => write!(f, "{err}"),
            Self::BadId { id } => write!(f, "no interrupt remapping entry {id:#x}"),
        }
    }
}

impl From<Error> for IrqError {
    fn from(err: Error) -> Self {
        Self::Hardware(err)
    }
}

/// Whether the local APICs run in x2APIC mode: `local_apic::init` uses it whenever the CPU
/// reports it, and so does this check.
fn x2apic() -> bool {
    crate::arch::cpuid::cpuid()
        .get_feature_info()
        .is_some_and(|info| info.has_x2apic())
}

/// Turns on interrupt remapping for the unit with registers `regs`, which `invalidator`
/// invalidates, and returns its table; logs and returns `None` when the unit cannot remap
/// interrupts.
pub(super) fn enable(
    regs: &Regs,
    invalidator: &mut Invalidator,
    flags: DmarFlags,
) -> Option<RemapTable> {
    match try_enable(regs, invalidator, flags) {
        Ok(table) => {
            let width = if table.extended {
                "x2APIC (32-bit)"
            } else {
                "xAPIC (8-bit)"
            };
            info!(
                "IOMMU: interrupt remapping enabled at unit {:#x}: {} entries, {width} destinations, compatibility-format interrupts blocked",
                regs.base,
                table.size.entries()
            );
            Some(table)
        }
        Err(err) => {
            if cfg!(iommu = "enforce") {
                warn!(
                    "IOMMU: enforce mode: unit {:#x} cannot remap interrupts ({err}); its devices can still send any interrupt",
                    regs.base
                );
            } else {
                info!(
                    "IOMMU: interrupt remapping not enabled at unit {:#x}: {err}",
                    regs.base
                );
            }
            None
        }
    }
}

/// [`enable`], without the logging.
fn try_enable(
    regs: &Regs,
    invalidator: &mut Invalidator,
    flags: DmarFlags,
) -> Result<RemapTable, IrqError> {
    let gsts: u32 = regs.read(regs::GSTS);
    let queue = match invalidator {
        Invalidator::Queued(queue) => Some(queue),
        Invalidator::Registers => None,
    };
    let plan = interrupt::plan(regs.ecap, flags, gsts, queue.is_some(), x2apic())
        .map_err(IrqError::Skip)?;
    let queue = queue.ok_or(IrqError::Skip(interrupt::InterruptSkip::NoQueue))?;
    let size = TableSize::DEFAULT;
    let frame = allocate_p2frame(TABLE_ORDER, true).ok_or(IrqError::NoMemory)?;
    let phys = frame.base().data() as u64;
    let coherent = regs.ecap.coherent();
    if !coherent {
        clflush(phys, PAGE_SIZE << TABLE_ORDER);
    }
    regs.write(regs::IRTA, irta(phys, size, plan.extended));
    regs.command(global::SIRTP, Step::InterruptTablePointer)?;
    // The unit must drop anything it cached from an earlier table before it uses this one.
    queue.submit(regs, &[Request::IecGlobal])?;
    regs.command(global::IRE, Step::InterruptRemappingEnable)?;
    let gsts: u32 = regs.read(regs::GSTS);
    if gsts & global::CFI != 0 {
        regs.write(regs::GCMD, global::command(gsts, 0) & !global::CFI);
        regs.wait(regs::GSTS, Step::CompatibilityBlock, |s: u32| {
            s & global::CFI == 0
        })?;
    }
    Ok(RemapTable {
        phys,
        size,
        extended: plan.extended,
        coherent,
        entries: IrteAllocator::new(size),
    })
}

/// An entry made by [`alloc_msi`].
#[derive(Clone, Copy, Debug)]
pub struct MsiEntry {
    /// The ID [`free`] takes: the unit's index in `UNITS` above bit 16, the entry's index below.
    pub id: u32,
    /// The remappable-format message that names the entry.
    pub message: MsiMessage,
}

/// The enabled units.
fn units() -> &'static [Unit] {
    UNITS.get().map_or(&[], Vec::as_slice)
}

/// The APIC ID of logical CPU `cpu`.
fn apic_id(cpu: u32) -> Result<u32, IrqError> {
    crate::percpu::get_percpu_block(LogicalCpuId::new(cpu))
        .and_then(|block| block.misc_arch_info.apic_id_opt.get())
        .map(|id| id.get())
        .ok_or(IrqError::NoCpu { cpu })
}

/// Makes an entry on unit `index` that delivers `vector` to `apic_id`, for requests that pass
/// `source`, and invalidates the unit's cached copy of it.
fn alloc_entry(
    index: usize,
    source: SourceValidation,
    apic_id: u32,
    vector: u8,
) -> Result<MsiEntry, IrqError> {
    let unit = units().get(index).ok_or(IrqError::NotEnabled)?;
    let mut state = unit.state.lock();
    let table = state.interrupts.as_mut().ok_or(IrqError::NotEnabled)?;
    let destination =
        Destination::for_apic(apic_id, table.extended).ok_or(IrqError::Destination { apic_id })?;
    let entry = table.entries.alloc().ok_or(IrqError::Full)?;
    table.write(entry, Irte::fixed(vector, destination, source).encode());
    let result = unit.invalidate(
        &mut state,
        &[Request::IecIndex {
            index: entry,
            index_mask: 0,
        }],
    );
    if let Err(err) = result {
        if let Some(table) = state.interrupts.as_mut() {
            table.write(entry, Irte::NOT_PRESENT);
            let _ = table.entries.free(entry);
        }
        return Err(err.into());
    }
    let unit_bits = u32::try_from(index).map_err(|_| IrqError::NotEnabled)?;
    Ok(MsiEntry {
        id: (unit_bits << 16) | u32::from(entry),
        message: remappable_msi(entry),
    })
}

/// Makes an interrupt remapping entry that delivers `vector` to logical CPU `cpu` for MSI or
/// MSI-X requests from `device` that pass `source`, and returns it with the message the
/// device must send.
///
/// # Errors
///
/// [`IrqError::Vector`] for a vector the kernel uses, [`IrqError::NoCpu`] and
/// [`IrqError::Destination`] when the CPU cannot be named, [`IrqError::Unit`] and
/// [`IrqError::NotEnabled`] when no unit remaps the device's interrupts, [`IrqError::Full`],
/// and [`IrqError::Hardware`] when the invalidation fails (the entry is then freed again).
pub fn alloc_msi(
    device: Device,
    source: SourceValidation,
    cpu: u32,
    vector: u8,
) -> Result<MsiEntry, IrqError> {
    // The CPU and vector are checked first, with or without remapping: ENODEV tells pcid to
    // program a compatibility-format message itself, so it must mean only that no unit remaps
    // the device, never that the route is allowed. A kernel vector (on an AP without
    // `irq-percpu`, 0x20-0x2f, whose interrupts would count for the BSP's legacy handles) is
    // refused whatever the unit.
    if kernel_vector(vector, cpu) {
        return Err(IrqError::Vector { vector });
    }
    let apic_id = apic_id(cpu)?;
    // Without a VT-d unit for the device the answer is ENODEV, so pcid falls back to the
    // compatibility format. AMD-Vi units do not remap interrupts yet, so on them pcid gets
    // ENODEV too and programs a compatibility-format message, which they pass through
    // unchanged.
    let index = domain::vtd_unit_index(device).map_err(|err| match err {
        DomainError::NotEnabled => IrqError::NotEnabled,
        err => IrqError::Unit(err),
    })?;
    let entry = alloc_entry(index, source, apic_id, vector)?;
    let in_use = in_use();
    info!(
        "IOMMU: interrupt remapping entry {} for {:04x}:{}: vector {vector:#x} on CPU {cpu} (APIC {apic_id}); {in_use} IRTE(s) allocated",
        entry.id & 0xffff,
        device.segment,
        device.bdf
    );
    Ok(entry)
}

/// Frees the entry [`alloc_msi`] returned as `id`: requests that name it fault from now on.
///
/// # Errors
///
/// [`IrqError::BadId`] or [`IrqError::NotAllocated`] when `id` names no entry in use, and
/// [`IrqError::Hardware`] when the invalidation fails (the entry stays allocated, since the
/// unit may still use its cached copy).
pub fn free(id: u32) -> Result<(), IrqError> {
    let unit = units()
        .get(usize::try_from(id >> 16).map_err(|_| IrqError::BadId { id })?)
        .ok_or(IrqError::BadId { id })?;
    let entry = (id & 0xffff) as u16;
    let mut state = unit.state.lock();
    let table = state.interrupts.as_mut().ok_or(IrqError::BadId { id })?;
    if !table.entries.is_used(entry) {
        return Err(IrqError::NotAllocated(IrteFreeError { index: entry }));
    }
    table.write(entry, Irte::NOT_PRESENT);
    unit.invalidate(
        &mut state,
        &[Request::IecIndex {
            index: entry,
            index_mask: 0,
        }],
    )?;
    if let Some(table) = state.interrupts.as_mut() {
        table.entries.free(entry).map_err(IrqError::NotAllocated)?;
    }
    Ok(())
}

/// How many entries are in use on all units.
fn in_use() -> u32 {
    units()
        .iter()
        .filter_map(|unit| {
            unit.state
                .lock()
                .interrupts
                .as_ref()
                .map(|t| t.entries.in_use())
        })
        .sum()
}

/// The I/O APIC pin that carries ISA IRQ 0, armed with a remappable entry until a few timer
/// ticks have come through it.
struct PinCheck {
    ioapic: IoApic,
    apic_id: u8,
    pin: u8,
    /// The pin's remappable redirection entry, unmasked.
    rte: u64,
    /// The interrupt remapping entry index.
    entry: u16,
    source: Bdf,
    ticks: u32,
}

/// The armed I/O APIC check, until it has seen [`PIN_CHECK_TICKS`] interrupts.
static PIN_CHECK: Mutex<Option<PinCheck>> = Mutex::new(None);

/// Counts an interrupt on [`FAULT_VECTOR`] for the I/O APIC check, and masks the pin and logs
/// once enough have come. Called by the fault handler, in interrupt context.
pub(super) fn on_fault_vector() {
    let Some(mut guard) = PIN_CHECK.try_lock() else {
        return;
    };
    let Some(check) = guard.as_mut() else {
        return;
    };
    check.ticks = check.ticks.saturating_add(1);
    if check.ticks < PIN_CHECK_TICKS {
        return;
    }
    check.ioapic.write_entry(check.pin, check.rte | RTE_MASKED);
    info!(
        "IOMMU: I/O APIC {} pin {} delivered {} timer interrupt(s) through interrupt remapping entry {} (its source-id check names {}); pin masked again",
        check.apic_id, check.pin, check.ticks, check.entry, check.source
    );
    *guard = None;
}

/// The GSI and polarity (active low) ISA IRQ 0 arrives on, from the MADT's interrupt source
/// overrides, and whether it is edge-triggered.
fn timer_gsi(madt: &madt::Madt) -> (u32, bool, bool) {
    for entry in madt.iter() {
        if let MadtEntry::IntSrcOverride(over) = entry
            && over.irq_source == 0
        {
            let flags = over.flags;
            // Polarity bits 1:0 = 11b is active low; trigger bits 3:2 = 11b is level.
            return (over.gsi_base, flags & 3 == 3, (flags >> 2) & 3 != 3);
        }
    }
    (0, false, true)
}

/// Masks every redirection entry of `ioapic` firmware left unmasked in compatibility format,
/// and returns how many there were.
fn mask_compatibility_entries(ioapic: &IoApic) -> u32 {
    let mut masked: u32 = 0;
    for pin in 0..ioapic.entries() {
        let rte = ioapic.read_entry(pin);
        if rte & (RTE_MASKED | RTE_REMAPPABLE) == 0 {
            ioapic.write_entry(pin, rte | RTE_MASKED);
            masked = masked.saturating_add(1);
        }
    }
    masked
}

/// Finds the I/O APICs that units with interrupt remapping serve, masks what firmware left in
/// compatibility format, and arms the check of the remappable path on the pin of ISA IRQ 0.
/// Runs once, after the units are enabled and before interrupts are.
pub(super) fn setup_ioapics() {
    let (Some(dmar), Some(madt)) = (DMAR.get(), madt::madt()) else {
        return;
    };
    let (timer_gsi, active_low, edge) = timer_gsi(madt);
    for (index, unit) in units().iter().enumerate() {
        if unit.state.lock().interrupts.is_none() {
            continue;
        }
        let drhd = dmar.structures().find_map(|s| match s {
            Ok(Structure::Drhd(d)) if d.register_base == unit.regs.base => Some(d),
            _ => None,
        });
        let sources = drhd.into_iter().flat_map(|d| d.interrupt_sources());
        for source in sources.filter(|s| s.kind == InterruptSourceKind::IoApic) {
            let Some(ioapic) = madt.iter().find_map(|e| match e {
                MadtEntry::IoApic(io) if io.id == source.id => Some(io),
                _ => None,
            }) else {
                warn!(
                    "IOMMU: DMAR names I/O APIC {} at {}, which the MADT does not list",
                    source.id, source.source
                );
                continue;
            };
            let base = PhysicalAddress::new(ioapic.address as usize);
            let regs = IoApic::new(
                map_registers(base, PAGE_SIZE) as *const u32,
                ioapic.gsi_base,
            );
            let masked = mask_compatibility_entries(&regs);
            info!(
                "IOMMU: I/O APIC {} (source-id {}) remapped by unit {:#x}; {masked} compatibility-format entries masked",
                source.id, source.source, unit.regs.base
            );
            let gsi_base = ioapic.gsi_base;
            let pin = timer_gsi
                .checked_sub(gsi_base)
                .and_then(|pin| u8::try_from(pin).ok())
                .filter(|&pin| pin < regs.entries());
            match pin {
                Some(pin) if edge => {
                    arm_pin_check(index, regs, source.id, source.source, pin, active_low)
                }
                Some(_) => {
                    info!("IOMMU: ISA IRQ 0 is level-triggered; I/O APIC check skipped");
                }
                None => {}
            }
        }
    }
}

/// Points `pin` of `ioapic` at a new remappable entry on unit `index` that delivers
/// [`FAULT_VECTOR`] to the BSP for the I/O APIC's `source` only, unmasked.
fn arm_pin_check(
    index: usize,
    ioapic: IoApic,
    apic_id: u8,
    source: Bdf,
    pin: u8,
    active_low: bool,
) {
    let check = SourceValidation::RequesterId {
        source_id: source.source_id(),
        ignore_function_bits: 0,
    };
    let entry = apic_id_of_bsp().and_then(|bsp| alloc_entry(index, check, bsp, FAULT_VECTOR));
    let entry = match entry {
        Ok(entry) => (entry.id & 0xffff) as u16,
        Err(err) => {
            warn!("IOMMU: cannot check I/O APIC {apic_id} pin {pin}: {err}");
            return;
        }
    };
    let rte = ioapic_rte(entry, FAULT_VECTOR, TriggerMode::Edge, active_low, false);
    ioapic.write_entry(pin, rte);
    info!(
        "IOMMU: I/O APIC {apic_id} pin {pin} (ISA IRQ 0) routed through interrupt remapping entry {entry} for a check"
    );
    *PIN_CHECK.lock() = Some(PinCheck {
        ioapic,
        apic_id,
        pin,
        rte,
        entry,
        source,
        ticks: 0,
    });
}

/// The BSP's APIC ID.
fn apic_id_of_bsp() -> Result<u32, IrqError> {
    apic_id(LogicalCpuId::BSP.get())
}
