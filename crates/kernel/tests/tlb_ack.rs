//! Shootdown acknowledgements with broadcasts: the actual mailbox (`memory/tlb.rs`) and
//! global-flush generation (`percpu/global_flush.rs`), wired as `percpu.rs` wires them.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    vec::Vec,
};

use crate::{
    cpu_set::LogicalCpuId,
    global_flush::GlobalFlushRequests,
    tlb::{TlbAcks, TlbInvalidation, TlbMailbox},
};

const CPUS: usize = 4;

struct Cpu {
    mailbox: TlbMailbox,
    flag: AtomicBool,
    pages: Mutex<Vec<usize>>,
    full_flushes: AtomicUsize,
    global_flushes: AtomicUsize,
}

struct System {
    cpus: Vec<Cpu>,
    global: GlobalFlushRequests<CPUS>,
}

impl System {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cpus: (0..CPUS)
                .map(|_| Cpu {
                    mailbox: TlbMailbox::new(),
                    flag: AtomicBool::new(false),
                    pages: Mutex::new(Vec::new()),
                    full_flushes: AtomicUsize::new(0),
                    global_flushes: AtomicUsize::new(0),
                })
                .collect(),
            global: GlobalFlushRequests::new(),
        })
    }

    /// `PercpuBlock::maybe_handle_tlb_shootdown` on `cpu`.
    fn serve(&self, cpu: usize) {
        let me = &self.cpus[cpu];
        if !me.flag.swap(false, Ordering::Acquire) {
            return;
        }
        me.mailbox.receive(
            |page| me.pages.lock().unwrap().push(page),
            || {
                self.global.flush(
                    cpu,
                    || {
                        me.global_flushes.fetch_add(1, Ordering::SeqCst);
                    },
                    || {},
                );
                me.full_flushes.fetch_add(1, Ordering::SeqCst);
            },
        );
    }

    /// `request_tlb_shootdown`, then waiting as `wait_for_tlb_acks` does.
    fn shootdown(&self, me: usize, targets: impl Iterator<Item = usize>, inv: &TlbInvalidation) {
        let mut acks = TlbAcks::new();
        for target in targets.filter(|&t| t != me) {
            let ack = self.cpus[target].mailbox.post(inv);
            self.cpus[target].flag.store(true, Ordering::Release);
            acks.record(LogicalCpuId::new(target as u32), ack).unwrap();
        }
        while acks.poll() != 0 {
            self.serve(me);
            std::thread::yield_now();
        }
    }

    /// `shootdown_tlb_all`.
    fn broadcast(&self, me: usize) {
        self.global.request();
        self.shootdown(me, 0..CPUS, &TlbInvalidation::ALL);
    }
}

fn page_list(page: usize) -> TlbInvalidation {
    let mut inv = TlbInvalidation::new();
    inv.add(page, 1);
    inv
}

/// A broadcast and a targeted round wait only for their own acknowledgements. A CPU that has
/// answered the broadcast has not answered a targeted round posted after it, and the other way
/// round.
#[test]
fn broadcast_and_targeted_round_do_not_satisfy_each_other() {
    let system = System::new();
    let target = &system.cpus[2].mailbox;

    let targeted = target.post(&page_list(7));
    system.global.request();
    let broadcast = target.post(&TlbInvalidation::ALL);
    let mut acks = TlbAcks::new();
    acks.record(LogicalCpuId::new(2), broadcast).unwrap();
    // Some other CPU answering does not help.
    let _ = system.cpus[3].mailbox.post(&TlbInvalidation::ALL);
    system.cpus[3].flag.store(true, Ordering::Release);
    system.serve(3);
    assert_eq!(acks.poll(), 1);
    assert!(!targeted.is_handled());

    system.cpus[2].flag.store(true, Ordering::Release);
    system.serve(2);
    assert!(targeted.is_handled());
    assert_eq!(acks.poll(), 0);
    assert_eq!(system.cpus[2].global_flushes.load(Ordering::SeqCst), 1);

    // A later targeted round needs a later handling.
    let later = target.post(&page_list(8));
    assert!(!later.is_handled());
}

/// One CPU broadcasts, another runs targeted rounds, and the rest only answer, all at once.
/// When a broadcast's wait ends, every other CPU has done a global flush since it began; when a
/// targeted round's wait ends, its target has invalidated the page or flushed everything since.
/// Both senders keep answering while they wait, so they never wait on each other for good.
#[test]
fn broadcast_racing_targeted_rounds_waits_for_each() {
    const ROUNDS: usize = if cfg!(miri) { 5 } else { 500 };
    const BROADCASTER: usize = 0;
    const SENDER: usize = 1;

    let system = System::new();
    let finished = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let answerers: Vec<_> = (2..CPUS)
        .map(|cpu| {
            let (system, done) = (system.clone(), done.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    system.serve(cpu);
                    std::thread::yield_now();
                }
            })
        })
        .collect();

    let finish = |system: &System, finished: &AtomicUsize, me: usize| {
        finished.fetch_add(1, Ordering::AcqRel);
        while finished.load(Ordering::Acquire) < 2 {
            system.serve(me);
            std::thread::yield_now();
        }
    };

    let broadcaster = {
        let (system, finished) = (system.clone(), finished.clone());
        std::thread::spawn(move || {
            for round in 0..ROUNDS {
                let before: Vec<_> = system
                    .cpus
                    .iter()
                    .map(|cpu| cpu.global_flushes.load(Ordering::SeqCst))
                    .collect();
                system.broadcast(BROADCASTER);
                for (cpu, before) in before.into_iter().enumerate() {
                    if cpu != BROADCASTER {
                        assert!(
                            system.cpus[cpu].global_flushes.load(Ordering::SeqCst) > before,
                            "broadcast {round}: CPU {cpu} acknowledged without a global flush"
                        );
                    }
                }
            }
            finish(&system, &finished, BROADCASTER);
        })
    };

    let sender = {
        let (system, finished) = (system.clone(), finished.clone());
        std::thread::spawn(move || {
            for round in 0..ROUNDS {
                let page = 1000 + round;
                let targets = [BROADCASTER, 2];
                let before: Vec<_> = targets
                    .iter()
                    .map(|&cpu| system.cpus[cpu].full_flushes.load(Ordering::SeqCst))
                    .collect();
                system.shootdown(SENDER, targets.into_iter(), &page_list(page));
                for (&cpu, before) in targets.iter().zip(before) {
                    let target = &system.cpus[cpu];
                    assert!(
                        target.pages.lock().unwrap().contains(&page)
                            || target.full_flushes.load(Ordering::SeqCst) > before,
                        "round {round}: CPU {cpu} acknowledged without invalidating"
                    );
                }
            }
            finish(&system, &finished, SENDER);
        })
    };

    broadcaster.join().unwrap();
    sender.join().unwrap();
    done.store(true, Ordering::Release);
    for answerer in answerers {
        answerer.join().unwrap();
    }
}
