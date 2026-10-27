//! IRQ helpers.
//!
//! This module allows easy handling of the `/scheme/irq` scheme, and allocating interrupt vectors
//! for use by INTx#, MSI, or MSI-X.

use std::convert::TryFrom;
use std::fs::{self, File};
use std::io::prelude::*;
use std::io::{self};
use std::num::NonZeroU8;

use ferric_vtd::interrupt::MsiFormat;

use crate::msi::{MsiAddrAndData, MsixTableEntry};
// Ferric Oxide: vectors on a chosen CPU.
use crate::percpu::{self, CpuEntry, CpuId, VectorError};
use crate::PciFunctionHandle;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use crate::{
    msi::MappedMsixRegs, MsiSetFeatureInfo, PciFeature, PciFeatureInfo, PcidChannel, SetFeatureInfo,
};

/// Read the local APIC ID of the bootstrap processor.
///
/// Ferric Oxide: despite the name, `irq:bsp` holds the BSP's logical CPU id, the number the
/// other helpers here take; see [`crate::percpu::bsp_cpu`].
pub fn read_bsp_apic_id() -> io::Result<usize> {
    let mut buffer = [0u8; 8];

    let mut file = File::open("/scheme/irq/bsp")?;
    let bytes_read = file.read(&mut buffer)?;

    (if bytes_read == 8 {
        usize::try_from(u64::from_le_bytes(buffer))
    } else if bytes_read == 4 {
        usize::try_from(u32::from_le_bytes([
            buffer[0], buffer[1], buffer[2], buffer[3],
        ]))
    } else {
        panic!(
            "`/scheme/irq` scheme responded with {} bytes, expected {}",
            bytes_read,
            size_of::<usize>()
        );
    })
    .or(Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "bad BSP int size",
    )))
}

// TODO: Perhaps read the MADT instead?
/// Obtains an interator over all of the visible CPU ids, for use in IRQ allocation and MSI
/// capability structs or MSI-X tables.
pub fn cpu_ids() -> io::Result<impl Iterator<Item = io::Result<usize>> + 'static> {
    Ok(
        fs::read_dir("/scheme/irq")?.filter_map(|entry| -> Option<io::Result<_>> {
            match entry {
                Ok(e) => {
                    let path = e.path();
                    let file_name = path.file_name()?.to_str()?;
                    // the file name should be in the format `cpu-<CPU ID>`
                    if !file_name.starts_with("cpu-") {
                        return None;
                    }
                    u8::from_str_radix(&file_name[4..], 16)
                        .map(usize::from)
                        .map(Ok)
                        .ok()
                }
                Err(e) => Some(Err(e)),
            }
        }),
    )
}

/// Allocate multiple interrupt vectors, from the IDT of the specified processor, returning the
/// start vector and the IRQ handles.
///
/// The alignment is a requirement for the allocation range. For example, with an alignment of 8,
/// only ranges that begin with a multiple of eight are accepted. The IRQ handles returned will
/// always correspond to the subsequent IRQ numbers beginning the first value in the return tuple.
///
/// This function is not actually guaranteed to allocate all of the IRQs specified in `count`,
/// since another process might already have requested one vector in the range. The caller must
/// check that the returned vector have the same length as `count`. In the future this function may
/// perhaps lock the entire directory to prevent this from happening, or maybe find the smallest free
/// range with the minimum alignment, to allow other drivers to obtain their necessary IRQs.
///
/// Note that this count/alignment restriction is only mandatory for MSI; MSI-X allows for
/// individually allocated vectors that might be spread out, even on multiple CPUs. Thus, multiple
/// invocations with alignment 1 and count 1 are totally acceptable, although allocating in bulk
/// minimizes the initialization overhead.
pub fn allocate_aligned_interrupt_vectors(
    cpu_id: usize,
    alignment: NonZeroU8,
    count: u8,
) -> io::Result<Option<(u8, Vec<File>)>> {
    let cpu_id = u8::try_from(cpu_id).expect("usize cpu ids not implemented yet");
    if count == 0 {
        return Ok(None);
    }

    let available_irqs = fs::read_dir(format!("/scheme/irq/cpu-{:02x}", cpu_id))?;
    let mut available_irq_numbers = available_irqs.filter_map(|entry| -> Option<io::Result<_>> {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => return Some(Err(err)),
        };

        let path = entry.path();

        let file_name = match path.file_name() {
            Some(f) => f,
            None => return None,
        };

        let path_str = match file_name.to_str() {
            Some(s) => s,
            None => return None,
        };

        match path_str.parse::<u8>() {
            Ok(p) => Some(Ok(p)),
            Err(_) => None,
        }
    });

    // TODO: fcntl F_SETLK on `/scheme/irq/`?

    let mut handles = Vec::with_capacity(usize::from(count));

    let mut index = 0;
    let mut first = None;

    while let Some(number) = available_irq_numbers.next() {
        let number = number?;

        // Skip until a suitable alignment is found.
        if number % u8::from(alignment) != 0 {
            continue;
        }
        let first = *first.get_or_insert(number);
        let irq_number = first + index;

        // From the point where the range is aligned, we can start to advance until `count` IRQs
        // have been allocated.
        if index >= count {
            break;
        }

        // if found, reserve the irq
        let irq_handle =
            match File::create(format!("/scheme/irq/cpu-{:02x}/{}", cpu_id, irq_number)) {
                Ok(handle) => handle,

                // return early if the entire range couldn't be allocated
                Err(err) if err.kind() == io::ErrorKind::NotFound => break,

                Err(err) => return Err(err),
            };
        handles.push(irq_handle);
        index += 1;
    }
    if handles.is_empty() {
        return Ok(None);
    }
    let first = match first {
        Some(f) => f,
        None => return Ok(None),
    };

    Ok(Some((first + 32, handles)))
}

/// Allocate at most `count` interrupt vectors, which can start at any offset. Unless MSI is used
/// and an entire aligned range of vectors is needed, this function should be used.
pub fn allocate_interrupt_vectors(cpu_id: usize, count: u8) -> io::Result<Option<(u8, Vec<File>)>> {
    allocate_aligned_interrupt_vectors(cpu_id, NonZeroU8::new(1).unwrap(), count)
}

/// Allocate a single interrupt vector, returning both the vector number (starting from 32 up to
/// 254), and its IRQ handle which is then reserved. Returns Ok(None) if allocation fails due to
/// no available IRQs.
pub fn allocate_single_interrupt_vector(cpu_id: usize) -> io::Result<Option<(u8, File)>> {
    let (base, mut files) = match allocate_interrupt_vectors(cpu_id, 1) {
        Ok(Some((base, files))) => (base, files),
        Ok(None) => return Ok(None),
        Err(err) => return Err(err),
    };
    assert_eq!(files.len(), 1);
    Ok(Some((base, files.pop().unwrap())))
}

/// Allocates a vector on logical CPU `cpu_id` and returns the MSI message that delivers it.
///
/// Ferric Oxide: the message comes from pcid over `channel` (an interrupt remapping entry when
/// the IOMMU remaps interrupts) instead of being computed here.
///
/// # Panics
///
/// When no vector is free on that CPU, or pcid has no message for it.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn allocate_single_interrupt_vector_for_msi(
    channel: &PcidChannel,
    cpu_id: usize,
) -> (MsiAddrAndData, File) {
    let cpu = u32::try_from(cpu_id).expect("CPU number above 32 bits");
    percpu::allocate_msi(channel, CpuId::new(cpu))
        .unwrap_or_else(|err| panic!("{}", percpu::report(&err)))
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn allocate_first_msi_interrupt_on_bsp(pcid_handle: &mut PciFunctionHandle) -> File {
    // TODO: Allow allocation of up to 32 vectors.

    let destination_id = read_bsp_apic_id().expect("failed to read BSP apic id");
    let (msg_addr_and_data, interrupt_handle) =
        allocate_single_interrupt_vector_for_msi(&pcid_handle.channel(), destination_id);

    let set_feature_info = MsiSetFeatureInfo {
        multi_message_enable: Some(0),
        message_address_and_data: Some(msg_addr_and_data),
        mask_bits: None,
    };
    pcid_handle.set_feature_info(SetFeatureInfo::Msi(set_feature_info));

    pcid_handle.enable_feature(PciFeature::Msi);
    log::debug!("Enabled MSI");

    interrupt_handle
}

#[derive(Debug)]
pub struct InterruptVector {
    irq_handle: File,
    vector: u16,
    kind: InterruptVectorKind,
    // Ferric Oxide: the logical CPU the vector is on.
    cpu: CpuId,
}

/// Sequential MSI-X table allocator. The first `alloc` takes table entry 0,
/// later calls take 1, 2, etc. until the table is exhausted, then they share
/// entry 0. Drivers hold one `Msix` for the PCI function and call `alloc`
/// once per virtqueue (or config vector).
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub struct Msix {
    mapped: MappedMsixRegs,
    next: u16,
    bsp: usize,
    // Ferric Oxide: each entry's handle and CPU.
    handles: Vec<(File, CpuId)>,
    // Ferric Oxide: for asking pcid for each entry's message.
    channel: PcidChannel,
    // Ferric Oxide: the kernel's CPUs, `None` without per-CPU delivery (`kernel.irq_percpu`).
    cpus: Option<Vec<CpuEntry>>,
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
impl Msix {
    pub fn enable(pcid_handle: &mut PciFunctionHandle, driver: &str) -> Self {
        let msix_info = match pcid_handle.feature_info(PciFeature::MsiX) {
            PciFeatureInfo::MsiX(msix) => msix,
            _ => unreachable!("{driver}: MSI-X advertised but capability missing"),
        };
        let mapped = unsafe { msix_info.map_and_mask_all(pcid_handle) };
        assert!(mapped.info.table_size > 0, "{driver}: MSI-X table is empty");
        pcid_handle.enable_feature(PciFeature::MsiX);

        let bsp = read_bsp_apic_id()
            .unwrap_or_else(|err| panic!("{driver}: failed to read BSP APIC ID: {err}"));

        log::debug!(
            "{driver}: MSI-X enabled (table_size={})",
            mapped.info.table_size
        );

        let cpus = percpu::online_cpus()
            .unwrap_or_else(|err| {
                log::warn!(
                    "{driver}: MSI-X vectors stay on the BSP: {}",
                    percpu::report(&err)
                );
                None
            })
            // An empty map is no map: `alloc_on` would otherwise refuse even the BSP.
            .filter(|cpus| !cpus.is_empty());

        Self {
            mapped,
            next: 0,
            bsp,
            handles: Vec::new(),
            channel: pcid_handle.channel(),
            cpus,
        }
    }

    pub fn alloc(&mut self) -> InterruptVector {
        if self.next < self.mapped.info.table_size {
            let bsp = CpuId::new(u32::try_from(self.bsp).expect("BSP id above 32 bits"));
            self.program_next(bsp)
                .unwrap_or_else(|err| panic!("{}", percpu::report(&err)))
        } else {
            let (handle, cpu) = &self.handles[0];
            let irq_handle = handle
                .try_clone()
                .expect("failed to clone MSI-X IRQ handle");
            let entry = self.mapped.table_entry_pointer(0);
            InterruptVector {
                irq_handle,
                vector: 0,
                kind: InterruptVectorKind::MsiX { table_entry: entry },
                cpu: *cpu,
            }
        }
    }

    /// Ferric Oxide: the number of entries in the MSI-X table.
    pub fn table_size(&self) -> u16 {
        self.mapped.info.table_size
    }

    /// Ferric Oxide: whether the kernel delivers interrupts per CPU (`kernel.irq_percpu`), so
    /// [`Self::alloc_on`] can put a vector on a CPU other than the BSP.
    pub fn per_cpu(&self) -> bool {
        self.cpus.is_some()
    }

    /// Ferric Oxide: the kernel's online CPUs, `None` without per-CPU delivery.
    pub fn cpus(&self) -> Option<&[CpuEntry]> {
        self.cpus.as_deref()
    }

    /// Ferric Oxide: programs the next MSI-X table entry with a vector on logical CPU `cpu`, or
    /// on the BSP when the kernel has no per-CPU delivery (check [`InterruptVector::cpu`]).
    ///
    /// Unlike [`Self::alloc`], it does not share entry 0 once the table is full.
    ///
    /// # Errors
    ///
    /// [`VectorError::TableFull`] when every entry has a vector, [`VectorError::UnknownCpu`] when
    /// `cpu` is not online, and the errors of reserving the vector and getting its message.
    pub fn alloc_on(&mut self, cpu: CpuId) -> Result<InterruptVector, VectorError> {
        let size = self.table_size();
        if self.next >= size {
            return Err(VectorError::TableFull { size });
        }
        let target = match &self.cpus {
            Some(cpus) if cpus.iter().any(|entry| entry.cpu == cpu) => cpu,
            Some(_) => return Err(VectorError::UnknownCpu { cpu }),
            None => CpuId::new(u32::try_from(self.bsp).expect("BSP id above 32 bits")),
        };
        self.program_next(target)
    }

    /// Ferric Oxide: programs up to `count` MSI-X table entries (no more than the table has
    /// free), one per CPU before any CPU gets a second, CPUs on NUMA node `node` first
    /// ([`ferric_irq::spread`]). Without per-CPU delivery every vector is on the BSP.
    ///
    /// Entry `i` of the table goes to the `i`-th CPU of that order, so separate calls carry on
    /// the rotation where the entries programmed so far (by any call) left it.
    ///
    /// # Errors
    ///
    /// The first error of [`Self::alloc_on`]. The entries this call programmed before it are
    /// masked and given back (their vectors are released and the next call reuses them), so a
    /// failed call changes nothing.
    pub fn alloc_spread(
        &mut self,
        count: u16,
        node: Option<u32>,
    ) -> Result<Vec<InterruptVector>, VectorError> {
        let first = self.next;
        let free = self.table_size().saturating_sub(first);
        let count = usize::from(count.min(free));
        // Without per-CPU delivery every target is the BSP.
        let targets: Vec<CpuId> =
            ferric_irq::spread_from(self.cpus.as_deref(), node, usize::from(first), count)
                .collect();
        let mut vectors = Vec::with_capacity(count);
        for cpu in targets {
            match self.alloc_on(cpu) {
                Ok(vector) => vectors.push(vector),
                Err(err) => {
                    drop(vectors);
                    self.release_from(first);
                    return Err(err);
                }
            }
        }
        Ok(vectors)
    }

    /// Ferric Oxide: steers MSI-X table entry `index` to logical CPU `cpu` (root only), keeping
    /// its `irq:` handles: the kernel moves the vector and re-keys every handle for it.
    ///
    /// The entry's own message decides who moves it. A remappable-format message names an
    /// interrupt remapping entry, which the kernel rewrites. A compatibility-format message
    /// names the CPU and vector itself, so the entry is masked before the kernel moves the
    /// vector (and gives the old one back), given pcid's message for the new vector, and then
    /// left masked or unmasked as it was; an interrupt raised while it is masked stays pending
    /// in the device and is sent the new way. The [`InterruptVector::cpu`] that
    /// [`Self::alloc_on`] returned for the entry is not updated; this call returns the new
    /// CPU's result instead.
    ///
    /// # Errors
    ///
    /// [`VectorError::NoEntry`] when `index` has no vector, [`VectorError::UnknownCpu`] when
    /// `cpu` is not online or the kernel has no per-CPU delivery, [`VectorError::Move`] when
    /// the kernel refuses (the entry is as it was), [`VectorError::NoMessage`] when pcid has no
    /// message for the new vector (the entry then stays masked), and
    /// [`VectorError::NotRemapped`] when the entry is remapped but the kernel found no
    /// remapping entry to move.
    pub fn retarget(
        &mut self,
        index: u16,
        cpu: CpuId,
    ) -> Result<ferric_irq::call::Moved, VectorError> {
        if !self
            .cpus()
            .is_some_and(|cpus| cpus.iter().any(|entry| entry.cpu == cpu))
        {
            return Err(VectorError::UnknownCpu { cpu });
        }
        if usize::from(index) >= self.handles.len() {
            return Err(VectorError::NoEntry { index });
        }
        let entry = self.mapped.table_entry_pointer(usize::from(index));
        let remapped = matches!(MsiFormat::of(entry.addr()), MsiFormat::Remappable { .. });
        let was_masked = entry.vec_ctl() & MsixTableEntry::VEC_CTL_MASK_BIT != 0;
        if !remapped {
            // Masked before the kernel gives the old vector back, so nothing reaches it after.
            entry.mask();
        }
        let (handle, held) = &mut self.handles[usize::from(index)];
        let moved = match percpu::move_vector(handle, cpu) {
            Ok(moved) => moved,
            Err(err) => {
                // Nothing moved, so the old message is still right.
                self.mapped
                    .table_entry_pointer(usize::from(index))
                    .set_masked(was_masked);
                return Err(err);
            }
        };
        *held = cpu;
        if remapped {
            return if moved.remapped == 0 {
                Err(VectorError::NotRemapped { index })
            } else {
                Ok(moved)
            };
        }
        let vector = moved.irq.get() + 32;
        let message = self
            .channel
            .msi_message(cpu.get() as usize, vector)
            .ok_or(VectorError::NoMessage { cpu, vector })?;
        let entry = self.mapped.table_entry_pointer(usize::from(index));
        entry.write_addr_and_data(message);
        entry.set_masked(was_masked);
        Ok(moved)
    }

    /// Ferric Oxide: masks the table entries from `first` on and gives them back, closing the
    /// handles this `Msix` keeps for them, so the next allocation programs `first` again.
    fn release_from(&mut self, first: u16) {
        for index in first..self.next {
            self.mapped.table_entry_pointer(usize::from(index)).mask();
        }
        self.handles.truncate(usize::from(first));
        self.next = first;
    }

    /// Ferric Oxide: programs the next table entry with a vector on `cpu`.
    fn program_next(&mut self, cpu: CpuId) -> Result<InterruptVector, VectorError> {
        let index = self.next;
        let (msg_addr_and_data, irq_handle) = percpu::allocate_msi(&self.channel, cpu)?;
        // Before the entry is touched, so a failure leaves nothing to undo.
        let kept = irq_handle
            .try_clone()
            .map_err(|source| VectorError::Clone { cpu, source })?;
        self.next += 1;
        let entry = self.mapped.table_entry_pointer(index as usize);
        entry.write_addr_and_data(msg_addr_and_data);
        entry.unmask();

        self.handles.push((kept, cpu));

        Ok(InterruptVector {
            irq_handle,
            vector: index,
            kind: InterruptVectorKind::MsiX { table_entry: entry },
            cpu,
        })
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub struct Msix;

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
impl Msix {
    pub fn enable(_pcid_handle: &mut PciFunctionHandle, driver: &str) -> Self {
        unimplemented!("{driver}: MSI-X is not implemented on this architecture")
    }

    pub fn alloc(&mut self) -> InterruptVector {
        unimplemented!("MSI-X is not implemented on this architecture")
    }

    /// Ferric Oxide: see the x86 [`Msix::alloc_on`].
    pub fn alloc_on(&mut self, _cpu: CpuId) -> Result<InterruptVector, VectorError> {
        unimplemented!("MSI-X is not implemented on this architecture")
    }

    /// Ferric Oxide: see the x86 [`Msix::alloc_spread`].
    pub fn alloc_spread(
        &mut self,
        _count: u16,
        _node: Option<u32>,
    ) -> Result<Vec<InterruptVector>, VectorError> {
        unimplemented!("MSI-X is not implemented on this architecture")
    }

    /// Ferric Oxide: see the x86 [`Msix::retarget`].
    pub fn retarget(
        &mut self,
        _index: u16,
        cpu: CpuId,
    ) -> Result<ferric_irq::call::Moved, VectorError> {
        Err(VectorError::UnknownCpu { cpu })
    }
}

#[derive(Debug)]
enum InterruptVectorKind {
    Legacy,
    Msi,
    MsiX { table_entry: *mut MsixTableEntry },
}

/// Ferric Oxide: how an [`InterruptVector`] is delivered, for drivers whose device registers
/// depend on it (an NVMe controller's INTMS and INTMC serve pin-based and MSI interrupts only).
///
/// Deliberately exhaustive: a driver that matches on it must decide what a new mechanism means
/// for its device rather than fall into a wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptMechanism {
    /// The INTx# pin, through the I/O APIC.
    Pin,
    /// An MSI message.
    Msi,
    /// An MSI-X table entry, which the driver masks through its Vector Control word.
    MsiX,
}

impl InterruptVector {
    /// Ferric Oxide: how this vector is delivered.
    pub fn mechanism(&self) -> InterruptMechanism {
        match self.kind {
            InterruptVectorKind::Legacy => InterruptMechanism::Pin,
            InterruptVectorKind::Msi => InterruptMechanism::Msi,
            InterruptVectorKind::MsiX { .. } => InterruptMechanism::MsiX,
        }
    }

    pub fn irq_handle(&self) -> &File {
        &self.irq_handle
    }

    pub fn vector(&self) -> u16 {
        self.vector
    }

    /// Ferric Oxide: the logical CPU the interrupt is delivered to.
    pub fn cpu(&self) -> CpuId {
        self.cpu
    }

    pub fn set_masked_if_fast(&mut self, masked: bool) -> bool {
        match self.kind {
            InterruptVectorKind::Legacy | InterruptVectorKind::Msi => false,
            InterruptVectorKind::MsiX { table_entry } => {
                unsafe { (*table_entry).set_masked(masked) };
                true
            }
        }
    }
}

/// Get the most optimal supported interrupt mechanism: either (in the order of preference):
/// MSI-X, MSI, and INTx# pin.
///
/// For more than one MSI-X vector, use [`Msix::enable`] and call [`Msix::alloc`] per vector.
// FIXME move MSI-X IRQ allocation to pcid
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn pci_allocate_interrupt_vector(
    pcid_handle: &mut PciFunctionHandle,
    driver: &str,
) -> InterruptVector {
    let features = pcid_handle.fetch_all_features();

    let has_msi = features.iter().any(|feature| feature.is_msi());
    let has_msix = features.iter().any(|feature| feature.is_msix());

    if has_msix {
        Msix::enable(pcid_handle, driver).alloc()
    } else if has_msi {
        InterruptVector {
            irq_handle: allocate_first_msi_interrupt_on_bsp(pcid_handle),
            vector: 0,
            kind: InterruptVectorKind::Msi,
            cpu: CpuId::BSP,
        }
    } else if let Some(irq) = pcid_handle.config().func.legacy_interrupt_line {
        // INTx# pin based interrupts.
        InterruptVector {
            irq_handle: irq.irq_handle(driver),
            vector: 0,
            kind: InterruptVectorKind::Legacy,
            cpu: CpuId::BSP,
        }
    } else {
        panic!("{driver}: no interrupts supported at all")
    }
}

// FIXME support MSI on non-x86 systems
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn pci_allocate_interrupt_vector(
    pcid_handle: &mut PciFunctionHandle,
    driver: &str,
) -> InterruptVector {
    if let Some(irq) = pcid_handle.config().func.legacy_interrupt_line {
        // INTx# pin based interrupts.
        InterruptVector {
            irq_handle: irq.irq_handle(driver),
            vector: 0,
            kind: InterruptVectorKind::Legacy,
            cpu: CpuId::BSP,
        }
    } else {
        panic!("{driver}: no interrupts supported at all")
    }
}
