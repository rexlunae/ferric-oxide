//! Ferric Oxide: the controller side of per-CPU I/O queue pairs (roadmap Phase 2.4, PR 4;
//! features.toml `drivers.nvme_percore`).
//!
//! The admin queue and the first I/O queue pair stay with the main thread, on MSI-X entry 0.
//! [`Nvme::request_io_queues`] asks the controller how many more queue pairs it grants (Set
//! Features, Number of Queues, before the first I/O queue is created), and [`Nvme::add_queue_pair`] creates one with its own completion
//! queue, MSI-X entry and thread context (bounce buffer included). The thread that serves it
//! then calls [`Nvme::enter_queue_pair`], after which its reads and writes go to that pair and
//! its executor polls only that pair's completion queue.

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
    /// `ENOMEM` when the context's buffers cannot be allocated.
    pub async fn add_queue_pair(&self, qid: u16, vector: InterruptVector) -> Result<()> {
        let iv: Iv = vector.vector();
        let cq = self.create_io_completion_queue(qid as CqId, Some(iv)).await;
        let sq = self
            .create_io_submission_queue(qid as SqId, qid as CqId)
            .await;
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

    /// Makes the calling thread serve the queue pair on MSI-X entry `vector`, which
    /// [`Self::add_queue_pair`] created.
    pub fn enter_queue_pair(&self, vector: Iv) {
        THREAD_CTXT.with(|key| key.set(vector));
    }
}
