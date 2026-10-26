use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::Arc;

use common::MemoryType;
use driver_block::{Disk, PhysAddr, RingDiskScheme};
use pcid_interface::{irq_helpers, PciFunctionHandle};

use self::nvme::Nvme;
use crate::nvme::NvmeNamespace;

mod nvme;
// Ferric Oxide: one I/O queue pair and worker thread per CPU (drivers.nvme_percore).
#[cfg(feature = "percore")]
mod percore;
// Ferric Oxide: request-phase timing (drivers.nvmed_perf).
#[cfg(feature = "perf")]
mod perf;

#[derive(Clone)]
struct NvmeDisk {
    nvme: Arc<Nvme>,
    ns: NvmeNamespace,
    /// Ferric Oxide: the per-CPU workers that run reads and writes, when there are any.
    #[cfg(feature = "percore")]
    workers: Option<Rc<percore::Dispatcher>>,
}

/// Ferric Oxide: where one request runs (see [`NvmeDisk::claim`]).
#[cfg(feature = "percore")]
enum Claim<'a> {
    /// On the main thread's queue pair, holding it until the guard (if any) drops.
    Inline(Option<percore::InlineGuard<'a>>),
    /// On a worker, through the dispatcher.
    Worker(&'a Rc<percore::Dispatcher>),
}

#[cfg(feature = "percore")]
impl NvmeDisk {
    /// Where the next request runs: inline without per-CPU workers or when the queue policy
    /// keeps it on the main thread's queue pair, on a worker otherwise.
    async fn claim(&self) -> Claim<'_> {
        match &self.workers {
            None => Claim::Inline(None),
            Some(workers) => match workers.claim_inline().await {
                Some(guard) => Claim::Inline(Some(guard)),
                None => Claim::Worker(workers),
            },
        }
    }
}

impl Disk for NvmeDisk {
    #[cfg(feature = "percore")]
    fn set_queue_policy(&mut self, policy: ferric_blkqueue::QueuePolicy) -> syscall::Result<()> {
        let workers = self
            .workers
            .as_ref()
            .ok_or(syscall::Error::new(syscall::EOPNOTSUPP))?;
        workers.set_policy(policy);
        Ok(())
    }

    /// Async dispatch is enabled before the first Adaptive request arrives.
    /// The dispatcher keeps that first request on the scheme thread's queue.
    #[cfg(feature = "percore")]
    fn concurrent(&self) -> bool {
        self.workers.as_ref().is_some_and(|workers| {
            workers
                .policy()
                .serves_concurrently_with_overlap(cfg!(feature = "overlap"))
        })
    }

    /// With per-CPU workers, a concurrent read or write goes to a worker, which writes the
    /// scheme's response itself (see `percore::Dispatcher::submit`).
    #[cfg(feature = "percore")]
    fn submit(&mut self, io: driver_block::PendingIo) -> Result<(), driver_block::PendingIo> {
        match &self.workers {
            Some(workers) => workers.submit(&self.nvme, self.ns, io),
            None => Err(io),
        }
    }

    fn block_size(&self) -> u32 {
        self.ns.block_size.try_into().unwrap()
    }

    fn size(&self) -> u64 {
        self.ns.blocks * self.ns.block_size
    }

    async fn read(&mut self, block: u64, buffer: &mut [u8]) -> syscall::Result<usize> {
        #[cfg(feature = "percore")]
        let _inline = match self.claim().await {
            Claim::Inline(guard) => guard,
            Claim::Worker(workers) => {
                let op = percore::Op::Read {
                    buf: buffer.as_mut_ptr(),
                    len: buffer.len(),
                };
                // SAFETY: `buffer` is borrowed for as long as this future lives.
                return unsafe { workers.run(self.ns, block, op) }.await;
            }
        };
        self.nvme.namespace_read(&self.ns, block, buffer).await
    }

    async fn write(&mut self, block: u64, buffer: &[u8]) -> syscall::Result<usize> {
        #[cfg(feature = "percore")]
        let _inline = match self.claim().await {
            Claim::Inline(guard) => guard,
            Claim::Worker(workers) => {
                let op = percore::Op::Write {
                    buf: buffer.as_ptr(),
                    len: buffer.len(),
                };
                // SAFETY: as in `read`.
                return unsafe { workers.run(self.ns, block, op) }.await;
            }
        };
        self.nvme.namespace_write(&self.ns, block, buffer).await
    }

    async unsafe fn write_dma(
        &mut self,
        start_lba: u64,
        addr: PhysAddr,
        num_sectors: u32,
    ) -> syscall::Result<()> {
        #[cfg(feature = "percore")]
        let _inline = match self.claim().await {
            Claim::Inline(guard) => guard,
            Claim::Worker(workers) => {
                let op = percore::Op::WriteDma {
                    addr: addr.as_usize() as u64,
                    sectors: num_sectors as u64,
                };
                // SAFETY: a DMA job borrows no memory of this process.
                return unsafe { workers.run(self.ns, start_lba, op) }
                    .await
                    .map(drop);
            }
        };
        self.nvme
            .namespace_write_zerocopy(
                &self.ns,
                start_lba,
                addr.as_usize() as u64,
                num_sectors as u64,
            )
            .await
    }

    async unsafe fn read_dma(
        &mut self,
        start_lba: u64,
        addr: PhysAddr,
        num_sectors: u32,
    ) -> syscall::Result<()> {
        #[cfg(feature = "percore")]
        let _inline = match self.claim().await {
            Claim::Inline(guard) => guard,
            Claim::Worker(workers) => {
                let op = percore::Op::ReadDma {
                    addr: addr.as_usize() as u64,
                    sectors: num_sectors as u64,
                };
                // SAFETY: as in `write_dma`.
                return unsafe { workers.run(self.ns, start_lba, op) }
                    .await
                    .map(drop);
            }
        };
        self.nvme
            .namespace_read_zerocopy(
                &self.ns,
                start_lba,
                addr.as_usize() as u64,
                num_sectors as u64,
            )
            .await
    }
}

/// Ferric Oxide: maps applications' grants into the DMA domain pcid gave the driver.
#[cfg(feature = "grant-dma")]
struct PcidGrantDma;

#[cfg(feature = "grant-dma")]
impl PcidGrantDma {
    /// The errno a failed grant mapping call returns to the application.
    fn errno(err: pcid_interface::DmaGrantError) -> syscall::Error {
        match err {
            pcid_interface::DmaGrantError::Kernel(err) => err.into(),
            _ => syscall::Error::new(syscall::ENODEV),
        }
    }
}

#[cfg(feature = "grant-dma")]
impl driver_block::GrantDma for PcidGrantDma {
    fn map_grant(&self, grant: &libredox::Fd) -> syscall::Result<u64> {
        pcid_interface::map_dma_grant(grant).map_err(Self::errno)
    }

    fn unmap_grant(&self, iova: u64) -> syscall::Result<()> {
        pcid_interface::unmap_dma_grant(iova).map_err(Self::errno)
    }
}

fn time_arm(time_handle: &mut File, secs: i64) -> io::Result<()> {
    let mut time_buf = syscall::TimeSpec::default();
    if time_handle.read(&mut time_buf)? < time_buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "time read too small",
        ));
    }
    time_buf.tv_sec += secs;
    time_handle.write(&time_buf)?;
    Ok(())
}

fn main() {
    pcid_interface::pci_daemon(daemon);
}

/// Ferric Oxide: the admin queue's interrupt and, with MSI-X, the table the per-CPU queue
/// pairs take their entries from (entry 0 is the admin queue's, on the BSP, as upstream).
#[cfg(feature = "percore")]
fn percore_interrupts(
    pcid_handle: &mut PciFunctionHandle,
) -> (irq_helpers::InterruptVector, Option<irq_helpers::Msix>) {
    let has_msix = pcid_handle
        .fetch_all_features()
        .iter()
        .any(|feature| feature.is_msix());
    if has_msix {
        let mut msix = irq_helpers::Msix::enable(pcid_handle, "nvmed");
        (msix.alloc(), Some(msix))
    } else {
        (
            irq_helpers::pci_allocate_interrupt_vector(pcid_handle, "nvmed"),
            None,
        )
    }
}

fn daemon(daemon: daemon::Daemon, mut pcid_handle: PciFunctionHandle) -> ! {
    let pci_config = pcid_handle.config();

    let scheme_name = format!("disk.{}-nvme", pci_config.func.name());

    common::setup_logging(
        "disk",
        "pci",
        &scheme_name,
        common::output_level(),
        common::file_level(),
    );

    unsafe {
        if libc::setpriority(libc::PRIO_PROCESS, 0, -10) == -1 {
            log::error!("nvmed: Failed to set nice value");
        } else {
            log::info!("nvmed: Successfully set process priority to 10");
        }
    }

    log::debug!("NVME PCI CONFIG: {:?}", pci_config);

    let address = unsafe { pcid_handle.map_bar(0, MemoryType::Uncacheable).ptr };

    // Ferric Oxide: with per-CPU queues, keep the MSI-X table for their entries.
    #[cfg(feature = "percore")]
    let (interrupt_vector, mut msix) = percore_interrupts(&mut pcid_handle);
    #[cfg(not(feature = "percore"))]
    let interrupt_vector = irq_helpers::pci_allocate_interrupt_vector(&mut pcid_handle, "nvmed");
    let iv = interrupt_vector.vector();
    let irq_handle = interrupt_vector.irq_handle().try_clone().unwrap();

    let mut nvme = Nvme::new(address.as_ptr() as usize, interrupt_vector, pcid_handle)
        .expect("nvmed: failed to allocate driver data");

    unsafe { nvme.init().expect("nvmed: failed to init") }
    log::debug!("Finished base initialization");
    let nvme = Arc::new(nvme);

    let executor = nvme::executor::init(Arc::clone(&nvme), iv, false /* FIXME */, irq_handle);

    let mut time_handle = File::open(&format!("/scheme/time/{}", libredox::flag::CLOCK_MONOTONIC))
        .expect("failed to open time handle");

    let mut time_events = Box::pin(
        executor.register_external_event(time_handle.as_raw_fd() as usize, event::EventFlags::READ),
    );

    // Ferric Oxide: the controller is asked for the per-CPU queue pairs before the first I/O
    // queue exists, which is the only time it may be asked.
    #[cfg(feature = "percore")]
    let grant = msix
        .as_ref()
        .and_then(|msix| percore::request(&nvme, msix, &executor));

    // Try to init namespaces for 5 seconds
    time_arm(&mut time_handle, 5).expect("failed to arm timer");
    let namespaces = executor.block_on(async {
        let namespaces_future = nvme.init_with_queues();
        let time_future = time_events.as_mut().next();
        futures::pin_mut!(namespaces_future);
        futures::pin_mut!(time_future);
        match futures::future::select(namespaces_future, time_future).await {
            futures::future::Either::Left((namespaces, _)) => namespaces,
            futures::future::Either::Right(_) => panic!("timeout on init"),
        }
    });
    log::debug!("Initialized!");

    // Ferric Oxide: per-CPU queue pairs and their workers, and the task that completes their
    // jobs, which must run before the partition tables are read.
    #[cfg(feature = "percore")]
    let workers = msix
        .as_mut()
        .zip(grant)
        .and_then(|(msix, grant)| percore::start(&nvme, msix, &executor, grant));
    #[cfg(feature = "percore")]
    if let Some(workers) = &workers {
        executor.spawn(Rc::clone(workers).pump(Rc::clone(&executor)));
        executor.spawn(Rc::clone(workers).watch(Rc::clone(&executor)));
    }

    let scheme = Rc::new(RefCell::new(RingDiskScheme::new(
        Some(daemon),
        scheme_name,
        namespaces
            .into_iter()
            .map(|(k, ns)| {
                (
                    k,
                    NvmeDisk {
                        nvme: nvme.clone(),
                        ns,
                        #[cfg(feature = "percore")]
                        workers: workers.clone(),
                    },
                )
            })
            .collect(),
        executor.clone(),
    )));
    // Ferric Oxide: applications may attach registered-buffer grants to their disk
    // descriptors, which the controller then DMAs into directly.
    #[cfg(feature = "grant-dma")]
    scheme.borrow_mut().set_grant_dma(Rc::new(PcidGrantDma));

    let mut scheme_events = Box::pin(executor.register_external_event(
        scheme.borrow().event_handle().raw(),
        event::EventFlags::READ,
    ));

    // TODO: Drivers-block will open time fd.
    // libredox::call::setrens(0, 0).expect("nvmed: failed to enter null namespace");

    log::debug!("Starting to listen for scheme events");

    #[cfg(feature = "perf")]
    if let Err(error) = perf::start_reporting() {
        log::error!("cannot start NVMe phase reporter: {error}");
        std::process::exit(1);
    }

    // Ferric Oxide: a close held back for concurrent requests becomes ready when the last of
    // them ends, which is no scheme event, so the loop also wakes for that.
    let closes = scheme.borrow().close_gate();
    executor.block_on(async {
        loop {
            log::trace!("new event iteration");
            if let Err(err) = scheme.borrow_mut().tick().await {
                log::error!("scheme error: {err}");
            }
            let event = std::pin::pin!(scheme_events.as_mut().next());
            futures::future::select(event, closes.ready()).await;
        }
    });

    //TODO: destroy NVMe stuff

    std::process::exit(0);
}
