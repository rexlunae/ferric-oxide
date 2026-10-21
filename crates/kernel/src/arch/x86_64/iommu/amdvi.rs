//! Ferric Oxide: AMD-Vi DMA remapping (`kernel.iommu`), written from the AMD I/O
//! Virtualization Technology (IOMMU) Specification, document 48882.
//!
//! [`init`] runs when firmware provides an IVRS table and no DMAR table. Every IOMMU the IVRS
//! lists (the IVHD of the highest type for each one, section 5.2.2) is switched on with:
//!
//! - a device table covering all 65 536 device IDs (2 MiB, section 2.2.2), in which every
//!   device starts in the identity domain (Mode 0, reads and writes allowed: DMA is not
//!   translated), or, under `kernel.iommu = "enforce"`, blocked (translated through an empty
//!   table with reads and writes refused). Each entry takes the DTE setting of the IVHD device
//!   entry that names its device, and requests a device makes under an alias ID (IVHD alias
//!   entries) are handled by programming the alias's entry too. Several functions can share
//!   an alias, so `ferric_amdvi::claims` records which domain holds each entry and which
//!   attached functions use it: another domain cannot take a held entry, attaching to an
//!   entry the same domain holds changes nothing, and detaching blocks only the entries no
//!   other function uses;
//! - a one-page command buffer (section 3.3.1). Every batch of commands ends with a
//!   COMPLETION_WAIT that stores a sequence number, which the kernel polls for;
//! - an event log of [`EVENT_LOG_ENTRIES`] entries (section 3.3.2), drained after every command
//!   batch and from the event interrupt. The interrupt is the IOMMU function's own PCI MSI
//!   (section 3.7.1), programmed with the fault vector [`FAULT_VECTOR`] for the BSP; without an
//!   MSI capability the log is only drained after commands. Identical events in a row are
//!   logged once with a count.
//!
//! DMA domains (`domain.rs`) use host page tables of [`plan::DOMAIN_LEVELS`] levels, built
//! by the same walker as VT-d's second-stage tables with the AMD-Vi encoding
//! (`ferric_amdvi::table`). Attaching a device first stores a blocked word 0 into its entry,
//! invalidates the entry and the old domain's pages, then writes the new entry (word 0 last)
//! and invalidates again (section 2.2.2's rule that software invalidates after changing an
//! entry). Changes to a domain's tables are followed by INVALIDATE_IOMMU_PAGES for the range,
//! with PDE set, before any freed table page is reused.
//!
//! Interrupt remapping is not implemented yet: IV stays clear in every entry, so the IOMMU
//! passes interrupts through unchanged, and pcid falls back to compatibility-format MSI.
//!
//! The IVRS parsing and every register, entry and command encoding are in the host-tested
//! `ferric-amdvi` crate (`crates/amdvi`).

use alloc::vec::Vec;
use core::{
    fmt,
    ops::Range,
    sync::atomic::{fence, AtomicBool, Ordering},
};

use ferric_amdvi::{
    claims::{Claims, Holder},
    command::{Command, CommandBuffer, PageRange},
    dte::DeviceTableEntry,
    event::{Coalescer, Event, EventLog, Run},
    ivrs::{DteSetting, Ivhd, Ivrs},
    plan::{
        self, DomainLayout, UnitPlan, BLOCKED_DOMAIN, DOMAIN_ID_LIMIT, DOMAIN_LEVELS,
        FIRST_DMA_DOMAIN, HT_RESERVED, IDENTITY_DOMAIN,
    },
    regs::{self, control, status, ExtendedFeatures, RingSize},
    DeviceId,
};
use ferric_vtd::{iova::DomainIds, plan::SelectError, table::Access, Bdf};
use spin::{Mutex, Once};

use super::{
    domain::{Attachment, Device, DomainError},
    pci_config_read, pci_config_write,
    volatile::Volatile,
    Error, Step, DOMAIN_IDS, FAULT_VECTOR, POLL_LIMIT,
};
use crate::memory::{allocate_p2frame, PhysicalAddress, PAGE_SIZE};

/// The device table's frames as a power of two: 2 MiB.
const DEVICE_TABLE_ORDER: u32 = 9;

/// The event log's size as a power of two of entries. Faults can come in floods (a device
/// retrying a DMA), so the log is large; overflow is still handled.
const EVENT_LOG_LOG2: u8 = 13;

/// Entries in the event log.
const EVENT_LOG_ENTRIES: u32 = 1 << EVENT_LOG_LOG2;

/// The event log's frames as a power of two: 2^13 entries of 16 bytes are 32 pages.
const EVENT_LOG_ORDER: u32 = 5;

/// The most commands sent in one batch, leaving room in the 256-entry buffer for the
/// COMPLETION_WAIT.
const COMMAND_BATCH: usize = 128;

/// PCI capability ID of MSI.
const MSI_CAPABILITY: u8 = 0x05;

/// The enabled IOMMUs.
static UNITS: Once<Vec<Unit>> = Once::new();

/// The IVRS table.
static IVRS: Once<Ivrs<'static>> = Once::new();

/// The table layout of DMA domains.
static LAYOUT: Once<DomainLayout> = Once::new();

/// A page that is all zeroes forever: the root of every blocked entry's empty tables.
static EMPTY_ROOT: Once<Block> = Once::new();

/// Zeroed, physically contiguous frames the IOMMU reads or writes. They are never freed: an
/// enabled IOMMU uses them until the machine resets.
#[derive(Clone, Copy, Debug)]
struct Block {
    phys: u64,
    words: usize,
}

impl Block {
    /// Allocates `2^order` zeroed frames.
    fn alloc(order: u32) -> Result<Self, Error> {
        let frame = allocate_p2frame(order, true).ok_or(Error::NoMemory)?;
        let bytes = PAGE_SIZE.checked_shl(order).ok_or(Error::NoMemory)?;
        Ok(Self {
            phys: frame.base().data() as u64,
            words: bytes / 8,
        })
    }

    /// The block's memory. Its frames are never freed, so it stays mapped.
    fn memory(self) -> Volatile {
        Volatile::frames(self.phys, self.words.saturating_mul(8))
    }

    /// Reads word `index` (0 past the end). Volatile: the IOMMU writes the block.
    fn read(self, index: usize) -> u64 {
        index
            .checked_mul(8)
            .map_or(0, |offset| self.memory().read(offset))
    }

    /// Writes word `index` (nothing past the end). Volatile: the IOMMU reads the block. Every
    /// block is written only with its unit's state lock held (or before the unit is shared).
    fn write(self, index: usize, value: u64) {
        if let Some(offset) = index.checked_mul(8) {
            self.memory().write(offset, value);
        }
    }
}

/// A mapped AMD-Vi register set ([`regs::MMIO_SPAN`] bytes).
struct Regs {
    base: u64,
    mmio: Volatile,
}

impl Regs {
    /// Reads the 64-bit register at `offset`.
    fn read(&self, offset: usize) -> u64 {
        self.mmio.read(offset)
    }

    /// Writes the 64-bit register at `offset`.
    fn write(&self, offset: usize, value: u64) {
        self.mmio.write(offset, value);
    }
}

/// An enabled IOMMU.
struct Unit {
    regs: Regs,
    ivhd: Ivhd<'static>,
    plan: UnitPlan,
    /// The status register's CmdBufRun bit tracks the command buffer, so a clear bit means
    /// it stopped.
    reports_run: bool,
    state: Mutex<UnitState>,
    events: Mutex<Events>,
    /// Set by every [`Unit::drain_events`] call; the context holding `events` drains again
    /// while it is set, so a call that finds the log busy is not lost.
    drain_wanted: AtomicBool,
}

/// The parts of a unit that change after it is enabled.
struct UnitState {
    device_table: Block,
    commands: CommandBuffer,
    ring: Block,
    /// The page COMPLETION_WAIT stores into.
    completion: Block,
    /// The value the next COMPLETION_WAIT stores.
    sequence: u64,
    /// Set after the command buffer stopped; no more commands are sent.
    stopped: bool,
    /// Which DMA domain holds each entry, and which attached functions use it.
    claims: Claims,
}

/// A unit's event log.
struct Events {
    log: EventLog,
    block: Block,
}

/// What a device's entry becomes.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// Translated through the host page tables at `root`, in domain `domain`.
    Translated {
        /// The root table.
        root: u64,
        /// The domain ID.
        domain: u16,
    },
    /// The entry the device had at boot: identity, or blocked under `enforce`.
    Identity,
    /// Every DMA faults.
    Blocked,
}

/// The blocked entry for a device with `setting`.
fn blocked_entry(setting: DteSetting) -> DeviceTableEntry {
    let root = EMPTY_ROOT.get().map_or(0, |block| block.phys);
    DeviceTableEntry::blocked(root, DOMAIN_LEVELS, BLOCKED_DOMAIN, setting)
}

/// The entry every device starts with.
fn boot_entry(setting: DteSetting) -> DeviceTableEntry {
    if cfg!(iommu = "enforce") {
        blocked_entry(setting)
    } else {
        DeviceTableEntry::passthrough(IDENTITY_DOMAIN, setting)
    }
}

impl Target {
    /// The entry for a device with `setting`.
    fn entry(self, setting: DteSetting) -> DeviceTableEntry {
        match self {
            Self::Translated { root, domain } => {
                DeviceTableEntry::translated(root, DOMAIN_LEVELS, domain, setting)
            }
            Self::Identity => boot_entry(setting),
            Self::Blocked => blocked_entry(setting),
        }
    }
}

impl UnitState {
    /// The first word of `device`'s entry.
    fn entry_index(device: DeviceId) -> usize {
        usize::from(device.0).saturating_mul(4)
    }

    /// `device`'s entry.
    fn dte(&self, device: DeviceId) -> DeviceTableEntry {
        let at = Self::entry_index(device);
        DeviceTableEntry([0, 1, 2, 3].map(|i| self.device_table.read(at.saturating_add(i))))
    }

    /// Writes `device`'s entry, word 3 first and word 0 (V, TV, Mode, root) last.
    fn write_dte(&mut self, device: DeviceId, entry: DeviceTableEntry) {
        let at = Self::entry_index(device);
        for (i, word) in entry.words() {
            self.device_table.write(at.saturating_add(i), word);
            // Keep the stores in this order.
            fence(Ordering::SeqCst);
        }
    }

    /// Writes only word 0 of `device`'s entry: one aligned store the IOMMU sees whole.
    fn write_word0(&mut self, device: DeviceId, word: u64) {
        self.device_table.write(Self::entry_index(device), word);
        fence(Ordering::SeqCst);
    }
}

impl Unit {
    /// Sends `commands` in batches, each followed by a COMPLETION_WAIT that is waited for,
    /// then drains the event log.
    fn submit(&self, state: &mut UnitState, commands: &[Command]) -> Result<(), Error> {
        let result = commands
            .chunks(COMMAND_BATCH)
            .try_for_each(|batch| self.submit_batch(state, batch));
        self.drain_events();
        result
    }

    /// Sends one batch and waits for its COMPLETION_WAIT.
    fn submit_batch(&self, state: &mut UnitState, batch: &[Command]) -> Result<(), Error> {
        if state.stopped {
            return Err(Error::CommandsStopped);
        }
        state.sequence = state.sequence.wrapping_add(1).max(1);
        let wait = Command::completion_wait(state.completion.phys, state.sequence);
        let count = u32::try_from(batch.len().saturating_add(1)).unwrap_or(u32::MAX);
        let slots = state
            .commands
            .reserve(self.regs.read(regs::COMMAND_HEAD), count)
            .map_err(Error::CommandsFull)?;
        state.completion.write(0, 0);
        for (slot, command) in slots.zip(batch.iter().copied().chain([wait])) {
            let at = (slot as usize).saturating_mul(2);
            state.ring.write(at, command.0[0]);
            state.ring.write(at.saturating_add(1), command.0[1]);
        }
        // The commands must reach memory before the IOMMU sees the new tail.
        fence(Ordering::SeqCst);
        self.regs
            .write(regs::COMMAND_TAIL, state.commands.tail_register());
        for _ in 0..POLL_LIMIT {
            if state.completion.read(0) == state.sequence {
                return Ok(());
            }
            if self.reports_run && self.regs.read(regs::STATUS) & status::CMD_BUF_RUN == 0 {
                state.stopped = true;
                return Err(Error::CommandsStopped);
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout(Step::CompletionWait))
    }

    /// Logs and consumes every event in the log, and acknowledges the status bits. When
    /// another context is draining it, that context drains again for this call, so no event
    /// is left in the log until the next interrupt or command batch.
    fn drain_events(&self) {
        self.drain_wanted.store(true, Ordering::Release);
        // A call that finds the lock held leaves `drain_wanted` set for the holder. The holder
        // checks it again only after giving the lock up, so a request made at the very end of
        // its pass is either seen there or finds the lock free and drains itself: one of the
        // two always drains after the request.
        loop {
            let Some(events) = self.events.try_lock() else {
                return;
            };
            while self.drain_wanted.swap(false, Ordering::AcqRel) {
                self.drain_once(&events);
            }
            drop(events);
            if !self.drain_wanted.load(Ordering::Acquire) {
                return;
            }
        }
    }

    /// One pass over the log. The status bits are acknowledged before `tail` is read: an event
    /// the IOMMU writes after that sets them again and raises another interrupt, and one it
    /// wrote before is below the `tail` read here. Acknowledging after the pass would clear
    /// the interrupt of an event written during it, which then waited in the log for an
    /// unrelated command batch and merged with the events logged in between.
    fn drain_once(&self, events: &Events) {
        let st = self.regs.read(regs::STATUS);
        if st & status::CLEAR & !status::EVENT_OVERFLOW != 0 {
            self.regs
                .write(regs::STATUS, st & status::CLEAR & !status::EVENT_OVERFLOW);
        }
        let head = self.regs.read(regs::EVENT_HEAD);
        let tail = self.regs.read(regs::EVENT_TAIL);
        let mut coalescer = Coalescer::new();
        for slot in events.log.unread(head, tail) {
            let at = (slot as usize).saturating_mul(2);
            let event = Event([
                events.block.read(at),
                events.block.read(at.saturating_add(1)),
            ]);
            if let Some(run) = coalescer.push(event) {
                report(self.regs.base, run);
            }
        }
        if let Some(run) = coalescer.finish() {
            report(self.regs.base, run);
        }
        self.regs
            .write(regs::EVENT_HEAD, tail & regs::RING_POINTER_MASK);
        // An overflow is acknowledged after the log is emptied (and restarted if it stopped).
        if st & status::EVENT_OVERFLOW != 0 {
            self.restart_event_log(st);
            self.regs.write(regs::STATUS, status::EVENT_OVERFLOW);
        }
    }

    /// After an overflow: logs it, and restarts the event log if it stopped (EventLogRun
    /// clear) by turning EventLogEn off, emptying the log and turning it on again.
    fn restart_event_log(&self, st: u64) {
        warn!(
            "IOMMU: unit {:#x}: event log overflowed; some events were lost",
            self.regs.base
        );
        if st & status::EVENT_LOG_RUN != 0 {
            return;
        }
        let ctl = self.regs.read(regs::CONTROL);
        self.regs.write(regs::CONTROL, ctl & !control::EVENT_LOG_EN);
        self.regs.write(regs::EVENT_HEAD, 0);
        self.regs.write(regs::EVENT_TAIL, 0);
        self.regs.write(regs::STATUS, status::EVENT_OVERFLOW);
        self.regs.write(regs::CONTROL, ctl | control::EVENT_LOG_EN);
    }

    /// Invalidates every cached device table entry and translation, after enabling.
    fn invalidate_everything(&self, state: &mut UnitState) -> Result<(), Error> {
        if self.plan.invalidate_all {
            return self.submit(state, &[Command::invalidate_all()]);
        }
        let ids = (0..=u16::MAX).map(|id| Command::invalidate_devtab_entry(DeviceId(id)));
        let mut batch = Vec::with_capacity(COMMAND_BATCH);
        for command in ids.chain([
            Command::invalidate_iommu_pages(IDENTITY_DOMAIN, PageRange::All),
            Command::invalidate_iommu_pages(BLOCKED_DOMAIN, PageRange::All),
        ]) {
            batch.push(command);
            if batch.len() == COMMAND_BATCH {
                self.submit(state, &batch)?;
                batch.clear();
            }
        }
        self.submit(state, &batch)
    }

    /// The DTE setting and the requester ID (alias) of `device`, from the IVHD.
    fn device_setting(&self, device: DeviceId) -> (DteSetting, DeviceId) {
        let special = self
            .ivhd
            .special_devices()
            .find(|s| s.device == device)
            .map(|s| s.setting);
        match self.ivhd.lookup(device) {
            Some(coverage) => (
                special.unwrap_or(coverage.setting),
                coverage.requester(device),
            ),
            None => (special.unwrap_or(DteSetting(0)), device),
        }
    }
}

/// Logs one run of identical events.
fn report(base: u64, run: Run) {
    // Ferric Oxide (`kernel.trace_iommu`): the entry as the IOMMU wrote it.
    trace_event!(
        ferric_trace::kernel::IOMMU_FAULT,
        base,
        ferric_trace::kernel::IommuFormat::AmdViEvent.raw(),
        run.event.0[0],
        run.event.0[1],
        run.count
    );
    let kind = run.event.decode();
    let times = Times(run.count);
    if run.event.is_dma_fault() {
        error!("IOMMU: DMA fault at unit {base:#x}: {kind}{times}");
    } else {
        error!("IOMMU: unit {base:#x}: {kind}{times}");
    }
}

/// " (logged N times)" for a run of more than one event.
struct Times(u32);

impl fmt::Display for Times {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 > 1 {
            write!(f, " (logged {} times)", self.0)
        } else {
            Ok(())
        }
    }
}

/// The enabled units.
fn units() -> &'static [Unit] {
    UNITS.get().map_or(&[], Vec::as_slice)
}

/// The layout of DMA domains, when AMD-Vi is enabled.
pub(super) fn layout() -> Option<DomainLayout> {
    LAYOUT.get().copied()
}

/// The IOVA ranges every domain leaves out: the HyperTransport range (section 2.1.4.1, which
/// the IOMMU does not translate) and every IVMD.
pub(super) fn reserved_iova() -> Vec<Range<u64>> {
    let ivmds = IVRS
        .get()
        .into_iter()
        .flat_map(|ivrs| ivrs.memory())
        .filter_map(|ivmd| ivmd.pages());
    core::iter::once(HT_RESERVED).chain(ivmds).collect()
}

/// The unity and exclusion ranges `device` must reach one-to-one, with the access each
/// grants (exclusion ranges grant both).
pub(super) fn reserved_ranges(device: Device) -> Vec<(Range<u64>, Access)> {
    let id = DeviceId(device.bdf.source_id());
    IVRS.get()
        .into_iter()
        .flat_map(|ivrs| ivrs.memory())
        .filter(|ivmd| ivmd.needs_identity_map() && ivmd.applies_to(id))
        .filter_map(|ivmd| {
            let access = if ivmd.flags.exclusion() {
                Access::ReadWrite
            } else {
                Access::from_bits(ivmd.flags.read(), ivmd.flags.write())?
            };
            Some((ivmd.pages()?, access))
        })
        .collect()
}

/// The index of the enabled unit whose IVHD names `device`.
pub(super) fn unit_for(device: Device) -> Result<usize, DomainError> {
    let id = DeviceId(device.bdf.source_id());
    let serves = |ivhd: &Ivhd<'_>| ivhd.segment == device.segment && ivhd.lookup(id).is_some();
    if let Some(index) = units().iter().position(|u| serves(&u.ivhd)) {
        return Ok(index);
    }
    match IVRS
        .get()
        .and_then(|ivrs| ivrs.units().find(|ivhd| serves(ivhd)))
    {
        Some(ivhd) => Err(DomainError::UnitNotEnabled { base: ivhd.base }),
        None => Err(DomainError::Select(SelectError::NotCovered)),
    }
}

impl Unit {
    /// The DTE setting of `device` and the entries its DMA can use: its IVHD alias, if it
    /// has one, then its own.
    fn entries_for(&self, device: DeviceId) -> (DteSetting, Vec<DeviceId>) {
        let (setting, requester) = self.device_setting(device);
        let ids = if requester == device {
            Vec::from([device])
        } else {
            Vec::from([requester, device])
        };
        (setting, ids)
    }

    /// Changes the entries `ids` to `target`, with the invalidations the change needs: each
    /// gets a blocked word 0 first, then the entries and their old domains' pages are
    /// invalidated, then the new entries are written and invalidated again.
    fn replace(
        &self,
        state: &mut UnitState,
        ids: &[DeviceId],
        setting: DteSetting,
        target: Target,
    ) -> Result<(), Error> {
        if ids.is_empty() {
            return Ok(());
        }
        let blocked = blocked_entry(setting).0[0];
        let mut commands = Vec::with_capacity(ids.len().saturating_mul(2).saturating_add(1));
        let mut old_domains: Vec<u16> = Vec::new();
        for &each in ids {
            let old = state.dte(each).domain_id();
            if !old_domains.contains(&old) {
                old_domains.push(old);
            }
            state.write_word0(each, blocked);
            commands.push(Command::invalidate_devtab_entry(each));
        }
        commands.extend(
            old_domains
                .iter()
                .map(|&did| Command::invalidate_iommu_pages(did, PageRange::All)),
        );
        self.submit(state, &commands)?;
        let new = target.entry(setting);
        commands.clear();
        for &each in ids {
            state.write_dte(each, new);
            commands.push(Command::invalidate_devtab_entry(each));
        }
        commands.push(Command::invalidate_iommu_pages(
            new.domain_id(),
            PageRange::All,
        ));
        self.submit(state, &commands)
    }
}

/// Attaches `device` on unit `index` to the domain with ID `domain` and root table `root`:
/// its own entry and its IVHD alias's. An entry that domain already holds (an alias pcid
/// attaches after the function, or one another function in the domain shares) is left as it
/// is.
///
/// # Errors
///
/// [`DomainError::Busy`] when another domain holds one of the entries, since taking it would
/// move the DMA of the functions using it into this domain; [`DomainError::Hardware`] when an
/// invalidation fails.
pub(super) fn attach(
    index: usize,
    device: Device,
    root: u64,
    domain: u16,
) -> Result<(), DomainError> {
    let unit = units().get(index).ok_or(DomainError::NotEnabled)?;
    let id = DeviceId(device.bdf.source_id());
    let (setting, ids) = unit.entries_for(id);
    let holder = Holder { domain, root };
    let mut state = unit.state.lock();
    let writes = state
        .claims
        .check_attach(&ids, holder)
        .map_err(|conflict| DomainError::Busy {
            did: conflict.holder.domain,
        })?;
    unit.replace(
        &mut state,
        &writes,
        setting,
        Target::Translated { root, domain },
    )?;
    state.claims.record_attach(id, &ids, holder);
    Ok(())
}

/// Takes `device` out of its domain on unit `index`. Entries no other attached function
/// uses are blocked; a shared alias stays in the domain until its last user leaves.
///
/// # Errors
///
/// [`DomainError::Hardware`] when an invalidation fails; the device then stays attached.
pub(super) fn detach(index: usize, device: Device) -> Result<(), DomainError> {
    let unit = units().get(index).ok_or(DomainError::NotEnabled)?;
    let id = DeviceId(device.bdf.source_id());
    let (setting, ids) = unit.entries_for(id);
    let mut state = unit.state.lock();
    let last = state.claims.check_release(id, &ids);
    unit.replace(&mut state, &last, setting, Target::Blocked)?;
    state.claims.record_release(id, &ids);
    Ok(())
}

/// Puts `device`, attached to no domain, back to its boot entry (and its alias's).
///
/// # Errors
///
/// [`DomainError::Busy`] when a domain holds one of the entries, and
/// [`DomainError::Hardware`] when an invalidation fails.
pub(super) fn restore_identity(index: usize, device: Device) -> Result<(), DomainError> {
    let unit = units().get(index).ok_or(DomainError::NotEnabled)?;
    let (setting, ids) = unit.entries_for(DeviceId(device.bdf.source_id()));
    let mut state = unit.state.lock();
    if let Some(holder) = ids.iter().find_map(|&each| state.claims.holder(each)) {
        return Err(DomainError::Busy { did: holder.domain });
    }
    unit.replace(&mut state, &ids, setting, Target::Identity)?;
    Ok(())
}

/// How `device` is attached on unit `index`, read back from its device table entry.
pub(super) fn attachment(index: usize, device: Device) -> Result<Attachment, DomainError> {
    let unit = units().get(index).ok_or(DomainError::NotEnabled)?;
    let (_, requester) = unit.device_setting(DeviceId(device.bdf.source_id()));
    let entry = unit.state.lock().dte(requester);
    let empty = EMPTY_ROOT.get().map_or(0, |block| block.phys);
    Ok(if entry.is_blocked(empty) {
        Attachment::Blocked
    } else if entry.is_passthrough() {
        Attachment::Identity
    } else if entry.mode() != 0 && entry.read() && entry.write() {
        Attachment::Domain {
            id: entry.domain_id(),
            root: entry.root(),
        }
    } else {
        Attachment::Other
    })
}

/// Invalidates `[iova, iova + len)` of domain `did` on unit `index` and waits for it.
pub(super) fn flush_pages(index: usize, did: u16, iova: u64, len: u64) -> Result<(), DomainError> {
    let unit = units().get(index).ok_or(DomainError::NotEnabled)?;
    let mut state = unit.state.lock();
    let command = Command::invalidate_iommu_pages(did, PageRange::covering(iova, len));
    unit.submit(&mut state, &[command])?;
    Ok(())
}

/// Drains every unit's event log. Called by the fault handler, in interrupt context.
pub(super) fn on_fault_vector() {
    for unit in units() {
        unit.drain_events();
    }
}

/// The offset of `bdf`'s capability `id` in configuration space, if it has one.
fn find_capability(bdf: Bdf, id: u8) -> Option<u8> {
    // Status bit 4: the capability list exists.
    if pci_config_read(bdf, 0x04) & (1 << 20) == 0 {
        return None;
    }
    let mut at = (pci_config_read(bdf, 0x34) & 0xfc) as u8;
    // A list longer than configuration space can hold is broken.
    for _ in 0..48 {
        if at < 0x40 {
            return None;
        }
        let header = pci_config_read(bdf, at);
        if header as u8 == id {
            return Some(at);
        }
        at = ((header >> 8) & 0xfc) as u8;
    }
    None
}

/// Programs the IOMMU function's MSI capability (section 3.7.1) to deliver [`FAULT_VECTOR`]
/// to the BSP, and returns whether it did.
fn route_event_interrupt(ivhd: &Ivhd<'_>) -> bool {
    let base = ivhd.base;
    if ivhd.segment != 0 {
        warn!("IOMMU: AMD-Vi unit at {base:#x} is in segment {}; its events are read after commands only", ivhd.segment);
        return false;
    }
    let Some(apic_id) = super::bsp_apic_id().and_then(|id| u8::try_from(id).ok()) else {
        warn!("IOMMU: BSP APIC ID unknown or above 255; events at unit {base:#x} are read after commands only");
        return false;
    };
    let bdf = Bdf::from_source_id(ivhd.device_id.0);
    let Some(cap) = find_capability(bdf, MSI_CAPABILITY) else {
        warn!("IOMMU: AMD-Vi unit at {base:#x} ({}) has no MSI capability; its events are read after commands only", ivhd.device_id);
        return false;
    };
    let header = pci_config_read(bdf, cap);
    let message_control = (header >> 16) as u16;
    // Bit 7: 64-bit message address.
    let wide = message_control & 0x80 != 0;
    pci_config_write(
        bdf,
        cap.wrapping_add(4),
        0xfee0_0000 | (u32::from(apic_id) << 12),
    );
    let data = if wide {
        pci_config_write(bdf, cap.wrapping_add(8), 0);
        cap.wrapping_add(12)
    } else {
        cap.wrapping_add(8)
    };
    // Fixed delivery, edge-triggered; the upper half (extended data) stays 0.
    pci_config_write(bdf, data, u32::from(FAULT_VECTOR));
    // One message (Multiple Message Enable 0), MSI Enable set.
    let enabled = (message_control & !0x0070) | 1;
    pci_config_write(bdf, cap, (header & 0xffff) | (u32::from(enabled) << 16));
    true
}

/// Fills a new device table: every device ID gets the boot entry, with the DTE setting of the
/// IVHD entry that names it, and aliases get the entry of the devices that use them.
fn fill_device_table(ivhd: &Ivhd<'_>, state: &mut UnitState) {
    let default = boot_entry(DteSetting(0));
    for id in 0..=u16::MAX {
        state.write_dte(DeviceId(id), default);
    }
    for coverage in ivhd.coverage() {
        let coverage = match coverage {
            Ok(coverage) => coverage,
            Err(err) => {
                warn!(
                    "IOMMU: AMD-Vi unit at {:#x}: stopped reading its device entries: {err}",
                    ivhd.base
                );
                break;
            }
        };
        let entry = boot_entry(coverage.setting);
        for id in coverage.first.0..=coverage.last.0 {
            state.write_dte(DeviceId(id), entry);
        }
        if let Some(alias) = coverage.alias {
            state.write_dte(alias, entry);
        }
    }
    for special in ivhd.special_devices() {
        state.write_dte(special.device, boot_entry(special.setting));
    }
}

/// Sets up and enables one IOMMU.
fn init_unit(ivhd: Ivhd<'static>) -> Result<Unit, Error> {
    let regs = Regs {
        base: ivhd.base,
        mmio: Volatile::registers(PhysicalAddress::new(ivhd.base as usize), regs::MMIO_SPAN),
    };
    let current = regs.read(regs::CONTROL);
    let efr = ExtendedFeatures(regs.read(regs::EXTENDED_FEATURES));
    let plan = plan::plan_unit(current, efr).map_err(Error::AmdViSkip)?;
    let event_size = RingSize::new(EVENT_LOG_LOG2).ok_or(Error::NoMemory)?;
    let mut state = UnitState {
        device_table: Block::alloc(DEVICE_TABLE_ORDER)?,
        commands: CommandBuffer::new(RingSize::ONE_PAGE),
        ring: Block::alloc(0)?,
        completion: Block::alloc(0)?,
        sequence: 0,
        stopped: false,
        claims: Claims::new(),
    };
    let events = Events {
        log: EventLog::new(event_size),
        block: Block::alloc(EVENT_LOG_ORDER)?,
    };
    fill_device_table(&ivhd, &mut state);
    regs.write(
        regs::DEVICE_TABLE_BASE,
        regs::device_table_base(state.device_table.phys),
    );
    regs.write(
        regs::COMMAND_BUFFER_BASE,
        regs::ring_base(state.ring.phys, RingSize::ONE_PAGE),
    );
    regs.write(
        regs::EVENT_LOG_BASE,
        regs::ring_base(events.block.phys, event_size),
    );
    for pointer in [
        regs::COMMAND_HEAD,
        regs::COMMAND_TAIL,
        regs::EVENT_HEAD,
        regs::EVENT_TAIL,
    ] {
        regs.write(pointer, 0);
    }
    regs.write(regs::STATUS, status::CLEAR);
    let interrupt = route_event_interrupt(&ivhd);
    // The tables must reach memory before the IOMMU starts reading them.
    fence(Ordering::SeqCst);
    regs.write(
        regs::CONTROL,
        regs::enable_control(ivhd.flags, interrupt, current),
    );
    let reports_run = regs.read(regs::STATUS) & status::CMD_BUF_RUN != 0;
    let unit = Unit {
        regs,
        ivhd,
        plan,
        reports_run,
        state: Mutex::new(state),
        events: Mutex::new(events),
        drain_wanted: AtomicBool::new(false),
    };
    {
        let mut state = unit.state.lock();
        unit.invalidate_everything(&mut state)?;
    }
    let unattached = if cfg!(iommu = "enforce") {
        "unattached devices blocked"
    } else {
        "identity domain"
    };
    info!(
        "IOMMU: AMD-Vi unit at {:#x} ({}) enabled ({unattached})",
        ivhd.base, ivhd.device_id
    );
    let how = if interrupt { "MSI" } else { "polled" };
    info!(
        "IOMMU: unit {:#x}: {EVENT_LOG_ENTRIES}-entry event log ({how}), {} invalidation",
        ivhd.base,
        if unit.plan.invalidate_all {
            "INVALIDATE_IOMMU_ALL"
        } else {
            "per-entry"
        }
    );
    Ok(unit)
}

/// Enables every AMD-Vi unit in the IVRS table `bytes`.
pub(super) fn init(bytes: &'static [u8]) {
    let ivrs = match Ivrs::parse(bytes) {
        Ok(ivrs) => IVRS.call_once(|| ivrs),
        Err(err) => {
            warn!("IOMMU: IVRS table is invalid ({err}), AMD-Vi not enabled");
            return;
        }
    };
    for ivmd in ivrs.memory() {
        if let Some(pages) = ivmd.pages() {
            info!(
                "IOMMU: IVMD {:#x}-{:#x} kept out of DMA domains{}",
                pages.start,
                pages.end,
                if ivmd.needs_identity_map() {
                    ", identity-mapped for its devices"
                } else {
                    ""
                }
            );
        }
    }
    match Block::alloc(0) {
        Ok(block) => {
            EMPTY_ROOT.call_once(|| block);
        }
        Err(err) => {
            warn!("IOMMU: AMD-Vi not enabled: {err}");
            return;
        }
    }
    let mut enabled = Vec::new();
    for ivhd in ivrs.units() {
        match init_unit(ivhd) {
            Ok(unit) => enabled.push(unit),
            Err(err) => {
                warn!("IOMMU: AMD-Vi unit at {:#x} skipped: {err}", ivhd.base);
            }
        }
    }
    // The fault handler reads UNITS, so events are drained from it only once it is set.
    let units = UNITS.call_once(|| enabled);
    on_fault_vector();
    if let Some(layout) = plan::domain_layout(units.iter().map(|u| u.plan)) {
        LAYOUT.call_once(|| layout);
        *DOMAIN_IDS.lock() = Some(DomainIds::new(FIRST_DMA_DOMAIN, DOMAIN_ID_LIMIT));
        super::selftest::run();
        super::announce_enforce();
        if cfg!(iommu = "enforce") {
            warn!(
                "IOMMU: AMD-Vi interrupt remapping is not implemented yet; device interrupts are not checked"
            );
        }
    }
}
