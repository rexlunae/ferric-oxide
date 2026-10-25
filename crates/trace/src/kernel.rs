//! The kernel's events: the one definition of their ids, names, levels and arguments, shared by
//! the kernel (which emits them) and every decoder.
//!
//! Ids are part of the record format: append new events at the end and never renumber.
//!
//! Some arguments are codes rather than plain numbers. Their one definition is here, next to
//! the events, so the kernel encodes them and decoders read them the same way:
//! [`PageFaultFlags`] and [`PageFaultOutcome`] for `mm.page_fault`, [`IrqSource`] for
//! `irq.deliver` and [`IommuFormat`] for `iommu.fault`.

use core::fmt;

crate::events! {
    /// Every kernel event, by id.
    pub catalog CATALOG;

    /// A system call starts: its number and its first four arguments.
    pub const SYSCALL_ENTER = 0 {
        name: "syscall.enter",
        subsystem: Syscall,
        level: Info,
        args: ["nr", "a", "b", "c", "d"],
    }

    /// A system call returns: its number and the raw return value (a negated errno on failure,
    /// as `Error::mux` returns it).
    pub const SYSCALL_EXIT = 1 {
        name: "syscall.exit",
        subsystem: Syscall,
        level: Info,
        args: ["nr", "ret"],
    }

    /// The scheduler switches this CPU from one context to another: the process ids and the
    /// contexts' debug ids.
    pub const SCHED_SWITCH = 2 {
        name: "sched.switch",
        subsystem: Sched,
        level: Info,
        args: ["prev_pid", "next_pid", "prev_id", "next_id"],
    }

    /// The kernel queues a request to a user-space scheme: the scheme's id, the request's SQE
    /// tag, its opcode (`schemev2::Opcode`), the caller's pid, and the request's first and
    /// third arguments (for reads and writes, the scheme's file number and the byte count).
    ///
    /// A tag is unique among a scheme's requests in flight and is reused afterwards, so a
    /// request is matched with the next `scheme.response` of the same scheme and tag. A
    /// `scheme.cancel` in between does not end the request.
    pub const SCHEME_REQUEST = 3 {
        name: "scheme.request",
        subsystem: Scheme,
        level: Info,
        args: ["scheme", "tag", "opcode", "caller", "arg0", "arg2"],
    }

    /// The kernel accepts a scheme's answer (its CQE) to a request still waiting for one: the
    /// scheme's id, the tag, the result and the responding process's pid. A CQE the kernel
    /// rejects (an unknown or finished tag) is not recorded.
    ///
    /// The result is what the caller's system call gets, in the `Error::mux` encoding (a
    /// negated errno on failure): the scheme's value after the kernel's normalization, which
    /// turns `ECANCELED` into `EINTR` for a request being canceled and an `EINTR` the kernel
    /// did not ask for into `EIO`. A response that carries file descriptors records 0.
    pub const SCHEME_RESPONSE = 4 {
        name: "scheme.response",
        subsystem: Scheme,
        level: Info,
        args: ["scheme", "tag", "ret", "responder"],
    }

    /// The kernel asks a scheme to cancel a request whose caller was interrupted: the scheme's
    /// id and the tag. This is not the end of the request: the scheme still answers it, and
    /// that `scheme.response` (often `EINTR`) is the one to match.
    pub const SCHEME_CANCEL = 5 {
        name: "scheme.cancel",
        subsystem: Scheme,
        level: Info,
        args: ["scheme", "tag"],
    }

    /// The kernel handled a page fault: the faulting address, the instruction pointer, the
    /// fault's [`PageFaultFlags`] and how it ended (a [`PageFaultOutcome`]).
    ///
    /// One record per fault, written when the handler has decided, before a fault it could not
    /// handle becomes a signal or a kernel panic. Faults the kernel takes on purpose to probe
    /// memory (the shadow-stack pool check) are not recorded.
    pub const MM_PAGE_FAULT = 6 {
        name: "mm.page_fault",
        subsystem: Mm,
        level: Info,
        args: ["addr", "ip", "flags", "outcome"],
    }

    /// The kernel counts an interrupt for the `irq:` handles that get it, and wakes them: the
    /// IRQ number, the CPU the interrupt is counted for, how it arrived (an [`IrqSource`]),
    /// that CPU's count of the IRQ with this interrupt, and how many handles were woken.
    ///
    /// The count is what a driver writes back to acknowledge, so an `irq.ack` with the same
    /// IRQ, CPU and count ends this interrupt's handling. A count matches the next `irq.ack`
    /// only: later interrupts of the IRQ raise it.
    pub const IRQ_DELIVER = 7 {
        name: "irq.deliver",
        subsystem: Irq,
        level: Info,
        args: ["irq", "cpu", "source", "count", "handles"],
    }

    /// The kernel accepts a driver's acknowledgement on an `irq:` handle: the IRQ number, the
    /// CPU the handle counts interrupts for, and the count acknowledged. An acknowledgement
    /// of a count that is not the current one changes nothing and is not recorded.
    pub const IRQ_ACK = 8 {
        name: "irq.ack",
        subsystem: Irq,
        level: Info,
        args: ["irq", "cpu", "count"],
    }

    /// An IOMMU wrote a report: a VT-d fault recording register (a blocked DMA or interrupt
    /// request; always one report per record) or an AMD-Vi event log entry (any event,
    /// including the unit's own command and table errors). The record holds the unit's
    /// register base, the format of the report (an [`IommuFormat`]), the report's two 64-bit
    /// words as the hardware wrote them, and how many identical reports in a row it stands for.
    ///
    /// Conditions that have no report of their own are only logged, not recorded: VT-d's
    /// invalidation queue errors and primary fault overflow (fault status bits), and a full
    /// AMD-Vi event log.
    ///
    /// The words are kept raw so that nothing is lost; decoders read them with the format's
    /// own decoder (`ferric-vtd`'s `FaultRecord::decode`, `ferric-amdvi`'s `Event`).
    pub const IOMMU_FAULT = 9 {
        name: "iommu.fault",
        subsystem: Iommu,
        level: Error,
        args: ["unit", "format", "lo", "hi", "count"],
    }

    /// A futex comparison succeeded and the caller joined the wait queue, before the
    /// queue lock is released: the physical word key, the caller's virtual address,
    /// context debug id, word width (32 or 64 bits), compared value, and whether a
    /// timeout was supplied (0 or 1). Refused waits do not produce this event.
    ///
    /// Match the next `sched.futex_resume` with the same key and context, even across
    /// CPUs. A key is a physical byte address and may be reused after memory is freed.
    pub const SCHED_FUTEX_WAIT = 10 {
        name: "sched.futex_wait",
        subsystem: Sched,
        level: Info,
        args: ["key", "addr", "context", "bits", "expected", "timed"],
    }

    /// A queued futex wait finished, after removing any remaining waiter: the physical
    /// word key, virtual address, context debug id and the result in `Error::mux`
    /// encoding (0 for a wake, negated errno for timeout or interruption).
    ///
    /// This ends the wait, not the waking call: the caller may only run much later.
    pub const SCHED_FUTEX_RESUME = 11 {
        name: "sched.futex_resume",
        subsystem: Sched,
        level: Info,
        args: ["key", "addr", "context", "ret"],
    }

    /// A valid futex wake finished: the physical word key, the waker's virtual address,
    /// requested wake count and returned count. Calls that find no waiters, including
    /// requests to wake zero, are recorded too.
    /// The record is written before releasing the queue lock, so a waiter processed by
    /// this call cannot record its resume before this wake is recorded.
    ///
    /// The returned count is the number of live queue entries processed by the syscall;
    /// a timeout or signal can already have made an entry runnable before its cleanup.
    /// It does not count contexts that necessarily changed from blocked to runnable.
    pub const SCHED_FUTEX_WAKE = 12 {
        name: "sched.futex_wake",
        subsystem: Sched,
        level: Info,
        args: ["key", "addr", "requested", "woken"],
    }

    /// A context was successfully spawned and registered, before its caller can start it:
    /// the new context's debug id and whether it may enter user mode (0 or 1).
    ///
    /// No PID has been assigned yet. This covers `context::spawn`, including ring workers,
    /// but not the per-CPU idle contexts. Dropping an unstarted context need not emit an exit.
    pub const SCHED_CONTEXT_CREATE = 13 {
        name: "sched.context_create",
        subsystem: Sched,
        level: Info,
        args: ["context", "user_mode"],
    }

    /// An accepted transition from not-yet-started to runnable, under the context lock:
    /// the assigned PID and debug id. The context cannot run before this record is written.
    ///
    /// Covers the proc scheme's `start`, bootstrap and ring workers. This is not an exec
    /// or a process birth: multiple contexts may belong to one PID, and PID 0 can mean
    /// an unassigned PID or a kernel context. The emitter may be a manager on another context.
    pub const SCHED_CONTEXT_START = 14 {
        name: "sched.context_start",
        subsystem: Sched,
        level: Info,
        args: ["pid", "context"],
    }

    /// A running context completed resource cleanup and is marked dead, before its owner
    /// is notified: PID, debug id, whether it has an unhandled exception (0 or 1), and the
    /// exception's raw `redox_syscall::Exception` kind, code and address (zero when absent).
    ///
    /// This is a context exit, not a process exit status or signal number. Exceptions
    /// delivered to a user-space handler do not end the context here.
    pub const SCHED_CONTEXT_EXIT = 15 {
        name: "sched.context_exit",
        subsystem: Sched,
        level: Info,
        args: ["pid", "context", "exception", "kind", "code", "addr"],
    }

    /// An `AddrSpace::mmap` attempt completed under the address-space lock: the user
    /// page-table root's physical address, requested address (0 without a hint), page
    /// count, raw `redox_syscall::MapFlags`, and returned address or negated errno.
    ///
    /// `space` identifies an address space while it exists; its frame may be reused after
    /// destruction. The producer may be a manager operating on another address space.
    /// A fixed request first removes what was mapped in its span: an `mm.replace` record
    /// precedes this one. With `mm.replace`, `mm.move`, `mm.clone` and `mm.destroy` the `mm`
    /// events journal every change to a user address space's grants.
    pub const MM_MAP = 16 {
        name: "mm.map",
        subsystem: Mm,
        level: Info,
        args: ["space", "addr", "pages", "flags", "ret"],
    }

    /// An `AddrSpaceWrapper::mprotect` attempt completed under the address-space lock:
    /// page-table root, requested address/page count, raw `MapFlags`, and 0 or negated
    /// errno. Permissions can have changed on earlier grants even when a later one fails.
    /// The flags express the request, not necessarily hardware-enforced read protection.
    pub const MM_PROTECT = 17 {
        name: "mm.protect",
        subsystem: Mm,
        level: Info,
        args: ["space", "addr", "pages", "flags", "ret"],
    }

    /// An `AddrSpaceWrapper::munmap` attempt completed under the address-space lock:
    /// page-table root, requested address/page count, whether to unpin (0 or 1), and 0
    /// or negated errno. Holes are allowed; a failure can follow partial unmapping.
    /// This precedes scheme notifications. Internal removals have their own events:
    /// `mm.replace`, `mm.move` and `mm.destroy`.
    pub const MM_UNMAP = 18 {
        name: "mm.unmap",
        subsystem: Mm,
        level: Info,
        args: ["space", "addr", "pages", "unpin", "ret"],
    }

    /// A DMA domain successfully mapped a physical run and completed required IOTLB
    /// invalidations: domain ID, physical root table, IOVA, physical address, byte length,
    /// and read/write permission bits (each 0 or 1).
    ///
    /// The ID/root pair identifies a live domain, not its producer; both may be reused
    /// after destruction. Cached translations and identity tables bypass this operation.
    /// Failed attempts are absent, even if they changed tables before failing.
    pub const IOMMU_MAP = 19 {
        name: "iommu.map",
        subsystem: Iommu,
        level: Info,
        args: ["domain", "root", "iova", "phys", "bytes", "read", "write"],
    }

    /// A DMA domain successfully changed permissions and completed IOTLB invalidation:
    /// domain ID, physical root table, IOVA, byte length and read/write bits (0 or 1).
    /// Failed or partially completed changes are absent.
    pub const IOMMU_PROTECT = 20 {
        name: "iommu.protect",
        subsystem: Iommu,
        level: Info,
        args: ["domain", "root", "iova", "bytes", "read", "write"],
    }

    /// A DMA domain successfully unmapped a range and completed IOTLB invalidation:
    /// domain ID, physical root table, IOVA and byte length.
    ///
    /// This can be rollback of a failed map. Failed/partial unmaps and whole-domain
    /// destruction are absent, so these events do not form a complete mapping journal.
    pub const IOMMU_UNMAP = 21 {
        name: "iommu.unmap",
        subsystem: Iommu,
        level: Info,
        args: ["domain", "root", "iova", "bytes"],
    }

    /// A fixed mapping removed what was mapped in its span, under the address-space lock,
    /// before installing its new grant: page-table root, address, page count, and 0 or
    /// negated errno. Its causes are a `MAP_FIXED` `mmap` (an `mm.map` record follows), a
    /// fixed `mremap` or grant move destination (an `mm.move` record follows), and a
    /// registered-buffer revoke, which maps zeroed memory in place.
    ///
    /// The span may have held nothing. A failure can follow partial removal, and the
    /// operation then fails too.
    pub const MM_REPLACE = 22 {
        name: "mm.replace",
        subsystem: Mm,
        level: Info,
        args: ["space", "addr", "pages", "ret"],
    }

    /// An `AddrSpaceWrapper::r#move` attempt completed under the destination's address-space
    /// lock: the source and destination page-table roots (equal for `mremap` within one
    /// space), source address and page count, requested destination address (0 without
    /// one), new page count, raw `MapFlags`, and the destination address or negated errno.
    ///
    /// A move shorter than its source unmaps the source's tail; a longer one maps zeroed
    /// pages past it. A failure can follow partial changes to either space.
    pub const MM_MOVE = 23 {
        name: "mm.move",
        subsystem: Mm,
        level: Info,
        args: [
            "src_space",
            "dst_space",
            "src_addr",
            "src_pages",
            "dst_addr",
            "pages",
            "flags",
            "ret",
        ],
    }

    /// `AddrSpaceWrapper::try_clone` (a `fork`) completed under the source's lock: the
    /// source and new page-table roots, the grants and pages copied, and 0 or negated
    /// errno. On failure `new_space`, `grants` and `pages` are 0; a partly built copy has
    /// already been torn down with its own `mm.destroy` record. Grants a clone does not keep
    /// (pinned scheme borrows, contiguous driver memory, file-mapping borrows) are not
    /// counted.
    pub const MM_CLONE = 24 {
        name: "mm.clone",
        subsystem: Mm,
        level: Info,
        args: ["space", "new_space", "grants", "pages", "ret"],
    }

    /// An address space's grants are being torn down because the address space is
    /// dropped (its last context exited or exec'd, or a clone failed): page-table root and
    /// the grants and pages it still held. After this record `space` may be reused.
    pub const MM_DESTROY = 25 {
        name: "mm.destroy",
        subsystem: Mm,
        level: Info,
        args: ["space", "grants", "pages"],
    }
}

/// The `flags` of an `mm.page_fault` record: what the access was and what the CPU found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PageFaultFlags(u64);

impl PageFaultFlags {
    /// The page was present: the access broke its protection rather than finding no page.
    pub const PRESENT: Self = Self(1 << 0);
    /// The access was a write.
    pub const WRITE: Self = Self(1 << 1);
    /// The access came from user mode. Without it the kernel faulted, possibly while copying
    /// to or from user memory.
    pub const USER: Self = Self(1 << 2);
    /// The access was an instruction fetch.
    pub const INSTRUCTION: Self = Self(1 << 3);
    /// A page table entry has a reserved bit set.
    pub const RESERVED: Self = Self(1 << 4);

    /// The flags, with their names in [`fmt::Display`] order.
    const NAMED: [(Self, &'static str); 5] = [
        (Self::PRESENT, "present"),
        (Self::WRITE, "write"),
        (Self::USER, "user"),
        (Self::INSTRUCTION, "fetch"),
        (Self::RESERVED, "reserved"),
    ];

    /// No flags: a read by the kernel of a page that is not present.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The flags of a raw argument, keeping unknown bits.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw argument.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Whether every flag of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// These flags plus `other` when `on`.
    #[must_use]
    pub const fn with(self, other: Self, on: bool) -> Self {
        if on { Self(self.0 | other.0) } else { self }
    }
}

impl fmt::Display for PageFaultFlags {
    /// The access and the flags in words, such as `write user` or `read present`; unknown
    /// bits are shown in hexadecimal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let access = if self.contains(Self::WRITE) {
            "write"
        } else if self.contains(Self::INSTRUCTION) {
            "fetch"
        } else {
            "read"
        };
        f.write_str(access)?;
        let mut known = 0;
        for (flag, name) in Self::NAMED {
            known |= flag.0;
            if self.contains(flag) && flag != Self::WRITE && flag != Self::INSTRUCTION {
                write!(f, " {name}")?;
            }
        }
        if self.0 & !known != 0 {
            write!(f, " {:#x}", self.0 & !known)?;
        }
        Ok(())
    }
}

/// How the kernel ended a page fault: the `outcome` of an `mm.page_fault` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum PageFaultOutcome {
    /// The kernel mapped the page (demand paging, copy on write) and the access is retried.
    Mapped = 0,
    /// The kernel faulted copying to or from user memory that is not mapped, and the copy
    /// fails with `EFAULT`.
    CopyFailed = 1,
    /// The fault could not be handled: a signal for a user process, a panic in the kernel.
    Unhandled = 2,
}

impl PageFaultOutcome {
    /// Every outcome.
    pub const ALL: [Self; 3] = [Self::Mapped, Self::CopyFailed, Self::Unhandled];

    /// The raw argument.
    pub const fn raw(self) -> u64 {
        self as u64
    }

    /// The outcome of a raw argument, if it is one.
    pub fn from_raw(raw: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|outcome| outcome.raw() == raw)
    }

    /// The outcome's name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Mapped => "mapped",
            Self::CopyFailed => "copy failed",
            Self::Unhandled => "unhandled",
        }
    }
}

impl fmt::Display for PageFaultOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How an interrupt arrived: the `source` of an `irq.deliver` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum IrqSource {
    /// A legacy line through the PIC or I/O APIC, which belongs to the BSP's `irq:N` handles.
    Line = 0,
    /// A vector reserved on the CPU that took it (`irq:cpu-XX/N`), which MSI and MSI-X
    /// messages target.
    Vector = 1,
}

impl IrqSource {
    /// Every source.
    pub const ALL: [Self; 2] = [Self::Line, Self::Vector];

    /// The raw argument.
    pub const fn raw(self) -> u64 {
        self as u64
    }

    /// The source of a raw argument, if it is one.
    pub fn from_raw(raw: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.raw() == raw)
    }

    /// The source's name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Vector => "vector",
        }
    }
}

impl fmt::Display for IrqSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What the words of an `iommu.fault` record are: its `format`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum IommuFormat {
    /// An Intel VT-d fault recording register: `lo` is its low and `hi` its high 64 bits
    /// (VT-d specification, register 10.4.14, as `ferric-vtd` reads it).
    VtdFaultRecord = 1,
    /// An AMD-Vi event log entry: `lo` is its first and `hi` its second 64 bits (AMD I/O
    /// Virtualization Technology specification, section 2.5, as `ferric-amdvi` reads it).
    AmdViEvent = 2,
}

impl IommuFormat {
    /// Every format.
    pub const ALL: [Self; 2] = [Self::VtdFaultRecord, Self::AmdViEvent];

    /// The raw argument.
    pub const fn raw(self) -> u64 {
        self as u64
    }

    /// The format of a raw argument, if it is one.
    pub fn from_raw(raw: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.raw() == raw)
    }

    /// The format's name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::VtdFaultRecord => "VT-d fault record",
            Self::AmdViEvent => "AMD-Vi event",
        }
    }
}

impl fmt::Display for IommuFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Subsystem;

    #[test]
    fn names_start_with_their_subsystem() {
        for event in CATALOG.events() {
            let (subsystem, rest) = event.name().split_once('.').expect("a dotted name");
            assert_eq!(subsystem, event.subsystem().name(), "{}", event.name());
            assert!(!rest.is_empty());
        }
    }

    #[test]
    fn ids_are_stable() {
        assert_eq!(SYSCALL_ENTER.id().get(), 0);
        assert_eq!(SYSCALL_EXIT.id().get(), 1);
        assert_eq!(SCHED_SWITCH.id().get(), 2);
        assert_eq!(SCHED_SWITCH.subsystem(), Subsystem::Sched);
        assert_eq!(SCHEME_REQUEST.id().get(), 3);
        assert_eq!(SCHEME_RESPONSE.id().get(), 4);
        assert_eq!(SCHEME_CANCEL.id().get(), 5);
        assert_eq!(SCHEME_CANCEL.subsystem(), Subsystem::Scheme);
        assert_eq!(MM_PAGE_FAULT.id().get(), 6);
        assert_eq!(MM_PAGE_FAULT.subsystem(), Subsystem::Mm);
        assert_eq!(IRQ_DELIVER.id().get(), 7);
        assert_eq!(IRQ_ACK.id().get(), 8);
        assert_eq!(IRQ_ACK.subsystem(), Subsystem::Irq);
        assert_eq!(IOMMU_FAULT.id().get(), 9);
        assert_eq!(IOMMU_FAULT.subsystem(), Subsystem::Iommu);
        assert_eq!(SCHED_FUTEX_WAIT.id().get(), 10);
        assert_eq!(SCHED_FUTEX_RESUME.id().get(), 11);
        assert_eq!(SCHED_FUTEX_WAKE.id().get(), 12);
        assert_eq!(SCHED_CONTEXT_CREATE.id().get(), 13);
        assert_eq!(SCHED_CONTEXT_START.id().get(), 14);
        assert_eq!(SCHED_CONTEXT_EXIT.id().get(), 15);
        assert_eq!(MM_MAP.id().get(), 16);
        assert_eq!(MM_PROTECT.id().get(), 17);
        assert_eq!(MM_UNMAP.id().get(), 18);
        assert_eq!(IOMMU_MAP.id().get(), 19);
        assert_eq!(IOMMU_PROTECT.id().get(), 20);
        assert_eq!(IOMMU_UNMAP.id().get(), 21);
        assert_eq!(MM_REPLACE.id().get(), 22);
        assert_eq!(MM_MOVE.id().get(), 23);
        assert_eq!(MM_CLONE.id().get(), 24);
        assert_eq!(MM_DESTROY.id().get(), 25);
    }

    #[test]
    fn every_subsystem_has_events() {
        for subsystem in Subsystem::ALL {
            assert!(
                CATALOG.events().iter().any(|e| e.subsystem() == subsystem),
                "{subsystem}"
            );
        }
    }

    #[test]
    fn levels_follow_the_rules() {
        // Ordinary events are info, so the default `kernel.trace_max_level` has them; an
        // IOMMU fault is something going wrong.
        for event in [
            &MM_PAGE_FAULT,
            &MM_MAP,
            &MM_PROTECT,
            &MM_UNMAP,
            &MM_REPLACE,
            &MM_MOVE,
            &MM_CLONE,
            &MM_DESTROY,
            &IOMMU_MAP,
            &IOMMU_PROTECT,
            &IOMMU_UNMAP,
            &IRQ_DELIVER,
            &IRQ_ACK,
            &SCHED_FUTEX_WAIT,
            &SCHED_FUTEX_RESUME,
            &SCHED_FUTEX_WAKE,
            &SCHED_CONTEXT_CREATE,
            &SCHED_CONTEXT_START,
            &SCHED_CONTEXT_EXIT,
        ] {
            assert_eq!(event.level(), crate::Level::Info, "{}", event.name());
        }
        assert_eq!(IOMMU_FAULT.level(), crate::Level::Error);
    }

    #[test]
    fn lifecycle_events_use_the_existing_scheduler_controls() {
        let selected = crate::select(&CATALOG, "sched").unwrap();
        let compiled = CATALOG.compiled_for(|f| f == "trace_sched" || f == "trace_level_info");
        for event in [
            &SCHED_FUTEX_WAIT,
            &SCHED_FUTEX_RESUME,
            &SCHED_FUTEX_WAKE,
            &SCHED_CONTEXT_CREATE,
            &SCHED_CONTEXT_START,
            &SCHED_CONTEXT_EXIT,
        ] {
            assert_eq!(event.subsystem(), Subsystem::Sched);
            assert!(selected.contains(event.id()));
            assert!(compiled.contains(event.id()));
        }
    }

    #[test]
    fn mapping_events_use_the_existing_memory_controls() {
        let selected = crate::select(&CATALOG, "mm").unwrap();
        let compiled = CATALOG.compiled_for(|f| f == "trace_mm" || f == "trace_level_info");
        let absent = CATALOG.compiled_for(|f| f == "trace_level_info");
        for event in [
            &MM_PAGE_FAULT,
            &MM_MAP,
            &MM_PROTECT,
            &MM_UNMAP,
            &MM_REPLACE,
            &MM_MOVE,
            &MM_CLONE,
            &MM_DESTROY,
        ] {
            assert_eq!(event.subsystem(), Subsystem::Mm);
            assert!(selected.contains(event.id()));
            assert!(compiled.contains(event.id()));
            assert!(!absent.contains(event.id()));
        }
    }

    #[test]
    fn dma_mapping_events_use_the_existing_iommu_controls() {
        let selected = crate::select(&CATALOG, "iommu").unwrap();
        let compiled = CATALOG.compiled_for(|f| f == "trace_iommu" || f == "trace_level_info");
        let absent = CATALOG.compiled_for(|f| f == "trace_level_info");
        let errors = CATALOG.compiled_for(|f| f == "trace_iommu" || f == "trace_level_error");
        for event in [&IOMMU_MAP, &IOMMU_PROTECT, &IOMMU_UNMAP] {
            assert_eq!(event.subsystem(), Subsystem::Iommu);
            assert!(selected.contains(event.id()));
            assert!(compiled.contains(event.id()));
            assert!(!absent.contains(event.id()));
            assert!(!errors.contains(event.id()));
        }
        assert!(errors.contains(IOMMU_FAULT.id()));
    }

    #[test]
    fn page_fault_flags_read_as_words() {
        use std::string::ToString;
        let flags = PageFaultFlags::empty()
            .with(PageFaultFlags::WRITE, true)
            .with(PageFaultFlags::USER, true)
            .with(PageFaultFlags::PRESENT, false);
        assert_eq!(flags.raw(), 0b110);
        assert!(flags.contains(PageFaultFlags::USER));
        assert!(!flags.contains(PageFaultFlags::PRESENT));
        assert_eq!(flags.to_string(), "write user");
        assert_eq!(PageFaultFlags::empty().to_string(), "read");
        let fetch = PageFaultFlags::INSTRUCTION.with(PageFaultFlags::PRESENT, true);
        assert_eq!(fetch.to_string(), "fetch present");
        assert_eq!(
            PageFaultFlags::from_raw(0x101 | PageFaultFlags::RESERVED.raw()).to_string(),
            "read present reserved 0x100"
        );
    }

    #[test]
    fn codes_round_trip() {
        for outcome in PageFaultOutcome::ALL {
            assert_eq!(PageFaultOutcome::from_raw(outcome.raw()), Some(outcome));
        }
        assert_eq!(PageFaultOutcome::from_raw(3), None);
        for source in IrqSource::ALL {
            assert_eq!(IrqSource::from_raw(source.raw()), Some(source));
        }
        assert_eq!(IrqSource::from_raw(2), None);
        for format in IommuFormat::ALL {
            assert_eq!(IommuFormat::from_raw(format.raw()), Some(format));
        }
        assert_eq!(IommuFormat::from_raw(0), None);
    }
}
