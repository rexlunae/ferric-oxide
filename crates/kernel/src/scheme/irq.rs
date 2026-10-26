// TODO: Rewrite this entire scheme. Legacy x86 APIs should be abstracted by a userspace scheme,
// this scheme should only handle raw IRQ registration and delivery to userspace.

use alloc::{borrow::ToOwned, string::String, vec::Vec};
use core::{
    str,
    str::FromStr,
    sync::atomic::{AtomicUsize, Ordering},
};

use smallvec::SmallVec;
use spin::{Mutex, Once};
use syscall::{
    data::GlobalSchemes,
    dirent::{DirEntry, DirentBuf, DirentKind},
};

use crate::context::file::InternalFlags;

use super::{CallerCtx, HandleMap, OpenResult, SchemeExt, StrOrBytes};
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
use crate::arch::interrupt::{available_irqs_iter, irq::acknowledge, is_reserved, set_reserved};
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
use crate::dtb::irqchip::{acknowledge, available_irqs_iter, is_reserved, set_reserved, IRQ_CHIP};
use crate::{
    cpu_set::LogicalCpuId,
    event,
    sync::{CleanLockToken, RwLock, L1},
    syscall::{
        data::Stat,
        error::*,
        flag::{
            CallFlags, EventFlags, EVENT_READ, MODE_CHR, MODE_DIR, O_CREAT, O_DIRECTORY, O_STAT,
        },
        usercopy::{UserSliceRo, UserSliceRw, UserSliceWo},
    },
};

///
/// IRQ queues
pub(super) static COUNTS: Mutex<[usize; 224]> = Mutex::new([0; 224]);

/// Ferric Oxide: serializes reserving, steering and giving back `irq:` vectors, so a move
/// works from the route its handle has when it starts and keeps it until the handles are
/// re-keyed: a second move of the same vector waits and then starts from where the first left
/// it, and a close cannot give back the old vector while its remapping entries are being
/// rewritten. Taken before [`HANDLES`], never while holding it, and never in interrupt context.
static VECTORS: Mutex<()> = Mutex::new(());
static HANDLES: RwLock<L1, HandleMap<Handle>> = RwLock::new(HandleMap::new());

/// These are IRQs 0..=15 (corresponding to interrupt vectors 32..=47). They are opened without the
/// O_CREAT flag.
const BASE_IRQ_COUNT: u8 = 16;

/// These are the extended IRQs, 16..=223 (interrupt vectors 48..=255). Some of them are reserved
/// for other devices, and some other interrupt vectors like 0x80 (software interrupts) and
/// 0x40..=0x43 (IPI).
///
/// Since these are non-sharable, they must be opened with O_CREAT, which then reserves them. They
/// are only freed when the file descriptor is closed.
const TOTAL_IRQ_COUNT: u8 = 224;

const INO_TOPLEVEL: u64 = 0x8002_0000_0000_0000;
const INO_AVAIL: u64 = 0x8000_0000_0000_0000;
const INO_BSP: u64 = 0x8001_0000_0000_0000;
const INO_PHANDLE: u64 = 0x8003_0000_0000_0000;
// Ferric Oxide: `irq:cpus`.
#[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
const INO_CPUS: u64 = 0x8004_0000_0000_0000;
/// Ferric Oxide: the root `getdents` position of `cpus`, after every `cpu-XX` (`XX` is a `u8`).
#[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
const CPUS_DIRENT: usize = 0x101;

/// Ferric Oxide (`irq-percpu`): the interrupt counts per (logical CPU, IRQ), which replace
/// [`COUNTS`] for counting and delivery, so the same vector number reserved on two CPUs is two
/// interrupts, and each CPU counts in its own row without a lock.
#[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
static PERCPU_COUNTS: ferric_irq::Counts<{ crate::cpu_set::MAX_CPU_COUNT as usize }> =
    ferric_irq::Counts::new();

/// Counts an interrupt of `irq` for the handles on `owner`, and returns the CPU whose handles get
/// it (Ferric Oxide: the BSP, as upstream, without `irq-percpu`) together with the count this
/// delivery made.
///
/// The count is returned here rather than read again afterwards: without `irq-percpu` another
/// CPU can count the same number between the increment and a later read, and `irq.deliver` must
/// name the count this delivery made for the driver's `irq.ack` to pair with it.
fn count_interrupt(irq: u8, owner: LogicalCpuId) -> (LogicalCpuId, usize) {
    #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
    if let Some(number) = ferric_irq::Irq::new(irq) {
        // Every online CPU has a row: there are `MAX_CPU_COUNT` of them.
        let count = PERCPU_COUNTS
            .trigger(ferric_irq::CpuId::new(owner.get()), number)
            .unwrap_or(0);
        return (owner, count);
    }
    let _ = owner;
    let count = COUNTS.lock().get_mut(usize::from(irq)).map_or(0, |count| {
        *count = count.wrapping_add(1);
        *count
    });
    (LogicalCpuId::BSP, count)
}

/// Ferric Oxide: whether a handle for `irq` on `cpu` gets only the interrupts of a vector below
/// 16 reserved on an AP (`irq:cpu-XX/N`), which no PIC or I/O APIC line backs: the legacy IRQs
/// 0-15 exist only on the BSP.
///
/// Only with `irq-percpu`, where such a handle counts only its own CPU's interrupts. Without
/// it, counts are per number as upstream, so the handle also sees the BSP's legacy IRQ of the
/// same number and must unmask its line when acknowledging, or the line stays masked.
const fn ap_low_vector(cpu: LogicalCpuId, irq: u8) -> bool {
    cfg!(all(feature = "irq-percpu", target_arch = "x86_64"))
        && irq < BASE_IRQ_COUNT
        && cpu.get() != LogicalCpuId::BSP.get()
}

/// The interrupts of `irq` that handles on `cpu` have had (Ferric Oxide: `cpu` matters only
/// with `irq-percpu`).
fn interrupt_count(cpu: LogicalCpuId, irq: u8) -> usize {
    #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
    if let Some(number) = ferric_irq::Irq::new(irq) {
        return PERCPU_COUNTS
            .get(ferric_irq::CpuId::new(cpu.get()), number)
            .unwrap_or(0);
    }
    let _ = cpu;
    total_count(irq)
}

/// The interrupts of `irq` so far on all CPUs, for `sys:irq`.
pub(super) fn total_count(irq: u8) -> usize {
    #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
    if let Some(number) = ferric_irq::Irq::new(irq) {
        return PERCPU_COUNTS.total(number);
    }
    COUNTS.lock().get(usize::from(irq)).copied().unwrap_or(0)
}

/// Whether a handle for `irq` on `handle_cpu` gets an interrupt of `irq` counted for `cpu`
/// (Ferric Oxide: every handle for `irq` does without `irq-percpu`, as upstream).
fn delivers_to(handle_cpu: LogicalCpuId, cpu: LogicalCpuId) -> bool {
    !cfg!(all(feature = "irq-percpu", target_arch = "x86_64")) || handle_cpu == cpu
}

/// Add to the input queue
///
/// Ferric Oxide: for interrupts from the PIC or I/O APIC (and the interrupt controllers of other
/// architectures), which belong to the BSP's `irq:N` handles wherever they are taken. A vector
/// reserved on a CPU goes through `vector_trigger`.
pub fn irq_trigger(irq: u8, token: &mut CleanLockToken) {
    deliver(irq, LogicalCpuId::BSP, Source::Line, token);
}

/// Ferric Oxide: adds an interrupt that arrived through a vector reserved on the running CPU
/// (`irq:cpu-XX/N`) to the input queue. With `irq-percpu` it belongs to that CPU's handles, even
/// for numbers below 16, which on an AP are ordinary vectors; without it, as upstream, to every
/// handle for the number.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn vector_trigger(irq: u8, token: &mut CleanLockToken) {
    deliver(irq, crate::cpu_id(), Source::Vector, token);
}

/// Ferric Oxide: how an interrupt reached [`deliver`], for `irq.deliver`
/// (`ferric_trace::kernel::IrqSource`).
#[derive(Clone, Copy)]
enum Source {
    /// [`irq_trigger`]: a PIC or I/O APIC line.
    Line,
    /// `vector_trigger`: a vector reserved on the running CPU (MSI, MSI-X).
    #[cfg_attr(
        not(any(target_arch = "x86", target_arch = "x86_64")),
        allow(dead_code)
    )]
    Vector,
}

/// Counts an interrupt of `irq` for `owner` and wakes the handles that get it.
fn deliver(irq: u8, owner: LogicalCpuId, source: Source, token: &mut CleanLockToken) {
    let _ = source;
    let (cpu, count) = count_interrupt(irq, owner);
    // Like `source`, `count` is only read by the record below, which is compiled out without
    // `trace`.
    let _ = count;
    let fds: SmallVec<[usize; 8]> = {
        HANDLES
            .read(token.token())
            .iter()
            .filter_map(|(fd, handle)| Some((fd, handle.as_irq_handle()?)))
            .filter(|&(_, (_, handle_irq, handle_cpu))| {
                handle_irq == irq && delivers_to(handle_cpu, cpu)
            })
            .map(|(f, _)| *f)
            .collect()
    };

    // Ferric Oxide (`kernel.trace_irq`): recorded before the handles wake, so a driver's
    // acknowledgement always comes after it.
    crate::trace::trace_event!(
        ferric_trace::kernel::IRQ_DELIVER,
        irq,
        cpu.get(),
        match source {
            Source::Line => ferric_trace::kernel::IrqSource::Line,
            Source::Vector => ferric_trace::kernel::IrqSource::Vector,
        }
        .raw(),
        count,
        fds.len(),
    );
    for fd in fds {
        event::trigger(GlobalSchemes::Irq.scheme_id(), fd, EVENT_READ, token);
    }
}

#[allow(dead_code)]
enum Handle {
    SchemeRoot,
    Irq {
        ack: AtomicUsize,
        irq: u8,
        // Ferric Oxide: the logical CPU whose interrupts this handle counts (the BSP for the
        // legacy IRQs and `irq:N`).
        cpu: LogicalCpuId,
        reservation: Option<IrqReservation>,
    },
    // Ferric Oxide (`irq-percpu`): the root-only `irq:cpus` map of online logical CPUs to
    // APIC ids and NUMA nodes, one `ferric_irq::CpuEntry` per line.
    #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
    Cpus(Vec<u8>),
    Avail(LogicalCpuId),
    TopLevel,
    Phandle(u8, Vec<u8>),
    Bsp,
}

#[derive(Clone, Copy)]
struct IrqReservation {
    cpu_id: LogicalCpuId,
    index: u8,
}

impl Handle {
    fn as_irq_handle(&self) -> Option<(&AtomicUsize, u8, LogicalCpuId)> {
        match self {
            &Self::Irq {
                ref ack, irq, cpu, ..
            } => Some((ack, irq, cpu)),
            _ => None,
        }
    }
}

/// The MADT's local APICs as (ACPI processor UID, APIC ID); `irq:` lists the UIDs (Ferric Oxide:
/// the APIC ID is kept to leave out the profiler CPU, see [`is_profiler_apic`]).
static CPUS: Once<Vec<(u8, u8)>> = Once::new();

pub struct IrqScheme;

impl IrqScheme {
    pub fn init() {
        let cpus = if cfg!(all(
            feature = "acpi",
            any(target_arch = "x86", target_arch = "x86_64")
        )) {
            use crate::acpi::madt::{madt, MadtEntry};

            match madt() {
                Some(madt) => madt
                    .iter()
                    .filter_map(|entry| match entry {
                        MadtEntry::LocalApic(apic) => Some((apic.processor, apic.id)),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                None => {
                    crate::warn!("no MADT found, defaulting to 1 CPU");
                    alloc::vec![(0, 0)]
                }
            }
        } else {
            alloc::vec![(0, 0)]
        };

        CPUS.call_once(|| cpus);
    }
    fn open_ext_irq(
        flags: usize,
        cpu_id: LogicalCpuId,
        path_str: &str,
    ) -> Result<(Handle, InternalFlags)> {
        let irq_number = u8::from_str(path_str).or(Err(Error::new(ENOENT)))?;

        Ok(
            if irq_number < BASE_IRQ_COUNT && cpu_id == LogicalCpuId::BSP {
                // Give legacy IRQs only to `irq:{0..15}` and `irq:cpu-<BSP>/{0..15}` (same handles).
                //
                // The only CPUs don't have the legacy IRQs in their IDTs.

                (
                    Handle::Irq {
                        ack: AtomicUsize::new(0),
                        irq: irq_number,
                        cpu: LogicalCpuId::BSP,
                        reservation: None,
                    },
                    InternalFlags::empty(),
                )
            } else if irq_number < TOTAL_IRQ_COUNT {
                if flags & O_CREAT == 0 && flags & O_STAT == 0 {
                    return Err(Error::new(EINVAL));
                }
                let mut ack = 0;
                let reservation = if flags & O_STAT == 0 {
                    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
                    {
                        let index = irq_to_vector(irq_number);
                        let _vectors = VECTORS.lock();
                        if is_reserved(cpu_id, index) {
                            return Err(Error::new(EEXIST));
                        }
                        // Taken before the vector is published as reserved, so an interrupt
                        // that arrives for the new owner right after is reported to it.
                        ack = fresh_ack(cpu_id, irq_number);
                        set_reserved(cpu_id, index, true);
                        Some(IrqReservation { cpu_id, index })
                    }
                    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
                    None
                } else {
                    None
                };
                (
                    Handle::Irq {
                        ack: AtomicUsize::new(ack),
                        irq: irq_number,
                        cpu: cpu_id,
                        reservation,
                    },
                    InternalFlags::empty(),
                )
            } else {
                return Err(Error::new(ENOENT));
            },
        )
    }

    #[cfg(dtb)]
    unsafe fn open_phandle_irq(
        flags: usize,
        phandle: usize,
        path_str: &str,
    ) -> Result<(Handle, InternalFlags)> {
        unsafe {
            let addr: Vec<u32> = path_str
                .split(',')
                .map(|x| u32::from_str(x).or(Err(Error::new(ENOENT))))
                .collect::<Result<_, _>>()?;
            let ic_idx = IRQ_CHIP
                .phandle_to_ic_idx(phandle as u32)
                .ok_or(Error::new(ENOENT))?;
            Ok({
                if flags & O_CREAT == 0 && flags & O_STAT == 0 {
                    return Err(Error::new(EINVAL));
                }
                let irq_number = IRQ_CHIP
                    .irq_xlate(ic_idx, addr.as_slice())
                    .or(Err(Error::new(ENOENT)))?;
                let irq_number = u8::try_from(irq_number).or(Err(Error::new(ENOENT)))?;
                if irq_number >= TOTAL_IRQ_COUNT {
                    return Err(Error::new(ENOENT));
                }
                crate::debug!("open_phandle_irq  virq={}", irq_number);
                let reservation = if flags & O_STAT == 0 {
                    let cpu_id = LogicalCpuId::BSP;
                    if is_reserved(cpu_id, irq_number) {
                        return Err(Error::new(EEXIST));
                    }
                    set_reserved(cpu_id, irq_number, true);
                    Some(IrqReservation {
                        cpu_id,
                        index: irq_number,
                    })
                } else {
                    None
                };
                (
                    Handle::Irq {
                        ack: AtomicUsize::new(0),
                        irq: irq_number,
                        cpu: LogicalCpuId::BSP,
                        reservation,
                    },
                    InternalFlags::empty(),
                )
            })
        }
    }
}

/// The CPUs `irq:` lists as `cpu-XX` (Ferric Oxide: with `irq-percpu`, the running logical
/// CPUs, which is what `cpu-XX` opens; upstream lists the MADT's ACPI processor UIDs).
fn listed_cpus() -> Vec<u8> {
    #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
    {
        crate::percpu::all_percpu_blocks()
            .filter(|block| !crate::profiling::is_profiler_cpu(block.cpu_id))
            .filter_map(|block| u8::try_from(block.cpu_id.get()).ok())
            .collect()
    }
    #[cfg(not(all(feature = "irq-percpu", target_arch = "x86_64")))]
    {
        CPUS.get()
            .expect("IRQ scheme not initialized")
            .iter()
            .filter(|&&(_, apic)| !is_profiler_apic(apic))
            .map(|&(uid, _)| uid)
            .collect()
    }
}

/// Ferric Oxide: whether local APIC `apic` is the `profiling` feature's profiler CPU, which
/// `cpu-XX` refuses to open (#214), so the MADT listing leaves it out as well.
#[cfg(all(
    feature = "profiling",
    not(all(feature = "irq-percpu", target_arch = "x86_64")),
    any(target_arch = "x86", target_arch = "x86_64")
))]
fn is_profiler_apic(apic: u8) -> bool {
    crate::percpu::all_percpu_blocks().any(|block| {
        crate::profiling::is_profiler_cpu(block.cpu_id)
            && block.misc_arch_info.apic_id_opt.get().map(|id| id.get()) == Some(u32::from(apic))
    })
}

/// Without `profiling` (or off x86) there is no profiler CPU to leave out.
#[cfg(all(
    not(all(
        feature = "profiling",
        any(target_arch = "x86", target_arch = "x86_64")
    )),
    not(all(feature = "irq-percpu", target_arch = "x86_64"))
))]
fn is_profiler_apic(_apic: u8) -> bool {
    false
}

/// Ferric Oxide (`irq-percpu`): the contents of `irq:cpus`, a [`ferric_irq::CpuEntry`] line for
/// each running CPU that has an APIC id.
#[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
fn cpus_file() -> Vec<u8> {
    use core::fmt::Write;

    let mut text = String::new();
    for block in crate::percpu::all_percpu_blocks() {
        if crate::profiling::is_profiler_cpu(block.cpu_id) {
            continue;
        }
        let Some(apic) = block.misc_arch_info.apic_id_opt.get() else {
            continue;
        };
        let entry = ferric_irq::CpuEntry {
            cpu: ferric_irq::CpuId::new(block.cpu_id.get()),
            apic: ferric_irq::ApicId::new(apic.get()),
            node: block.numa_node.get().map(|(node, _)| node.get()),
        };
        // Writing to a `String` cannot fail.
        let _ = writeln!(text, "{entry}");
    }
    text.into_bytes()
}

/// Ferric Oxide (`irq-percpu`, roadmap Phase 2.4 PR 3): steers the vector of handle `id` to
/// logical CPU `to`, as [`ferric_irq::call::Call::Move`] describes.
///
/// The steps keep every interrupt accounted for:
///
/// 1. A free vector is reserved on `to`, so the move cannot fail for want of one later.
/// 2. Every interrupt remapping entry that delivered the old vector to the old CPU is rewritten
///    to deliver the new one (`crate::arch::iommu::interrupt::move_vector`); the device's
///    message stays as it is. The old vector stays reserved meanwhile, so what the unit had
///    already sent the old way is still counted on the old CPU's row.
/// 3. Every handle of the old (CPU, vector) is re-keyed to the new one, with an interrupt
///    pending, so whatever arrived on the old vector during the move wakes its reader (at worst
///    one spurious wakeup). Then the old vector is given back.
///
/// Without remapping entries for the vector (`Moved::remapped` is 0), only the reservation and
/// the handles move: the device still sends the old vector until its driver programs the new
/// message, which only the owner can do, so a handle that reserved nothing is refused then.
#[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
fn steer(
    id: usize,
    to: LogicalCpuId,
    token: &mut CleanLockToken,
) -> Result<ferric_irq::call::Moved> {
    let vectors = VECTORS.lock();
    let (from, irq, owns) = {
        let handles = HANDLES.read(token.token());
        let Handle::Irq {
            irq,
            cpu,
            reservation,
            ..
        } = *handles.get(id)?
        else {
            return Err(Error::new(EBADF));
        };
        // The vector must be one an `irq:` handle reserved: not a legacy IRQ, not the kernel's.
        let reserved = handles.iter().any(|(_, handle)| {
            matches!(handle, Handle::Irq { cpu: c, irq: i, reservation: Some(_), .. }
                if *c == cpu && *i == irq)
        });
        if !reserved || (cpu == LogicalCpuId::BSP && irq < BASE_IRQ_COUNT) {
            return Err(Error::new(EINVAL));
        }
        (cpu, irq, reservation.is_some())
    };
    if to == from {
        return Err(Error::new(EINVAL));
    }
    if crate::percpu::interrupt_target(to).is_none() {
        return Err(Error::new(ENOENT));
    }
    let from_vector = irq_to_vector(irq);
    // IRQs 16 and up, as drivers reserve them; 0x80 stays the kernel's, as for AllocIrte.
    let to_vector = available_irqs_iter(to)
        .find(|&vector| vector >= irq_to_vector(BASE_IRQ_COUNT) && vector != 0x80)
        .ok_or(Error::new(EEXIST))?;
    set_reserved(to, to_vector, true);
    let remapped = match remap(from, from_vector, to, to_vector) {
        Ok(0) if !owns => Err(Error::new(EOPNOTSUPP)),
        result => result,
    };
    let remapped = match remapped {
        Ok(remapped) => remapped,
        Err(err) => {
            set_reserved(to, to_vector, false);
            return Err(err);
        }
    };
    let new_irq = vector_to_irq(to_vector);
    let (fds, moved_reservation) = {
        let mut handles = HANDLES.write(token.token());
        let pending = interrupt_count(to, new_irq).wrapping_sub(1);
        let mut fds: SmallVec<[usize; 4]> = SmallVec::new();
        let mut moved_reservation = false;
        for (&fd, handle) in handles.iter_mut() {
            let Handle::Irq {
                ack,
                irq: handle_irq,
                cpu,
                reservation,
            } = handle
            else {
                continue;
            };
            if *cpu != from || *handle_irq != irq {
                continue;
            }
            *cpu = to;
            *handle_irq = new_irq;
            ack.store(pending, Ordering::SeqCst);
            if let Some(reservation) = reservation {
                *reservation = IrqReservation {
                    cpu_id: to,
                    index: to_vector,
                };
                moved_reservation = true;
            }
            fds.push(fd);
        }
        (fds, moved_reservation)
    };
    if moved_reservation {
        set_reserved(from, from_vector, false);
    } else {
        // Not reached: the owner was there when the move started, under `VECTORS`, and its
        // close waits for the lock. Kept so the new vector is never left reserved by nobody.
        set_reserved(to, to_vector, false);
    }
    drop(vectors);
    for fd in fds {
        event::trigger(GlobalSchemes::Irq.scheme_id(), fd, EVENT_READ, token);
    }
    ferric_irq::Irq::new(new_irq)
        .map(|irq| ferric_irq::call::Moved { irq, remapped })
        .ok_or(Error::new(EIO))
}

/// Ferric Oxide: has the IOMMU move the interrupt remapping entries of `from_vector` on `from`
/// to `to_vector` on `to`, and returns how many moved; 0 without IOMMU support.
#[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
fn remap(from: LogicalCpuId, from_vector: u8, to: LogicalCpuId, to_vector: u8) -> Result<u32> {
    #[cfg(iommu_enabled)]
    {
        use crate::arch::iommu::interrupt::{move_vector, IrqError};
        move_vector(from.get(), from_vector, to.get(), to_vector).map_err(|err| {
            crate::warn!(
                "irq: cannot move vector {from_vector:#x} from CPU {} to CPU {}: {err}",
                from.get(),
                to.get()
            );
            if matches!(err, IrqError::Hardware(_)) {
                // Best effort: point the entries back where the old vector is still reserved.
                let _ = move_vector(to.get(), to_vector, from.get(), from_vector);
            }
            Error::new(match err {
                IrqError::Hardware(_) => EIO,
                IrqError::NoCpu { .. } => ENOENT,
                _ => EINVAL,
            })
        })
    }
    #[cfg(not(iommu_enabled))]
    {
        let _ = (from, from_vector, to, to_vector);
        Ok(0)
    }
}

/// Ferric Oxide: the acknowledged count a new reservation of `irq` on `cpu` starts with. With
/// `kernel.irq_percpu` a vector can be reserved again after its owner gave it back (or after it
/// was steered elsewhere), and its row still holds the old owner's interrupts; the new handle
/// starts acknowledged at that count, so it reports only interrupts that arrive for it. The
/// caller reads it before marking the vector reserved. Without `irq_percpu`, 0, as upstream.
fn fresh_ack(cpu: LogicalCpuId, irq: u8) -> usize {
    #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
    return interrupt_count(cpu, irq);
    #[cfg(not(all(feature = "irq-percpu", target_arch = "x86_64")))]
    {
        let _ = (cpu, irq);
        0
    }
}

const fn irq_to_vector(irq: u8) -> u8 {
    irq + 32
}
const fn vector_to_irq(vector: u8) -> u8 {
    vector - 32
}

impl crate::scheme::KernelScheme for IrqScheme {
    fn scheme_root(&self, token: &mut CleanLockToken) -> Result<usize> {
        let id = HANDLES.write(token.token()).insert(Handle::SchemeRoot);
        Ok(id)
    }
    fn kopenat(
        &self,
        id: usize,
        user_buf: StrOrBytes,
        flags: usize,
        _fcntl_flags: u32,
        ctx: CallerCtx,
        token: &mut CleanLockToken,
    ) -> Result<OpenResult> {
        {
            let handles = HANDLES.read(token.token());
            let handle = handles.get(id)?;

            if !matches!(handle, Handle::SchemeRoot) {
                return Err(Error::new(EACCES));
            }
        }

        let path = user_buf.as_str().or(Err(Error::new(EINVAL)))?;
        if ctx.uid != 0 {
            return Err(Error::new(EACCES));
        }

        let path_str = path.trim_start_matches('/');

        let (handle, int_flags) = if path_str.is_empty() {
            if flags & O_DIRECTORY == 0 && flags & O_STAT == 0 {
                return Err(Error::new(EISDIR));
            }
            // list every logical CPU in the format of e.g. `cpu-1b`

            let mut bytes = String::new();

            use core::fmt::Write;

            writeln!(bytes, "bsp").unwrap();
            #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
            let _ = writeln!(bytes, "cpus");

            for cpu_id in listed_cpus() {
                writeln!(bytes, "cpu-{:02x}", cpu_id).unwrap();
            }

            #[cfg(dtb)]
            unsafe {
                for chip in &IRQ_CHIP.irq_chip_list.chips {
                    writeln!(bytes, "phandle-{}", chip.phandle).unwrap();
                }
            }

            (Handle::TopLevel, InternalFlags::POSITIONED)
        } else if path_str == "bsp" {
            (Handle::Bsp, InternalFlags::empty())
        } else if cfg!(all(feature = "irq-percpu", target_arch = "x86_64")) && path_str == "cpus" {
            #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
            {
                (Handle::Cpus(cpus_file()), InternalFlags::POSITIONED)
            }
            #[cfg(not(all(feature = "irq-percpu", target_arch = "x86_64")))]
            unreachable!()
        } else if let Some(path_str) = path_str.strip_prefix("cpu-") {
            let cpu_id = u8::from_str_radix(path_str.get(..2).ok_or(Error::new(ENOENT))?, 16)
                .or(Err(Error::new(ENOENT)))?;
            let path_str = path_str[2..].trim_end_matches('/');
            // Ferric Oxide: a CPU that is not running has no IDT to reserve vectors in (and
            // looking for one used to panic), and the profiler CPU would drop them (#214).
            if crate::percpu::interrupt_target(LogicalCpuId::new(cpu_id.into())).is_none() {
                return Err(Error::new(ENOENT));
            }

            if path_str.is_empty() {
                (
                    Handle::Avail(LogicalCpuId::new(cpu_id.into())),
                    InternalFlags::POSITIONED,
                )
            } else if let Some(path_str) = path_str.strip_prefix('/') {
                Self::open_ext_irq(flags, LogicalCpuId::new(cpu_id.into()), path_str)?
            } else {
                return Err(Error::new(ENOENT));
            }
        } else if cfg!(dtb) && path_str.starts_with("phandle-") {
            #[cfg(dtb)]
            unsafe {
                let (phandle_str, path_str) =
                    path_str[8..].split_once('/').unwrap_or((path_str, ""));
                let phandle = usize::from_str(phandle_str).or(Err(Error::new(ENOENT)))?;
                if path_str.is_empty() {
                    let has_any = IRQ_CHIP.irq_iter_for(phandle as u32).next().is_some();
                    if has_any {
                        let data = String::new();
                        (
                            Handle::Phandle(phandle as u8, data.into_bytes()),
                            InternalFlags::POSITIONED,
                        )
                    } else {
                        return Err(Error::new(ENOENT));
                    }
                } else {
                    Self::open_phandle_irq(flags, phandle, path_str)?
                }
            }
            #[cfg(not(dtb))]
            panic!("")
        } else if let Ok(plain_irq_number) = u8::from_str(path_str) {
            if plain_irq_number < BASE_IRQ_COUNT {
                (
                    Handle::Irq {
                        ack: AtomicUsize::new(0),
                        irq: plain_irq_number,
                        cpu: LogicalCpuId::BSP,
                        reservation: None,
                    },
                    InternalFlags::empty(),
                )
            } else {
                return Err(Error::new(ENOENT));
            }
        } else {
            return Err(Error::new(ENOENT));
        };
        let fd = HANDLES.write(token.token()).insert(handle);
        Ok(OpenResult::SchemeLocal(fd, int_flags))
    }
    fn getdents(
        &self,
        id: usize,
        buf: UserSliceWo,
        header_size: u16,
        opaque_id_start: u64,
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let Ok(opaque) = usize::try_from(opaque_id_start) else {
            return Ok(0);
        };

        use core::fmt::Write;

        let mut buf = DirentBuf::new(buf, header_size).ok_or(Error::new(EIO))?;
        let mut intermediate = String::new();

        match *HANDLES.read(token.token()).get(id)? {
            Handle::TopLevel => {
                let cpus = listed_cpus();

                if opaque == 0 {
                    buf.entry(DirEntry {
                        inode: 0,
                        next_opaque_id: 1,
                        kind: DirentKind::CharDev,
                        name: "bsp",
                    })?;
                }

                // list every logical CPU in the format of e.g. `cpu-1b`
                for cpu_id in cpus.iter().filter(|i| opaque <= usize::from(**i)) {
                    intermediate.clear();
                    write!(&mut intermediate, "cpu-{:02x}", cpu_id).unwrap();
                    buf.entry(DirEntry {
                        kind: DirentKind::Directory,
                        name: &intermediate,
                        inode: 0,
                        next_opaque_id: u64::from(*cpu_id + 1),
                    })?;
                }

                // Ferric Oxide: `cpus` comes after the CPU directories, whose next ids are at
                // most 256, so its own id resumes after it without repeating or skipping.
                #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
                if opaque <= CPUS_DIRENT {
                    buf.entry(DirEntry {
                        inode: INO_CPUS,
                        next_opaque_id: CPUS_DIRENT as u64 + 1,
                        kind: DirentKind::CharDev,
                        name: "cpus",
                    })?;
                }
            }
            Handle::Avail(cpu_id) => {
                for vector in available_irqs_iter(cpu_id).skip(opaque) {
                    let irq = vector_to_irq(vector);
                    if cpu_id == LogicalCpuId::BSP && irq < BASE_IRQ_COUNT {
                        continue;
                    }
                    intermediate.clear();
                    write!(intermediate, "{}", irq).unwrap();
                    buf.entry(DirEntry {
                        inode: 0,
                        kind: DirentKind::CharDev,
                        name: &intermediate,
                        next_opaque_id: u64::from(vector) + 1,
                    })?;
                }
            }
            _ => return Err(Error::new(ENOTDIR)),
        }
        Ok(buf.finalize())
    }

    // Ferric Oxide: `SYS_CALL` on an irq handle (`ferric_irq::call`).
    fn kcall(
        &self,
        fds: &[usize],
        _payload: UserSliceRw,
        _flags: CallFlags,
        metadata: &[u64],
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let [id]: [usize; 1] = fds.try_into().map_err(|_| Error::new(EINVAL))?;
        #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
        {
            use ferric_irq::call::Call;
            match Call::decode(metadata).map_err(|_| Error::new(EINVAL))? {
                Call::Move { cpu } => {
                    let moved = steer(id, LogicalCpuId::new(cpu.get()), token)?;
                    usize::try_from(moved.to_word()).map_err(|_| Error::new(EIO))
                }
                _ => Err(Error::new(EINVAL)),
            }
        }
        #[cfg(not(all(feature = "irq-percpu", target_arch = "x86_64")))]
        {
            let _ = (id, metadata, token);
            Err(Error::new(EOPNOTSUPP))
        }
    }

    fn fcntl(
        &self,
        _id: usize,
        _cmd: usize,
        _arg: usize,
        _token: &mut CleanLockToken,
    ) -> Result<usize> {
        Ok(0)
    }

    fn fevent(
        &self,
        id: usize,
        flags: EventFlags,
        token: &mut CleanLockToken,
    ) -> Result<EventFlags> {
        let handles_guard = HANDLES.read(token.token());
        let handle = handles_guard.get(id)?;
        let (handle_ack, handle_irq, handle_cpu) =
            handle.as_irq_handle().ok_or(Error::new(EBADF))?;

        let current = interrupt_count(handle_cpu, handle_irq);
        if flags.contains(EVENT_READ) && handle_ack.load(Ordering::SeqCst) != current {
            Ok(EventFlags::EVENT_READ)
        } else {
            Ok(EventFlags::empty())
        }
    }

    fn fsync(&self, _file: usize, _token: &mut CleanLockToken) -> Result<()> {
        Ok(())
    }

    fn close(&self, id: usize, token: &mut CleanLockToken) -> Result<()> {
        // Ferric Oxide: a move of this handle's vector finishes before it is given back.
        let _vectors = VECTORS.lock();
        let handle = HANDLES.write(token.token()).remove(id)?;

        if let Handle::Irq {
            reservation: Some(reservation),
            ..
        } = handle
        {
            set_reserved(reservation.cpu_id, reservation.index, false);
        }
        Ok(())
    }
    fn kwrite(
        &self,
        file: usize,
        buffer: UserSliceRo,
        _flags: u32,
        _stored_flags: u32,
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let handles_guard = HANDLES.read(token.token());
        let handle = handles_guard.get(file)?;

        match handle {
            &Handle::Irq {
                irq: handle_irq,
                ack: ref handle_ack,
                cpu: handle_cpu,
                ..
            } => {
                if buffer.len() < size_of::<usize>() {
                    return Err(Error::new(EINVAL));
                }
                let ack = buffer.read_usize()?;
                let current = interrupt_count(handle_cpu, handle_irq);

                if ack != current {
                    return Ok(0);
                }
                handle_ack.store(ack, Ordering::SeqCst);
                crate::trace::trace_event!(
                    ferric_trace::kernel::IRQ_ACK,
                    handle_irq,
                    handle_cpu.get(),
                    ack
                );
                // Ferric Oxide: only a legacy line has a PIC or I/O APIC pin to unmask. A number
                // below 16 reserved on an AP is an ordinary vector there, and acknowledging it
                // must not unmask the BSP's line of the same number.
                if !ap_low_vector(handle_cpu, handle_irq) {
                    unsafe {
                        acknowledge(handle_irq as usize);
                    }
                }
                Ok(size_of::<usize>())
            }
            _ => Err(Error::new(EBADF)),
        }
    }

    fn kfstat(&self, id: usize, buf: UserSliceWo, token: &mut CleanLockToken) -> Result<()> {
        let handles_guard = HANDLES.read(token.token());
        let handle = handles_guard.get(id)?;

        buf.copy_exactly(&match *handle {
            Handle::Irq {
                irq: handle_irq, ..
            } => Stat {
                st_mode: MODE_CHR | 0o600,
                st_size: size_of::<usize>() as u64,
                st_blocks: 1,
                st_blksize: size_of::<usize>() as u32,
                st_ino: handle_irq.into(),
                st_nlink: 1,
                ..Default::default()
            },
            Handle::Bsp => Stat {
                st_mode: MODE_CHR | 0o400,
                st_size: size_of::<usize>() as u64,
                st_blocks: 1,
                st_blksize: size_of::<usize>() as u32,
                st_ino: INO_BSP,
                st_nlink: 1,
                ..Default::default()
            },
            Handle::Avail(cpu_id) => Stat {
                st_mode: MODE_DIR | 0o700,
                st_size: 0,
                st_ino: INO_AVAIL | (u64::from(cpu_id.get()) << 32),
                st_nlink: 2,
                ..Default::default()
            },
            Handle::Phandle(phandle, ref buf) => Stat {
                st_mode: MODE_DIR | 0o700,
                st_size: buf.len() as u64,
                st_ino: INO_PHANDLE | (u64::from(phandle) << 32),
                st_nlink: 2,
                ..Default::default()
            },
            #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
            Handle::Cpus(ref text) => Stat {
                st_mode: MODE_CHR | 0o400,
                st_size: text.len() as u64,
                st_ino: INO_CPUS,
                st_nlink: 1,
                ..Default::default()
            },
            Handle::TopLevel => Stat {
                st_mode: MODE_DIR | 0o500,
                st_size: 0,
                st_ino: INO_TOPLEVEL,
                st_nlink: 1,
                ..Default::default()
            },
            _ => return Err(Error::new(EBADF)),
        })?;

        Ok(())
    }
    fn kfpath(&self, id: usize, buf: UserSliceWo, token: &mut CleanLockToken) -> Result<usize> {
        let handles_guard = HANDLES.read(token.token());
        let handle = handles_guard.get(id)?;

        let scheme_path = match handle {
            Handle::Irq { irq, .. } => alloc::format!("irq:{}", irq),
            Handle::Bsp => "irq:bsp".to_owned(),
            Handle::Avail(cpu_id) => alloc::format!("irq:cpu-{:2x}", cpu_id.get()),
            Handle::Phandle(phandle, _) => alloc::format!("irq:phandle-{}", phandle),
            Handle::TopLevel => "irq:".to_owned(),
            #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
            Handle::Cpus(_) => "irq:cpus".to_owned(),
            _ => return Err(Error::new(EBADF)),
        }
        .into_bytes();

        buf.copy_common_bytes_from_slice(&scheme_path)
    }
    fn kreadoff(
        &self,
        file: usize,
        buffer: UserSliceWo,
        _offset: u64,
        _flags: u32,
        _stored_flags: u32,
        token: &mut CleanLockToken,
    ) -> Result<usize> {
        let handles_guard = HANDLES.read(token.token());
        let handle = handles_guard.get(file)?;

        match *handle {
            // Ensures that the length of the buffer is larger than the size of a usize
            Handle::Irq {
                irq: handle_irq,
                ack: ref handle_ack,
                cpu: handle_cpu,
                ..
            } => {
                if buffer.len() < size_of::<usize>() {
                    return Err(Error::new(EINVAL));
                }
                let current = interrupt_count(handle_cpu, handle_irq);
                if handle_ack.load(Ordering::SeqCst) != current {
                    buffer.write_usize(current)?;
                    Ok(size_of::<usize>())
                } else {
                    Ok(0)
                }
            }
            Handle::Bsp => {
                if buffer.len() < size_of::<usize>() {
                    return Err(Error::new(EINVAL));
                }
                buffer.write_u32(LogicalCpuId::BSP.get())?;
                Ok(size_of::<usize>())
            }
            #[cfg(all(feature = "irq-percpu", target_arch = "x86_64"))]
            Handle::Cpus(ref text) => {
                let start = usize::try_from(_offset).map_or(text.len(), |o| o.min(text.len()));
                buffer.copy_common_bytes_from_slice(text.get(start..).unwrap_or_default())
            }
            Handle::Avail(_) | Handle::TopLevel | Handle::Phandle(_, _) | Handle::SchemeRoot => {
                Err(Error::new(EISDIR))
            }
        }
    }
}
