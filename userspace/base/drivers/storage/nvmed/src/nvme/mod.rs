use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::iter;
use std::sync::atomic::AtomicU16;
use std::sync::Arc;

use common::dma::Dma;
use common::io::{Io, Mmio};
use common::timeout::Timeout;
use parking_lot::{Mutex, ReentrantMutex, RwLock};
use pcid_interface::irq_helpers::InterruptVector;
use pcid_interface::PciFunctionHandle;
use syscall::error::{Error, Result, EIO};

pub mod cmd;
pub mod executor;
pub mod identify;
// Ferric Oxide: one I/O queue pair per CPU (features.toml: drivers.nvme_percore).
#[cfg(feature = "percore")]
pub mod percore;
pub mod queues;

use self::executor::NvmeExecutor;
pub use self::queues::{NvmeCmd, NvmeCmdQueue, NvmeComp, NvmeCompQueue};

#[repr(C, packed)]
pub struct NvmeRegs {
    /// Controller Capabilities
    cap_low: Mmio<u32>,
    cap_high: Mmio<u32>,
    /// Version
    vs: Mmio<u32>,
    /// Interrupt mask set
    intms: Mmio<u32>,
    /// Interrupt mask clear
    intmc: Mmio<u32>,
    /// Controller configuration
    cc: Mmio<u32>,
    /// Reserved
    _rsvd: Mmio<u32>,
    /// Controller status
    csts: Mmio<u32>,
    /// NVM subsystem reset
    nssr: Mmio<u32>,
    /// Admin queue attributes
    aqa: Mmio<u32>,
    /// Admin submission queue base address
    asq_low: Mmio<u32>,
    asq_high: Mmio<u32>,
    /// Admin completion queue base address
    acq_low: Mmio<u32>,
    acq_high: Mmio<u32>,
    /// Controller memory buffer location
    cmbloc: Mmio<u32>,
    /// Controller memory buffer size
    cmbsz: Mmio<u32>,
}

#[derive(Copy, Clone, Debug)]
pub struct NvmeNamespace {
    pub id: u32,
    pub blocks: u64,
    pub block_size: u64,
}

pub type CqId = u16;
pub type SqId = u16;
pub type CmdId = u16;
pub type AtomicCqId = AtomicU16;
pub type AtomicSqId = AtomicU16;
pub type AtomicCmdId = AtomicU16;
pub type Iv = u16;

/// The bytes one zero-copy command carries without a PRP list (two pages), and the default
/// before the controller reports its MDTS.
const ZEROCOPY_CHUNK: u64 = 8192;

struct Caps(u64);

impl Caps {
    fn dstrd(&self) -> u8 {
        ((self.0 >> 32) & 0b1111) as u8
    }

    /// Ferric Oxide: the minimum memory page size (`CAP.MPSMIN`), in bytes.
    #[cfg(feature = "grant-dma")]
    fn min_page_size(&self) -> u64 {
        1 << (12 + ((self.0 >> 48) & 0b1111))
    }
}

pub struct Nvme {
    interrupt_vector: Mutex<InterruptVector>,
    pcid_interface: Mutex<PciFunctionHandle>,
    regs: RwLock<&'static mut NvmeRegs>,

    sq_ivs: RwLock<HashMap<SqId, Iv>>,
    cq_ivs: RwLock<HashMap<CqId, Iv>>,

    // maps interrupt vectors with the completion queues they have
    thread_ctxts: RwLock<HashMap<Iv, Arc<ReentrantMutex<ThreadCtxt>>>>,

    next_sqid: AtomicSqId,
    next_cqid: AtomicCqId,

    caps: Caps,

    /// Ferric Oxide: spare PRP list pages for zero-copy transfers of more than two pages.
    #[cfg(feature = "grant-dma")]
    prp_lists: Mutex<Vec<Dma<[u64; ferric_blkgrant::prp::LIST_ENTRIES]>>>,
    /// Ferric Oxide: the most bytes one zero-copy command carries (MDTS, capped by one PRP
    /// list).
    #[cfg(feature = "grant-dma")]
    max_transfer: std::sync::atomic::AtomicU64,

    /// Ferric Oxide: the MSI-X vectors of the per-CPU queue pairs, by table entry, for masking.
    #[cfg(feature = "percore")]
    percore_vectors: Mutex<HashMap<Iv, InterruptVector>>,
}

thread_local! {
    /// Ferric Oxide: the key in `thread_ctxts` of this thread's context (its queue pair's
    /// vector); 0, the admin and first I/O queue, unless a per-CPU worker set it.
    static THREAD_CTXT: std::cell::Cell<Iv> = const { std::cell::Cell::new(0) };
}

pub struct ThreadCtxt {
    buffer: RefCell<Dma<[u8; 512 * 4096]>>, // 2MB of buffer
    buffer_prp: RefCell<Dma<[u64; 512]>>,   // 4KB of PRP for the buffer

    // Yes, technically NVME allows multiple submission queues to be mapped to the same completion
    // queue, but we don't use that feature.
    queues: RefCell<HashMap<u16, (NvmeCmdQueue, NvmeCompQueue)>>,

    /// Ferric Oxide: the I/O submission queue this thread's reads and writes go to.
    io_queue: SqId,
}

impl ThreadCtxt {
    pub fn push_sqe(
        &self,
        sq_id: SqId,
        cmd_init: impl FnOnce(CmdId) -> NvmeCmd,
        fail: impl FnOnce(),
    ) -> Option<(CqId, CmdId)> {
        match self.queues.borrow_mut().get_mut(&sq_id).unwrap() {
            (sq, _cq) => {
                if sq.is_full() {
                    fail();
                    return None;
                }
                let cmd_id = sq.tail;
                let _tail = sq.submit_unchecked(cmd_init(cmd_id));

                Some((sq_id, cmd_id))
            }
        }
    }

    pub fn submit(&self, sq_id: SqId) {
        match self.queues.borrow_mut().get_mut(&sq_id).unwrap() {
            (sq, _cq) => sq.kick(),
        }
    }
}

unsafe impl Send for Nvme {}
unsafe impl Sync for Nvme {}

/// How to handle full submission queues.
pub enum FullSqHandling {
    /// Return an error immediately prior to posting the command.
    ErrorDirectly,

    /// Tell the executor that we want to be notified when a command on the same submission queue
    /// has been completed.
    Wait,
}

impl Nvme {
    pub fn new(
        address: usize,
        interrupt_vector: InterruptVector,
        pcid_interface: PciFunctionHandle,
    ) -> Result<Self> {
        let regs = unsafe { &mut *(address as *mut NvmeRegs) };
        log::debug!("CAP_LOW: {:X}", regs.cap_low.read());
        log::debug!("CAP_HIGH: {:X}", regs.cap_high.read());
        log::debug!("VS: {:X}", regs.vs.read());
        log::debug!("CC: {:X}", regs.cc.read());
        log::debug!("CSTS: {:X}", regs.csts.read());

        let cap_low = regs.cap_low.read() as u64;
        let cap_high = regs.cap_high.read() as u64;
        let caps = Caps(cap_low | cap_high << 32);

        Ok(Nvme {
            regs: RwLock::new(unsafe { &mut *(address as *mut NvmeRegs) }),
            thread_ctxts: RwLock::new(
                iter::once((
                    0_u16,
                    Arc::new(ReentrantMutex::new(ThreadCtxt {
                        buffer: RefCell::new(unsafe { Dma::zeroed()?.assume_init() }),
                        buffer_prp: RefCell::new(unsafe { Dma::zeroed()?.assume_init() }),
                        queues: RefCell::new(HashMap::new()),
                        io_queue: 1,
                    })),
                ))
                .collect(),
            ),

            cq_ivs: RwLock::new(iter::once((0, 0)).collect()),
            sq_ivs: RwLock::new(iter::once((0, 0)).collect()),

            interrupt_vector: Mutex::new(interrupt_vector),
            pcid_interface: Mutex::new(pcid_interface),

            // TODO
            next_sqid: AtomicSqId::new(2),
            next_cqid: AtomicCqId::new(2),

            caps,

            #[cfg(feature = "grant-dma")]
            prp_lists: Mutex::new(Vec::new()),
            #[cfg(feature = "grant-dma")]
            max_transfer: std::sync::atomic::AtomicU64::new(ZEROCOPY_CHUNK),
            #[cfg(feature = "percore")]
            percore_vectors: Mutex::new(HashMap::new()),
        })
    }
    /// # Locking
    /// Locks `regs`.
    unsafe fn get_doorbell(&self, index: usize) -> &'static mut Mmio<u32> {
        use std::ops::DerefMut;

        let mut regs_guard = self.regs.write();
        let regs: &mut NvmeRegs = regs_guard.deref_mut();

        let addr = (regs as *mut NvmeRegs as usize) + 0x1000 + index * (4 << self.caps.dstrd());
        &mut *(addr as *mut Mmio<u32>)
    }
    fn cur_thread_ctxt(&self) -> Arc<ReentrantMutex<ThreadCtxt>> {
        // Ferric Oxide: a per-CPU worker has its own context, keyed by its vector.
        let key = THREAD_CTXT.with(|key| key.get());
        Arc::clone(self.thread_ctxts.read().get(&key).unwrap())
    }

    pub unsafe fn init(&mut self) -> Result<()> {
        // first (zero) offset of io_cq_id / io_sq_id
        let asq_doorbell = self.get_doorbell(0);
        let acq_doorbell = self.get_doorbell(1);

        let thread_ctxts = self.thread_ctxts.get_mut();

        log::debug!("Disabling controller.");
        self.regs.get_mut().cc.writef(1, false);

        {
            log::trace!("Waiting for not ready.");
            let timeout = Timeout::from_secs(1);
            loop {
                let csts = self.regs.get_mut().csts.read();
                log::trace!("CSTS: {:X}", csts);
                if csts & 1 == 1 {
                    timeout.run().map_err(|()| {
                        log::error!("failed to wait for not ready");
                        Error::new(EIO)
                    })?;
                } else {
                    break;
                }
            }
        }

        if !self.interrupt_vector.get_mut().set_masked_if_fast(false) {
            self.regs.get_mut().intms.write(0xFFFF_FFFF);
            self.regs.get_mut().intmc.write(0x0000_0001);
        }

        {
            let main_ctxt = thread_ctxts.get(&0).unwrap().lock();

            for (i, prp) in main_ctxt.buffer_prp.borrow_mut().iter_mut().enumerate() {
                *prp = (main_ctxt.buffer.borrow_mut().physical() + i * 4096) as u64;
            }

            let regs = self.regs.get_mut();

            let mut queues = main_ctxt.queues.borrow_mut();
            let asq = NvmeCmdQueue::new(asq_doorbell)?;
            let acq = NvmeCompQueue::new(acq_doorbell)?;

            regs.aqa
                .write(((acq.data.len() as u32 - 1) << 16) | (asq.data.len() as u32 - 1));
            regs.asq_low.write(asq.data.physical() as u32);
            regs.asq_high
                .write((asq.data.physical() as u64 >> 32) as u32);
            regs.acq_low.write(acq.data.physical() as u32);
            regs.acq_high
                .write((acq.data.physical() as u64 >> 32) as u32);

            // Set IOCQES, IOSQES, AMS, MPS, and CSS
            let mut cc = regs.cc.read();
            cc &= 0xFF00000F;
            cc |= (4 << 20) | (6 << 16);
            regs.cc.write(cc);

            queues.insert(0, (asq, acq));
        }

        log::debug!("Enabling controller.");
        self.regs.get_mut().cc.writef(1, true);

        {
            log::debug!("Waiting for ready");
            let timeout = Timeout::from_secs(1);
            loop {
                let csts = self.regs.get_mut().csts.read();
                log::debug!("CSTS: {:X}", csts);
                if csts & 1 == 0 {
                    timeout.run().map_err(|()| {
                        log::error!("failed to wait for ready");
                        Error::new(EIO)
                    })?;
                } else {
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn set_vector_masked(&self, vector: u16, masked: bool) {
        // Ferric Oxide: a per-CPU queue pair's own MSI-X entry.
        #[cfg(feature = "percore")]
        if let Some(own) = self.percore_vectors.lock().get_mut(&vector) {
            if own.set_masked_if_fast(masked) {
                return;
            }
        }
        let mut interrupt_vector_guard = (&self).interrupt_vector.lock();

        if !interrupt_vector_guard.set_masked_if_fast(masked) {
            let mut to_mask = 0x0000_0000;
            let mut to_clear = 0x0000_0000;

            let vector = vector as u8;

            if masked {
                assert_ne!(
                    to_clear & (1 << vector),
                    (1 << vector),
                    "nvmed: internal error: cannot both mask and set"
                );
                to_mask |= 1 << vector;
            } else {
                assert_ne!(
                    to_mask & (1 << vector),
                    (1 << vector),
                    "nvmed: internal error: cannot both mask and set"
                );
                to_clear |= 1 << vector;
            }

            if to_mask != 0 {
                (&self).regs.write().intms.write(to_mask);
            }
            if to_clear != 0 {
                (&self).regs.write().intmc.write(to_clear);
            }
        }
    }

    pub async fn submit_and_complete_command(
        &self,
        sq_id: SqId,
        cmd_init: impl FnMut(CmdId) -> NvmeCmd,
    ) -> NvmeComp {
        NvmeExecutor::current().submit(sq_id, cmd_init).await
    }

    pub async fn submit_and_complete_admin_command(
        &self,
        cmd_init: impl FnMut(CmdId) -> NvmeCmd,
    ) -> NvmeComp {
        self.submit_and_complete_command(0, cmd_init).await
    }
    pub async fn create_io_completion_queue(
        &self,
        io_cq_id: CqId,
        vector: Option<Iv>,
    ) -> NvmeCompQueue {
        self.create_io_completion_queue_status(io_cq_id, vector)
            .await
            .0
    }

    /// Ferric Oxide: [`Self::create_io_completion_queue`], with the command's status field
    /// (0 on success).
    pub async fn create_io_completion_queue_status(
        &self,
        io_cq_id: CqId,
        vector: Option<Iv>,
    ) -> (NvmeCompQueue, u16) {
        let doorbell = unsafe { self.get_doorbell(2 * (io_cq_id as usize) + 1) };
        let queue =
            NvmeCompQueue::new(doorbell).expect("nvmed: failed to allocate I/O completion queue");

        let len = u16::try_from(queue.data.len())
            .expect("nvmed: internal error: I/O CQ longer than 2^16 entries");
        let raw_len = len
            .checked_sub(1)
            .expect("nvmed: internal error: CQID 0 for I/O CQ");

        let comp = self
            .submit_and_complete_admin_command(|cid| {
                NvmeCmd::create_io_completion_queue(
                    cid,
                    io_cq_id,
                    queue.data.physical(),
                    raw_len,
                    vector,
                )
            })
            .await;

        /*match comp.status.specific {
            1 => panic!("invalid queue identifier"),
            2 => panic!("invalid queue size"),
            8 => panic!("invalid interrupt vector"),
            _ => (),
        }*/

        (queue, comp.status >> 1)
    }
    pub async fn create_io_submission_queue(&self, io_sq_id: SqId, io_cq_id: CqId) -> NvmeCmdQueue {
        self.create_io_submission_queue_status(io_sq_id, io_cq_id)
            .await
            .0
    }

    /// Ferric Oxide: [`Self::create_io_submission_queue`], with the command's status field
    /// (0 on success).
    pub async fn create_io_submission_queue_status(
        &self,
        io_sq_id: SqId,
        io_cq_id: CqId,
    ) -> (NvmeCmdQueue, u16) {
        let doorbell = unsafe { self.get_doorbell(2 * (io_sq_id as usize)) };
        let q = NvmeCmdQueue::new(doorbell).expect("failed to create submission queue");

        let len = u16::try_from(q.data.len())
            .expect("nvmed: internal error: I/O SQ longer than 2^16 entries");
        let raw_len = len
            .checked_sub(1)
            .expect("nvmed: internal error: SQID 0 for I/O SQ");

        let comp = self
            .submit_and_complete_admin_command(|cid| {
                NvmeCmd::create_io_submission_queue(
                    cid,
                    io_sq_id,
                    q.data.physical(),
                    raw_len,
                    io_cq_id,
                )
            })
            .await;
        /*match comp.status.specific {
            0 => panic!("completion queue invalid"),
            1 => panic!("invalid queue identifier"),
            2 => panic!("invalid queue size"),
            _ => (),
        }*/

        (q, comp.status >> 1)
    }

    pub async fn init_with_queues(&self) -> BTreeMap<u32, NvmeNamespace> {
        log::trace!("preinit");

        self.identify_controller().await;

        let nsids = self.identify_namespace_list(0).await;

        log::debug!("first commands");

        let mut namespaces = BTreeMap::new();

        for nsid in nsids.iter().copied() {
            namespaces.insert(nsid, self.identify_namespace(nsid).await);
        }

        // TODO: Multiple queues
        let cq = self.create_io_completion_queue(1, Some(0)).await;
        log::trace!("created compq");
        let sq = self.create_io_submission_queue(1, 1).await;
        log::trace!("created subq");
        self.thread_ctxts
            .read()
            .get(&0)
            .unwrap()
            .lock()
            .queues
            .borrow_mut()
            .insert(1, (sq, cq));
        self.sq_ivs.write().insert(1, 0);
        self.cq_ivs.write().insert(1, 0);

        namespaces
    }

    async fn namespace_rw(
        &self,
        ctxt: &ThreadCtxt,
        namespace: &NvmeNamespace,
        lba: u64,
        blocks_1: u16,
        write: bool,
        #[cfg(feature = "perf")] timing: &mut crate::perf::RequestTiming,
    ) -> Result<()> {
        let block_size = namespace.block_size;

        let prp = ctxt.buffer_prp.borrow_mut();
        let bytes = ((blocks_1 as u64) + 1) * block_size;
        let (ptr0, ptr1) = if bytes <= 4096 {
            (prp[0], 0)
        } else if bytes <= 8192 {
            (prp[0], prp[1])
        } else {
            (prp[0], (prp.physical() + 8) as u64)
        };

        let mut cmd = NvmeCmd::default();
        #[cfg(feature = "perf")]
        let mut perf_device_start = 0;
        let comp = self
            .submit_and_complete_command(ctxt.io_queue, |cid| {
                // The executor calls this only after it has an SQ slot.
                #[cfg(feature = "perf")]
                {
                    perf_device_start = crate::perf::ticks();
                }
                cmd = if write {
                    NvmeCmd::io_write(cid, namespace.id, lba, blocks_1, ptr0, ptr1)
                } else {
                    NvmeCmd::io_read(cid, namespace.id, lba, blocks_1, ptr0, ptr1)
                };
                cmd
            })
            .await;
        #[cfg(feature = "perf")]
        timing.command(perf_device_start);

        let status = comp.status >> 1;
        if status == 0 {
            Ok(())
        } else {
            log::error!("command {:#x?} failed with status {:#x}", cmd, status);
            Err(Error::new(EIO))
        }
    }

    pub async fn namespace_read(
        &self,
        namespace: &NvmeNamespace,
        mut lba: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        #[cfg(feature = "perf")]
        let mut timing = crate::perf::RequestTiming::new(false);
        let ctxt = self.cur_thread_ctxt();
        let ctxt = ctxt.lock();

        let block_size = namespace.block_size as usize;

        for chunk in buf.chunks_mut(/* TODO: buf len */ 8192) {
            let blocks = chunk.len().div_ceil(block_size);

            assert!(blocks > 0);
            assert!(blocks <= 0x1_0000);

            self.namespace_rw(
                &ctxt,
                namespace,
                lba,
                (blocks - 1) as u16,
                false,
                #[cfg(feature = "perf")]
                &mut timing,
            )
            .await?;

            chunk.copy_from_slice(&ctxt.buffer.borrow()[..chunk.len()]);

            lba += blocks as u64;
        }

        Ok(buf.len())
    }

    pub async fn namespace_write(
        &self,
        namespace: &NvmeNamespace,
        mut lba: u64,
        buf: &[u8],
    ) -> Result<usize> {
        #[cfg(feature = "perf")]
        let mut timing = crate::perf::RequestTiming::new(true);
        let ctxt = self.cur_thread_ctxt();
        let ctxt = ctxt.lock();

        let block_size = namespace.block_size as usize;

        for chunk in buf.chunks(/* TODO: buf len */ 8192) {
            let blocks = chunk.len().div_ceil(block_size);

            assert!(blocks > 0);
            assert!(blocks <= 0x1_0000);

            ctxt.buffer.borrow_mut()[..chunk.len()].copy_from_slice(chunk);

            self.namespace_rw(
                &ctxt,
                namespace,
                lba,
                (blocks - 1) as u16,
                true,
                #[cfg(feature = "perf")]
                &mut timing,
            )
            .await?;

            lba += blocks as u64;
        }

        Ok(buf.len())
    }

    /// Ferric Oxide: records the controller's MDTS, which bounds zero-copy transfers.
    #[cfg(feature = "grant-dma")]
    fn set_mdts(&self, mdts: u8) {
        let max = ferric_blkgrant::prp::max_transfer(mdts, self.caps.min_page_size());
        log::info!(
            "  - Zero-copy transfers of up to {} KiB (MDTS {mdts})",
            max / 1024
        );
        self.max_transfer
            .store(max, std::sync::atomic::Ordering::Relaxed);
    }

    /// The most bytes one zero-copy command carries.
    fn max_zerocopy(&self) -> u64 {
        #[cfg(feature = "grant-dma")]
        return self.max_transfer.load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(feature = "grant-dma"))]
        ZEROCOPY_CHUNK
    }

    /// Ferric Oxide: a PRP list page, from the spares or newly allocated.
    #[cfg(feature = "grant-dma")]
    fn take_prp_list(&self) -> Result<Dma<[u64; ferric_blkgrant::prp::LIST_ENTRIES]>> {
        if let Some(list) = self.prp_lists.lock().pop() {
            return Ok(list);
        }
        // SAFETY: an all-zero array of integers is initialized.
        Ok(unsafe { Dma::zeroed()?.assume_init() })
    }

    async fn namespace_rw_zerocopy(
        &self,
        namespace: &NvmeNamespace,
        lba: u64,
        blocks_1: u16,
        write: bool,
        phys_addr: u64,
        #[cfg(feature = "perf")] timing: &mut crate::perf::RequestTiming,
    ) -> Result<()> {
        let block_size = namespace.block_size;

        let bytes = ((blocks_1 as u64) + 1) * block_size;
        #[cfg(feature = "grant-dma")]
        let mut prp_list = None;
        let (ptr0, ptr1) = if bytes <= 4096 {
            (phys_addr, 0)
        } else if bytes <= 8192 {
            (phys_addr, phys_addr + 4096)
        } else {
            // Ferric Oxide: longer transfers point PRP2 at a list of the following pages.
            #[cfg(feature = "grant-dma")]
            {
                let mut list = self.take_prp_list()?;
                let list_addr = list.physical() as u64;
                let pointers =
                    ferric_blkgrant::prp::data_pointers(phys_addr, bytes, &mut list[..], list_addr)
                        .map_err(|_| Error::new(syscall::EINVAL))?;
                prp_list = Some(list);
                pointers
            }
            #[cfg(not(feature = "grant-dma"))]
            return Err(Error::new(syscall::EINVAL));
        };

        let mut cmd = NvmeCmd::default();
        // Ferric Oxide: this thread's I/O queue.
        let io_queue = self.cur_thread_ctxt().lock().io_queue;
        #[cfg(feature = "perf")]
        let mut perf_device_start = 0;
        let comp = self
            .submit_and_complete_command(io_queue, |cid| {
                // The executor calls this only after it has an SQ slot.
                #[cfg(feature = "perf")]
                {
                    perf_device_start = crate::perf::ticks();
                }
                cmd = if write {
                    NvmeCmd::io_write(cid, namespace.id, lba, blocks_1, ptr0, ptr1)
                } else {
                    NvmeCmd::io_read(cid, namespace.id, lba, blocks_1, ptr0, ptr1)
                };
                cmd
            })
            .await;

        #[cfg(feature = "perf")]
        timing.command(perf_device_start);

        // Ferric Oxide: the controller is done with the list once the command completed.
        #[cfg(feature = "grant-dma")]
        if let Some(list) = prp_list {
            self.prp_lists.lock().push(list);
        }

        let status = comp.status >> 1;
        if status == 0 {
            Ok(())
        } else {
            log::error!("command {:#x?} failed with status {:#x}", cmd, status);
            Err(Error::new(EIO))
        }
    }

    pub async fn namespace_read_zerocopy(
        &self,
        namespace: &NvmeNamespace,
        start_lba: u64,
        mut phys_addr: u64,
        num_sectors: u64,
    ) -> Result<()> {
        #[cfg(feature = "perf")]
        let mut timing = crate::perf::RequestTiming::new(false);
        let end_lba = start_lba + num_sectors;

        // TODO: PRP
        let max_blocks = self.max_zerocopy() / namespace.block_size;

        let mut lba = start_lba;
        while lba < end_lba {
            let blocks = (end_lba - lba).min(max_blocks);

            self.namespace_rw_zerocopy(
                namespace,
                lba,
                (blocks - 1) as u16,
                false,
                phys_addr,
                #[cfg(feature = "perf")]
                &mut timing,
            )
            .await?;

            phys_addr += blocks * namespace.block_size;
            lba += blocks;
        }

        Ok(())
    }

    pub async fn namespace_write_zerocopy(
        &self,
        namespace: &NvmeNamespace,
        start_lba: u64,
        mut phys_addr: u64,
        num_sectors: u64,
    ) -> Result<()> {
        #[cfg(feature = "perf")]
        let mut timing = crate::perf::RequestTiming::new(true);
        let end_lba = start_lba + num_sectors;
        // TODO: PRP
        let max_blocks = self.max_zerocopy() / namespace.block_size;

        let mut lba = start_lba;
        while lba < end_lba {
            let blocks = (end_lba - lba).min(max_blocks);

            self.namespace_rw_zerocopy(
                namespace,
                lba,
                (blocks - 1) as u16,
                true,
                phys_addr,
                #[cfg(feature = "perf")]
                &mut timing,
            )
            .await?;

            phys_addr += blocks * namespace.block_size;
            lba += blocks;
        }

        Ok(())
    }
}
