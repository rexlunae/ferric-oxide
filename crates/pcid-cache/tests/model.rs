//! A model checker for the PCID slot cache: a simulated machine with a PCID-tagged TLB and
//! paging-structure caches, CPUs that switch address spaces through [`SlotCache`] as the
//! kernel will, and flushes, shootdown IPIs and teardown as the kernel does them.
//!
//! The oracle: no CPU ever translates through an entry that belongs to another address space,
//! to a destroyed one, or to a page-table entry (or page table) that was removed or downgraded
//! by a flush this CPU has already acknowledged. A stale entry is allowed only on a CPU that
//! still owes the flush that retired it an acknowledgement: until then the flushing CPU has not
//! freed anything, which is today's shootdown contract.
//!
//! Two drivers run the same machine: random operation sequences over many seeds, and a
//! breadth-first search of every sequence up to a depth over a small machine. Each runs
//! correct and with mutations (a skipped generation bump, NOFLUSH on a miss, the generation
//! read before `used_by` is set, and others), which must be caught.
//!
//! The model follows Intel SDM Vol. 3A §4.10: a CR3 load without NOFLUSH drops the non-global
//! entries of the PCID it loads, `INVLPG` and `INVPCID` type 0 drop one page's entries in one
//! PCID along with its paging-structure cache entries, `INVPCID` type 1 drops a whole PCID, and
//! lookups only use entries of the loaded PCID. User pages are never global, so globals are not
//! modelled. The invalidation log is a stand-in for the per-address-space log of the
//! `tlb-generation` change (pcid-design.md §3.4), with the same overflow and "full record" rules.

use std::collections::{BTreeMap, HashSet, VecDeque};

use ferric_pcid_cache::{CtxId, SlotCache, SlotIndex, SwitchKind, TlbGen};

/// Log records kept per address space.
const LOG_RECORDS: usize = 3;
/// Pages a replay may invalidate; a longer gap flushes instead.
const REPLAY_MAX: usize = 2;
/// Pages per page table: unmapping both pages of a table frees the table.
const TABLE_PAGES: usize = 2;

/// A deliberate bug, to check that the oracle catches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Mutation {
    /// Correct protocol.
    None,
    /// The flush of a downgrade (mprotect) forgets to bump the generation.
    SkipDowngradeBump,
    /// A miss loads the reused slot with NOFLUSH, keeping the evicted address space's entries.
    NoflushOnMiss,
    /// The switch reads the generation before it sets `used_by`.
    GenBeforeUsedBy,
    /// A flushing load forgets `INVPCID` type 1 on the user (KPTI shadow) PCID.
    SkipUserPcidFlush,
    /// A replay only invalidates the kernel PCID, as plain `invlpg` would.
    ReplayKernelPcidOnly,
    /// A new address space reuses the context id of the one it replaces.
    ReuseCtxIds,
    /// A shootdown receiver acknowledges without catching its slot up.
    AckWithoutCatchUp,
    /// The flushing CPU marks its slot current without invalidating its own TLB.
    SkipLocalInvalidate,
}

/// Which PCID an access goes through: the shadow's (user mode under KPTI) or the kernel table's
/// (the kernel touching user memory).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Mode {
    User,
    Kernel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Op {
    /// The first half of a context switch: leave the old address space, join the new one's
    /// `used_by` and read its generation.
    SwitchBegin { cpu: usize, target: Option<usize> },
    /// The second half: consult the cache and load CR3.
    SwitchCommit { cpu: usize },
    /// A load or store through the TLB.
    Access {
        cpu: usize,
        page: usize,
        mode: Mode,
        write: bool,
    },
    /// The next change to a page (map, then downgrade, then unmap), made and flushed by `cpu`.
    Change {
        cpu: usize,
        space: usize,
        page: usize,
    },
    /// A pending shootdown IPI arrives.
    Deliver { cpu: usize },
    /// The address space is torn down and a new one takes its place.
    Recreate { space: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Violation {
    /// An entry filled for another address space was used.
    Foreign { cpu: usize, version: usize },
    /// An entry of a destroyed address space was used.
    Dead { cpu: usize, version: usize },
    /// An entry retired by a flush this CPU acknowledged was used.
    Stale { cpu: usize, version: usize },
}

/// A page-table entry or a page table, as a unique version: every change retires the old one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum VersionState {
    Live,
    /// Removed or downgraded by this flush.
    Retired(usize),
    /// Its address space was destroyed.
    Dead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Version {
    /// The address-space instance (never reused) it belongs to.
    owner: usize,
    state: VersionState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Pte {
    version: usize,
    writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Record {
    Pages(Vec<usize>),
    /// Too many pages, or a page table was freed: catching up needs a full flush.
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Space {
    instance: usize,
    ctx: CtxId,
    generation: u64,
    pages: Vec<Option<Pte>>,
    tables: Vec<Option<usize>>,
    log: VecDeque<(u64, Record)>,
    used_by: u32,
    /// The flush whose acknowledgements its flusher waits for, holding the address-space lock.
    busy: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Loaded {
    space: usize,
    instance: usize,
    slot: SlotIndex,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Cpu<const N: usize> {
    cache: SlotCache<N>,
    loaded: Option<Loaded>,
    /// (PCID, page) -> (version, writable).
    tlb: BTreeMap<(u16, usize), (usize, bool)>,
    /// (PCID, table) -> table version: the paging-structure caches.
    psc: BTreeMap<(u16, usize), usize>,
    pending: bool,
    /// Between `SwitchBegin` and `SwitchCommit`: the target and the generation read.
    switching: Option<(Option<usize>, Option<u64>)>,
}

/// What a run exercised, so a test can check that it reached every path. Ignored when machines
/// are compared, so it does not split the exhaustive search's states.
#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    hit: u64,
    replay: u64,
    stale: u64,
    miss: u64,
    ipis: u64,
    /// Accesses through a retired entry by a CPU that still owed the flush its acknowledgement.
    stale_allowed: u64,
}

impl Stats {
    fn add(&mut self, other: Self) {
        self.hit += other.hit;
        self.replay += other.replay;
        self.stale += other.stale;
        self.miss += other.miss;
        self.ipis += other.ipis;
        self.stale_allowed += other.stale_allowed;
    }
}

impl PartialEq for Stats {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Stats {}

impl std::hash::Hash for Stats {
    fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Machine<const N: usize> {
    mutation: Mutation,
    stats: Stats,
    cpus: Vec<Cpu<N>>,
    spaces: Vec<Space>,
    versions: Vec<Version>,
    /// Per flush, the CPUs that still owe an acknowledgement.
    outstanding: Vec<u32>,
    next_instance: usize,
    next_ctx: u64,
}

impl<const N: usize> Machine<N> {
    fn new(cpus: usize, spaces: usize, pages: usize, mutation: Mutation) -> Self {
        let mut machine = Self {
            mutation,
            stats: Stats::default(),
            cpus: (0..cpus)
                .map(|_| Cpu {
                    cache: SlotCache::new(),
                    loaded: None,
                    tlb: BTreeMap::new(),
                    psc: BTreeMap::new(),
                    pending: false,
                    switching: None,
                })
                .collect(),
            spaces: Vec::new(),
            versions: Vec::new(),
            outstanding: Vec::new(),
            next_instance: 0,
            next_ctx: 1,
        };
        for _ in 0..spaces {
            let space = machine.fresh_space(pages, None);
            machine.spaces.push(space);
        }
        machine
    }

    /// A new address space with every page mapped writable.
    fn fresh_space(&mut self, pages: usize, reuse_ctx: Option<CtxId>) -> Space {
        let instance = self.next_instance;
        self.next_instance += 1;
        let ctx = reuse_ctx.unwrap_or_else(|| {
            let ctx = CtxId::new(self.next_ctx).unwrap();
            self.next_ctx += 1;
            ctx
        });
        let tables = (0..pages.div_ceil(TABLE_PAGES))
            .map(|_| Some(self.new_version(instance)))
            .collect();
        let pages = (0..pages)
            .map(|_| {
                Some(Pte {
                    version: self.new_version(instance),
                    writable: true,
                })
            })
            .collect();
        Space {
            instance,
            ctx,
            generation: TlbGen::INITIAL.get(),
            pages,
            tables,
            log: VecDeque::new(),
            used_by: 0,
            busy: None,
        }
    }

    fn new_version(&mut self, owner: usize) -> usize {
        self.versions.push(Version {
            owner,
            state: VersionState::Live,
        });
        self.versions.len() - 1
    }

    fn enabled(&self, ops: &mut Vec<Op>, accesses: &[(Mode, bool)]) {
        ops.clear();
        for (cpu, state) in self.cpus.iter().enumerate() {
            if state.switching.is_some() {
                ops.push(Op::SwitchCommit { cpu });
                continue;
            }
            let current = state.loaded.as_ref().map(|loaded| loaded.space);
            for target in std::iter::once(None).chain((0..self.spaces.len()).map(Some)) {
                // Threads of one process switch without touching CR3.
                if target != current {
                    ops.push(Op::SwitchBegin { cpu, target });
                }
            }
            if let Some(loaded) = &state.loaded {
                for page in 0..self.spaces[loaded.space].pages.len() {
                    for &(mode, write) in accesses {
                        ops.push(Op::Access {
                            cpu,
                            page,
                            mode,
                            write,
                        });
                    }
                }
            }
            for (space, data) in self.spaces.iter().enumerate() {
                if data.busy.is_none() {
                    for page in 0..data.pages.len() {
                        ops.push(Op::Change { cpu, space, page });
                    }
                }
            }
            if state.pending {
                ops.push(Op::Deliver { cpu });
            }
        }
        for space in 0..self.spaces.len() {
            if self.can_destroy(space) {
                ops.push(Op::Recreate { space });
            }
        }
    }

    /// No CPU has the address space loaded or is switching to it, and no flush of it waits.
    fn can_destroy(&self, space: usize) -> bool {
        self.spaces[space].busy.is_none()
            && self.cpus.iter().all(|cpu| {
                cpu.loaded
                    .as_ref()
                    .is_none_or(|loaded| loaded.space != space)
                    && cpu
                        .switching
                        .is_none_or(|(target, _)| target != Some(space))
            })
    }

    fn step(&mut self, op: Op) -> Result<(), Violation> {
        match op {
            Op::SwitchBegin { cpu, target } => self.switch_begin(cpu, target),
            Op::SwitchCommit { cpu } => self.switch_commit(cpu),
            Op::Access {
                cpu,
                page,
                mode,
                write,
            } => return self.access(cpu, page, mode, write),
            Op::Change { cpu, space, page } => self.change(cpu, space, page),
            Op::Deliver { cpu } => self.deliver(cpu),
            Op::Recreate { space } => self.recreate(space),
        }
        Ok(())
    }

    fn switch_begin(&mut self, cpu: usize, target: Option<usize>) {
        let bit = 1 << cpu;
        if let Some(loaded) = &self.cpus[cpu].loaded {
            self.spaces[loaded.space].used_by &= !bit;
        }
        // `switch_arch_hook` services a pending shootdown once it has left the old address
        // space, still on its table.
        if self.cpus[cpu].pending {
            self.deliver(cpu);
        }
        let generation = target.map(|space| {
            if self.mutation != Mutation::GenBeforeUsedBy {
                self.spaces[space].used_by |= bit;
            }
            self.spaces[space].generation
        });
        self.cpus[cpu].switching = Some((target, generation));
    }

    fn switch_commit(&mut self, cpu: usize) {
        let (target, generation) = self.cpus[cpu].switching.take().unwrap();
        let Some(space) = target else {
            // The empty table under PCID 0, which holds no user translations.
            self.cpus[cpu].cache.switch_to_none();
            self.cpus[cpu].loaded = None;
            return;
        };
        if self.mutation == Mutation::GenBeforeUsedBy {
            self.spaces[space].used_by |= 1 << cpu;
        }
        let generation = generation.unwrap();
        let mut replay = Vec::new();
        let ctx = self.spaces[space].ctx;
        let log = &self.spaces[space].log;
        let switch = self.cpus[cpu]
            .cache
            .switch_to(ctx, TlbGen::new(generation), |from, to| match logged_pages(
                log,
                from.get(),
                to.get(),
            ) {
                Some(pages) => {
                    replay = pages;
                    true
                }
                None => false,
            });
        let slot = switch.slot;
        match switch.kind {
            SwitchKind::Hit => self.stats.hit += 1,
            SwitchKind::Replay { .. } => self.stats.replay += 1,
            SwitchKind::Stale { .. } => self.stats.stale += 1,
            SwitchKind::Miss { .. } => self.stats.miss += 1,
        }
        let flush = switch.flush()
            && !(self.mutation == Mutation::NoflushOnMiss
                && matches!(switch.kind, SwitchKind::Miss { .. }));
        let state = &mut self.cpus[cpu];
        if flush {
            // CR3 without NOFLUSH, then INVPCID type 1 on the user PCID.
            flush_pcid(state, slot.kernel_pcid());
            if self.mutation != Mutation::SkipUserPcidFlush {
                flush_pcid(state, slot.user_pcid());
            }
        }
        if let SwitchKind::Replay { .. } = switch.kind {
            for page in replay {
                invalidate_page(state, slot.kernel_pcid(), page);
                if self.mutation != Mutation::ReplayKernelPcidOnly {
                    invalidate_page(state, slot.user_pcid(), page);
                }
            }
        }
        state.loaded = Some(Loaded {
            space,
            instance: self.spaces[space].instance,
            slot,
        });
    }

    fn access(
        &mut self,
        cpu: usize,
        page: usize,
        mode: Mode,
        write: bool,
    ) -> Result<(), Violation> {
        let loaded = self.cpus[cpu].loaded.clone().unwrap();
        let pcid = match mode {
            Mode::User => loaded.slot.user_pcid(),
            Mode::Kernel => loaded.slot.kernel_pcid(),
        };
        if let Some(&(version, writable)) = self.cpus[cpu].tlb.get(&(pcid, page)) {
            if !write || writable {
                return self.check(cpu, &loaded, version);
            }
            // A write through a read-only entry faults; the fault handler's walk replaces it.
            self.cpus[cpu].tlb.remove(&(pcid, page));
        }
        let table = page / TABLE_PAGES;
        match self.cpus[cpu].psc.get(&(pcid, table)) {
            Some(&version) => self.check(cpu, &loaded, version)?,
            None => match self.spaces[loaded.space].tables[table] {
                Some(version) => {
                    self.cpus[cpu].psc.insert((pcid, table), version);
                }
                None => return Ok(()),
            },
        }
        if let Some(pte) = self.spaces[loaded.space].pages[page]
            && (!write || pte.writable)
        {
            self.cpus[cpu]
                .tlb
                .insert((pcid, page), (pte.version, pte.writable));
        }
        Ok(())
    }

    /// The oracle, for a translation (or cached page table) `version` used by `cpu`.
    fn check(&mut self, cpu: usize, loaded: &Loaded, version: usize) -> Result<(), Violation> {
        let info = self.versions[version];
        if info.owner != loaded.instance {
            return Err(Violation::Foreign { cpu, version });
        }
        match info.state {
            VersionState::Live => Ok(()),
            VersionState::Retired(flush) if self.outstanding[flush] & (1 << cpu) != 0 => {
                self.stats.stale_allowed += 1;
                Ok(())
            }
            VersionState::Retired(_) => Err(Violation::Stale { cpu, version }),
            VersionState::Dead => Err(Violation::Dead { cpu, version }),
        }
    }

    fn change(&mut self, cpu: usize, space: usize, page: usize) {
        let instance = self.spaces[space].instance;
        let table = page / TABLE_PAGES;
        let (retired, record, downgrade) = match self.spaces[space].pages[page] {
            None => {
                // A new mapping, maybe in a new table: nothing to invalidate on x86, so no flush
                // and no generation bump (as with the skip-newmap change).
                if self.spaces[space].tables[table].is_none() {
                    self.spaces[space].tables[table] = Some(self.new_version(instance));
                }
                let version = self.new_version(instance);
                self.spaces[space].pages[page] = Some(Pte {
                    version,
                    writable: true,
                });
                return;
            }
            Some(pte) if pte.writable => {
                let version = self.new_version(instance);
                self.spaces[space].pages[page] = Some(Pte {
                    version,
                    writable: false,
                });
                (vec![pte.version], Record::Pages(vec![page]), true)
            }
            Some(pte) => {
                let data = &mut self.spaces[space];
                data.pages[page] = None;
                let first = table * TABLE_PAGES;
                let last = (first + TABLE_PAGES).min(data.pages.len());
                if data.pages[first..last].iter().all(Option::is_none) {
                    // The table is freed with the batch: catching up must flush fully.
                    let table_version = data.tables[table].take().unwrap();
                    (vec![pte.version, table_version], Record::Full, false)
                } else {
                    (vec![pte.version], Record::Pages(vec![page]), false)
                }
            }
        };
        let flush = self.outstanding.len();
        for version in retired {
            self.versions[version].state = VersionState::Retired(flush);
        }
        let data = &mut self.spaces[space];
        if !(downgrade && self.mutation == Mutation::SkipDowngradeBump) {
            data.generation += 1;
            data.log.push_back((data.generation, record.clone()));
            if data.log.len() > LOG_RECORDS {
                data.log.pop_front();
            }
        }
        let generation = data.generation;
        let targets = data.used_by & !(1 << cpu);
        for target in 0..self.cpus.len() {
            if targets & (1 << target) != 0 {
                self.stats.ipis += 1;
                self.cpus[target].pending = true;
            }
        }
        let state = &mut self.cpus[cpu];
        if state
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.space == space)
        {
            let slot = state.loaded.as_ref().unwrap().slot;
            if self.mutation != Mutation::SkipLocalInvalidate {
                match record {
                    Record::Pages(pages) => {
                        for page in pages {
                            invalidate_page(state, slot.kernel_pcid(), page);
                            invalidate_page(state, slot.user_pcid(), page);
                        }
                    }
                    Record::Full => {
                        flush_pcid(state, slot.kernel_pcid());
                        flush_pcid(state, slot.user_pcid());
                    }
                }
            }
            state.cache.advance_current(TlbGen::new(generation));
        }
        self.outstanding.push(targets);
        // The flusher holds the address-space lock until every target acknowledges.
        self.spaces[space].busy = (targets != 0).then_some(flush);
    }

    fn deliver(&mut self, cpu: usize) {
        let state = &mut self.cpus[cpu];
        state.pending = false;
        if self.mutation != Mutation::AckWithoutCatchUp
            && let Some(loaded) = state.loaded.clone()
        {
            let space = &self.spaces[loaded.space];
            let from = state.cache.current_gen().unwrap().get();
            if from < space.generation {
                match logged_pages(&space.log, from, space.generation) {
                    Some(pages) => {
                        for page in pages {
                            invalidate_page(state, loaded.slot.kernel_pcid(), page);
                            invalidate_page(state, loaded.slot.user_pcid(), page);
                        }
                    }
                    None => {
                        flush_pcid(state, loaded.slot.kernel_pcid());
                        flush_pcid(state, loaded.slot.user_pcid());
                    }
                }
                state.cache.advance_current(TlbGen::new(space.generation));
            }
        }
        let bit = 1 << cpu;
        for (flush, waiting) in self.outstanding.iter_mut().enumerate() {
            if *waiting & bit != 0 {
                *waiting &= !bit;
                if *waiting == 0 {
                    for space in &mut self.spaces {
                        if space.busy == Some(flush) {
                            space.busy = None;
                        }
                    }
                }
            }
        }
    }

    fn recreate(&mut self, space: usize) {
        let instance = self.spaces[space].instance;
        for version in &mut self.versions {
            if version.owner == instance {
                version.state = VersionState::Dead;
            }
        }
        let reuse = (self.mutation == Mutation::ReuseCtxIds).then_some(self.spaces[space].ctx);
        let pages = self.spaces[space].pages.len();
        // No CPU has it loaded or is switching to it, so none is in `used_by`.
        assert_eq!(self.spaces[space].used_by, 0);
        self.spaces[space] = self.fresh_space(pages, reuse);
    }
}

/// The pages logged for generations `from` (exclusive) to `to` (inclusive), or `None` if a
/// record is missing (wrapped), full, or the total exceeds [`REPLAY_MAX`].
fn logged_pages(log: &VecDeque<(u64, Record)>, from: u64, to: u64) -> Option<Vec<usize>> {
    let mut pages = Vec::new();
    for generation in from + 1..=to {
        let (_, record) = log.iter().find(|(logged, _)| *logged == generation)?;
        match record {
            Record::Pages(logged) => pages.extend_from_slice(logged),
            Record::Full => return None,
        }
        if pages.len() > REPLAY_MAX {
            return None;
        }
    }
    Some(pages)
}

/// `INVLPG` or `INVPCID` type 0: the page's entry and the paging-structure cache entries in
/// `pcid`. (Intel drops all of the PCID's paging-structure cache entries; dropping only the
/// page's table is the weaker hardware, so the model assumes it.)
fn invalidate_page<const N: usize>(cpu: &mut Cpu<N>, pcid: u16, page: usize) {
    cpu.tlb.remove(&(pcid, page));
    cpu.psc.remove(&(pcid, page / TABLE_PAGES));
}

/// A CR3 load without NOFLUSH or `INVPCID` type 1: everything in `pcid`.
fn flush_pcid<const N: usize>(cpu: &mut Cpu<N>, pcid: u16) {
    cpu.tlb.retain(|&(tagged, _), _| tagged != pcid);
    cpu.psc.retain(|&(tagged, _), _| tagged != pcid);
}

/// xorshift64*, so runs are reproducible without dependencies.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const ALL_ACCESSES: [(Mode, bool); 4] = [
    (Mode::User, false),
    (Mode::User, true),
    (Mode::Kernel, false),
    (Mode::Kernel, true),
];

/// Runs `seeds` random sequences of `steps` operations; returns what they exercised, or the
/// first violation with its seed and step.
fn random<const N: usize>(
    mutation: Mutation,
    cpus: usize,
    spaces: usize,
    seeds: u64,
    steps: usize,
) -> Result<Stats, (u64, usize, Violation)> {
    let mut ops = Vec::new();
    let mut stats = Stats::default();
    for seed in 0..seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut machine = Machine::<N>::new(cpus, spaces, 4, mutation);
        for step in 0..steps {
            machine.enabled(&mut ops, &ALL_ACCESSES);
            // Favour accesses, so stale entries get used when they exist.
            let op = if rng.below(3) == 0 {
                let accesses: Vec<_> = ops
                    .iter()
                    .filter(|op| matches!(op, Op::Access { .. }))
                    .collect();
                if accesses.is_empty() {
                    ops[rng.below(ops.len())]
                } else {
                    *accesses[rng.below(accesses.len())]
                }
            } else {
                ops[rng.below(ops.len())]
            };
            if let Err(violation) = machine.step(op) {
                return Err((seed, step, violation));
            }
        }
        stats.add(machine.stats);
    }
    Ok(stats)
}

/// Explores every operation sequence up to `depth` from a small machine, breadth first,
/// skipping states already seen; returns the first violation and the number of states.
fn exhaustive<const N: usize>(
    mutation: Mutation,
    cpus: usize,
    spaces: usize,
    pages: usize,
    depth: usize,
    accesses: &[(Mode, bool)],
) -> (Option<(Vec<Op>, Violation)>, usize) {
    let start = Machine::<N>::new(cpus, spaces, pages, mutation);
    let mut seen = HashSet::from([start.clone()]);
    let mut frontier = vec![(start, Vec::new())];
    let mut ops = Vec::new();
    for _ in 0..depth {
        let mut next = Vec::new();
        for (machine, path) in &frontier {
            machine.enabled(&mut ops, accesses);
            for &op in &ops {
                let mut successor = machine.clone();
                let mut path = path.clone();
                path.push(op);
                if let Err(violation) = successor.step(op) {
                    return (Some((path, violation)), seen.len());
                }
                if seen.insert(successor.clone()) {
                    next.push((successor, path));
                }
            }
        }
        frontier = next;
    }
    (None, seen.len())
}

const MUTATIONS: [Mutation; 8] = [
    Mutation::SkipDowngradeBump,
    Mutation::NoflushOnMiss,
    Mutation::GenBeforeUsedBy,
    Mutation::SkipUserPcidFlush,
    Mutation::ReplayKernelPcidOnly,
    Mutation::ReuseCtxIds,
    Mutation::AckWithoutCatchUp,
    Mutation::SkipLocalInvalidate,
];

/// Random runs per configuration, and operations per run. The crate has no unsafe code, so
/// these tests are not run under Miri.
const SEEDS: u64 = 400;
const STEPS: usize = 400;

#[test]
fn random_sequences_never_use_a_stale_translation() {
    // Two slots for four address spaces forces evictions; six is the kernel's default.
    let stats = random::<2>(Mutation::None, 3, 4, SEEDS, STEPS).unwrap();
    random::<6>(Mutation::None, 3, 4, SEEDS, STEPS).unwrap();
    random::<1>(Mutation::None, 2, 3, SEEDS, STEPS).unwrap();
    // Every kind of switch happened, and CPUs did use stale entries while that was allowed,
    // so the oracle's interesting cases were reached.
    assert!(stats.hit > 0, "{stats:?}");
    assert!(stats.replay > 0, "{stats:?}");
    assert!(stats.stale > 0, "{stats:?}");
    assert!(stats.miss > 0, "{stats:?}");
    assert!(stats.ipis > 0, "{stats:?}");
    assert!(stats.stale_allowed > 0, "{stats:?}");
}

#[test]
fn random_sequences_catch_every_mutation() {
    for mutation in MUTATIONS {
        assert!(
            random::<2>(mutation, 3, 4, 100, STEPS).is_err(),
            "{mutation:?} was not caught"
        );
    }
}

/// Exhaustive search on a small machine: two CPUs, two address spaces of `pages` pages in one
/// table, and one slot, so every switch between them evicts.
fn small(
    mutation: Mutation,
    pages: usize,
    depth: usize,
    accesses: &[(Mode, bool)],
) -> (Option<(Vec<Op>, Violation)>, usize) {
    exhaustive::<1>(mutation, 2, 2, pages, depth, accesses)
}

#[test]
fn every_short_sequence_is_safe() {
    let (violation, states) = small(Mutation::None, 2, 6, &ALL_ACCESSES);
    assert_eq!(violation, None);
    assert!(states > 1);
    // Longer sequences over a smaller alphabet: one page, user-mode reads.
    let (violation, _) = small(Mutation::None, 1, 9, &[(Mode::User, false)]);
    assert_eq!(violation, None);
}

#[test]
fn exhaustive_search_catches_every_mutation() {
    // Each mutation has a counterexample of at most nine operations on this machine; the
    // search returns the shortest.
    for mutation in MUTATIONS {
        let (violation, _) = small(mutation, 1, 9, &[(Mode::User, false)]);
        assert!(violation.is_some(), "{mutation:?} was not caught");
    }
}
