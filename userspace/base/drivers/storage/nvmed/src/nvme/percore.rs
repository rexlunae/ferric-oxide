//! Ferric Oxide: the controller side of per-CPU I/O queue pairs (roadmap Phase 2.4, PR 4;
//! features.toml `drivers.nvme_percore`).
//!
//! The admin queue and the first I/O queue pair stay with the main thread, on MSI-X entry 0.
//! [`Nvme::request_io_queues`] asks the controller how many more queue pairs it grants (Set
//! Features, Number of Queues, before the first I/O queue is created), and [`Nvme::add_queue_pair`] creates one with its own completion
//! queue, MSI-X entry and thread context (bounce buffer included). The thread that serves it
//! then calls [`Nvme::enter_queue_pair`], after which its reads and writes go to that pair and
//! its executor polls only that pair's completion queue.
//!
//! A pair is published (its context and vectors recorded) only once the controller has
//! accepted both Create I/O Completion Queue (05h) and Create I/O Submission Queue (01h); if
//! the submission queue fails, the completion queue is deleted again with Delete I/O
//! Completion Queue (04h), as the NVMe Base Specification 2.0, section 5.6, describes, and
//! the caller carries on with the pairs it has.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use common::dma::Dma;
use parking_lot::ReentrantMutex;
use pcid_interface::irq_helpers::InterruptVector;
use syscall::error::{Error, Result, EIO};

use super::{CqId, Iv, Nvme, NvmeCmd, SqId, ThreadCtxt, THREAD_CTXT};

impl Nvme {
    /// Asks the controller for `wanted` I/O submission and completion queues in all (the first
    /// pair included), and returns how many pairs it grants: at most `wanted`, at least 1.
    ///
    /// Call it before any I/O queue is created: the NVMe Base Specification allows Number of
    /// Queues (feature 07h) only then, and a controller may fail the command afterwards (QEMU
    /// answers Command Sequence Error).
    ///
    /// # Errors
    ///
    /// `EIO` when the controller fails the command.
    pub async fn request_io_queues(&self, wanted: u16) -> Result<u16> {
        let wanted_1 = wanted.max(1) - 1;
        let comp = self
            .submit_and_complete_admin_command(|cid| {
                NvmeCmd::set_features_number_of_queues(cid, wanted_1, wanted_1)
            })
            .await;
        if comp.status >> 1 != 0 {
            log::warn!(
                "nvmed: Set Features (Number of Queues) failed with status {:#x}",
                comp.status >> 1
            );
            return Err(Error::new(EIO));
        }
        // Dword 0: completion queues allocated in bits 31:16, submission queues in 15:0, both
        // 0-based, and possibly more than asked for.
        let dw0 = comp.command_specific;
        let sqs = (dw0 & 0xffff) as u16;
        let cqs = (dw0 >> 16) as u16;
        Ok(sqs.min(cqs).saturating_add(1).min(wanted.max(1)))
    }

    /// Creates I/O queue pair `qid` (completion queue and submission queue with the same id)
    /// interrupting through MSI-X table entry `vector`, with a thread context of its own.
    ///
    /// # Errors
    ///
    /// - `EIO` when the controller fails either Create command; nothing is published, and a
    ///   completion queue it had created is deleted again.
    /// - `ENOMEM` when the context's buffers cannot be allocated.
    pub async fn add_queue_pair(&self, qid: u16, vector: InterruptVector) -> Result<()> {
        let iv: Iv = vector.vector();
        let (cq, status) = self
            .create_io_completion_queue_status(qid as CqId, Some(iv))
            .await;
        if status != 0 {
            log::error!("nvmed: Create I/O Completion Queue {qid} failed with status {status:#x}");
            return Err(Error::new(EIO));
        }
        let (sq, status) = self
            .create_io_submission_queue_status(qid as SqId, qid as CqId)
            .await;
        if status != 0 {
            log::error!("nvmed: Create I/O Submission Queue {qid} failed with status {status:#x}");
            self.delete_completion_queue(qid, cq).await;
            return Err(Error::new(EIO));
        }
        let ctxt = ThreadCtxt {
            // SAFETY: all-zero bytes and integers are initialized.
            buffer: RefCell::new(unsafe { Dma::zeroed()?.assume_init() }),
            buffer_prp: RefCell::new(unsafe { Dma::zeroed()?.assume_init() }),
            queues: RefCell::new(HashMap::from([(qid, (sq, cq))])),
            io_queue: qid,
        };
        for (i, prp) in ctxt.buffer_prp.borrow_mut().iter_mut().enumerate() {
            *prp = (ctxt.buffer.borrow().physical() + i * 4096) as u64;
        }
        self.percore_vectors.lock().insert(iv, vector);
        self.thread_ctxts
            .write()
            .insert(iv, Arc::new(ReentrantMutex::new(ctxt)));
        self.sq_ivs.write().insert(qid, iv);
        self.cq_ivs.write().insert(qid, iv);
        Ok(())
    }

    /// Deletes I/O completion queue `qid`, which has no submission queue, and frees its memory
    /// once the controller has let go of it.
    async fn delete_completion_queue(&self, qid: u16, cq: super::NvmeCompQueue) {
        let comp = self
            .submit_and_complete_admin_command(|cid| NvmeCmd::delete_io_completion_queue(cid, qid))
            .await;
        let status = comp.status >> 1;
        if status != 0 {
            log::error!(
                "nvmed: Delete I/O Completion Queue {qid} failed with status {status:#x}; its memory stays allocated"
            );
            // The controller may still write completions there, so the memory must not be
            // reused.
            std::mem::forget(cq);
        }
    }

    /// Whether queue pair `qid`'s completion queue, in the calling thread's context, has an
    /// entry waiting that nothing has taken yet.
    pub fn completion_waiting(&self, qid: u16) -> bool {
        let ctxt = self.cur_thread_ctxt();
        let guard = ctxt.lock();
        let waiting = guard
            .queues
            .borrow()
            .get(&qid)
            .is_some_and(|(_, cq)| cq.has_entry());
        waiting
    }

    /// Makes the calling thread serve the queue pair on MSI-X entry `vector`, which
    /// [`Self::add_queue_pair`] created.
    pub fn enter_queue_pair(&self, vector: Iv) {
        THREAD_CTXT.with(|key| key.set(vector));
    }
}

impl NvmeCmd {
    /// Delete I/O Completion Queue (admin opcode 04h, NVMe Base Specification 2.0, section
    /// 5.6): command dword 10 bits 15:0 hold the queue id.
    fn delete_io_completion_queue(cid: u16, qid: u16) -> Self {
        Self {
            opcode: 0x04,
            flags: 0,
            cid,
            nsid: 0,
            _rsvd: 0,
            mptr: 0,
            dptr: [0, 0],
            cdw10: u32::from(qid),
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        }
    }
}
