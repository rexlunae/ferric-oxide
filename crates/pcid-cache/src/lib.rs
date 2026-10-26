//! `ferric-pcid-cache`: the per-CPU cache of PCID slots behind per-address-space TLB tagging on
//! x86-64, `no_std`.
//!
//! With CR4.PCIDE set, the CPU tags every TLB entry with the 12-bit PCID loaded in CR3, and a
//! CR3 write with bit 63 set (NOFLUSH) keeps the entries of the PCID it loads. Each CPU keeps a
//! few PCIDs, its *slots*, for the address spaces it ran last, so switching back to one of them
//! need not flush its translations. This crate makes the decisions; the kernel does the CR3
//! writes and invalidations they call for. It is not wired into the kernel yet.
//!
//! The pieces:
//!
//! - [`CtxId`]: a never-reused 64-bit name for an address space. A recycled `Arc` or table
//!   address could inherit another address space's slot; a [`CtxId`] cannot.
//! - [`TlbGen`]: an address space's TLB generation. Every flush that invalidates anything in the
//!   address space bumps it, so a slot whose generation is at least the address space's holds no
//!   stale translation for it.
//! - [`SlotCache`]: one CPU's slots ([`NR_SLOTS`] by default). [`SlotCache::switch_to`] picks the
//!   slot to load and says whether the load may keep its entries ([`SwitchKind`]).
//! - [`SlotIndex`]: a slot and its PCIDs. Slot *i* uses kernel PCID *i* + 1 and, with KPTI,
//!   user PCID `0x800 | (i + 1)`, which is exactly what the KPTI entry and exit code compute by
//!   toggling bit 11. PCID 0 is left to the boot and idle tables.
//!
//! # Protocol
//!
//! The cache is only correct as part of this protocol, which `tests/model.rs` checks against a
//! model of a PCID-tagged TLB:
//!
//! - **Switch-in** (interrupts off): set this CPU in the address space's `used_by`, fence
//!   (SeqCst), read its [`TlbGen`], then call [`SlotCache::switch_to`] and load CR3 as the
//!   returned [`Switch`] says. The generation must be read *after* `used_by` is set: a flush
//!   that bumps it later then sees this CPU and interrupts it.
//! - **Flush**: after changing page tables, bump the generation (and log the pages), fence,
//!   read `used_by` and interrupt those CPUs. A CPU that has the address space loaded and
//!   invalidated everything up to the new generation calls [`SlotCache::advance_current`].
//! - **Shootdown receiver**: catch the current slot up from [`SlotCache::current_gen`] to the
//!   address space's generation (replay or full flush of its PCIDs), then call
//!   [`SlotCache::advance_current`] and acknowledge.
//!
//! A slot left behind by an address space that is not loaded keeps its stale entries, but no
//! CPU can use them: translations are looked up only under the loaded PCID, and loading the slot
//! again either catches it up first or flushes it.
//!
//! Sources: Intel SDM Vol. 3A §4.10 (PCIDs, `INVPCID`, `INVLPG`) and AMD APM Vol. 2 §5.5. The
//! slot-and-generation scheme follows public descriptions of Linux's x86 ASID handling; no Linux
//! code was read.
//!
//! # Examples
//!
//! ```
//! use ferric_pcid_cache::{CtxId, SlotCache, SwitchKind, TlbGen};
//!
//! let mut cache: SlotCache = SlotCache::new();
//! let a = CtxId::new(1).unwrap();
//! let b = CtxId::new(2).unwrap();
//! let no_log = |_, _| false;
//!
//! // First run of each address space on this CPU: a flushing load into a fresh slot.
//! let first = cache.switch_to(a, TlbGen::INITIAL, no_log);
//! assert_eq!(first.kind, SwitchKind::Miss { evicted: None });
//! assert!(first.flush());
//! cache.switch_to(b, TlbGen::INITIAL, no_log);
//!
//! // Back to `a`, unchanged since: its entries are kept.
//! let again = cache.switch_to(a, TlbGen::INITIAL, no_log);
//! assert_eq!(again.kind, SwitchKind::Hit);
//! assert_eq!(again.slot, first.slot);
//! assert!(!again.flush());
//! assert_eq!(again.slot.kernel_pcid(), 1);
//! assert_eq!(again.slot.user_pcid(), 0x801);
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::num::NonZeroU64;

/// Slots per CPU by default. A CPU's hot set (a client, the file system, a driver, a daemon or
/// two) fits, and the TLB could not keep more address spaces warm anyway.
pub const NR_SLOTS: usize = 6;

/// The PCID bit that marks a KPTI shadow (user) table. Every kernel PCID is below it.
pub const USER_PCID_BIT: u16 = 0x800;

/// The largest slot count: kernel PCIDs `1..=N` must stay below [`USER_PCID_BIT`], and the
/// round-robin pointer is a `u8`.
pub const MAX_SLOTS: usize = 255;

/// A never-reused name for an address space, from a global counter that starts at 1.
///
/// At 10^9 new address spaces a second the counter lasts 584 years, so it never wraps in
/// practice and two address spaces never share a name, unlike their `Arc` pointers or table
/// addresses, which are reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CtxId(NonZeroU64);

impl CtxId {
    /// The context id `value`, or `None` for 0, which no address space has.
    #[inline]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// The id as a number.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// An address space's TLB generation: how many invalidating flushes it has had, plus one.
///
/// A slot records the generation its entries are current to. An address space's generation
/// only grows, and generations of different address spaces are unrelated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TlbGen(u64);

impl TlbGen {
    /// The generation of a new address space.
    pub const INITIAL: Self = Self(1);

    /// The generation `value`.
    #[inline]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The generation as a number.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One of a CPU's slots, and the PCIDs it loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotIndex(u8);

impl SlotIndex {
    /// The slot's position, below the cache's slot count.
    #[inline]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    /// The PCID of the slot's kernel table: the position plus one, as PCID 0 belongs to the boot
    /// and idle tables. Without KPTI this is the slot's only PCID.
    #[inline]
    pub const fn kernel_pcid(self) -> u16 {
        self.0 as u16 + 1
    }

    /// The PCID of the slot's KPTI shadow table: the kernel PCID with [`USER_PCID_BIT`] set.
    #[inline]
    pub const fn user_pcid(self) -> u16 {
        self.kernel_pcid() | USER_PCID_BIT
    }
}

/// What a switch-in found in the cache, and so how CR3 must be loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SwitchKind {
    /// The slot holds this address space and is current: load with NOFLUSH.
    Hit,
    /// The slot holds this address space but is behind, and the invalidation log covers the gap:
    /// load with NOFLUSH, then invalidate every page logged for generations `from` (exclusive)
    /// to `to` (inclusive) under the slot's PCIDs, as the caller's replay callback promised.
    Replay {
        /// The slot's generation before the switch.
        from: TlbGen,
        /// The address space's generation, which the slot now records.
        to: TlbGen,
    },
    /// The slot holds this address space but is behind further than the log reaches: load with
    /// a flush (and, with KPTI, `INVPCID` type 1 on the user PCID).
    Stale {
        /// The slot's generation before the switch.
        from: TlbGen,
        /// The address space's generation, which the slot now records.
        to: TlbGen,
    },
    /// No slot holds this address space: the next victim is reused for it, so load with a
    /// flush (and, with KPTI, `INVPCID` type 1 on the user PCID).
    Miss {
        /// The address space whose slot was taken, if the slot was in use.
        evicted: Option<CtxId>,
    },
}

/// The outcome of [`SlotCache::switch_to`]: which slot to load, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Switch {
    /// The slot to load: CR3 gets the table with [`SlotIndex::kernel_pcid`].
    pub slot: SlotIndex,
    /// What the cache found, which decides [`Self::flush`].
    pub kind: SwitchKind,
}

impl Switch {
    /// Whether the CR3 write must flush the slot's PCIDs (bit 63 clear, plus `INVPCID` type 1 on
    /// the user PCID with KPTI). If not, it must set bit 63 (NOFLUSH).
    #[inline]
    pub const fn flush(&self) -> bool {
        matches!(
            self.kind,
            SwitchKind::Stale { .. } | SwitchKind::Miss { .. }
        )
    }
}

/// What one slot holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Slot {
    ctx: CtxId,
    /// Every invalidation of `ctx` up to this generation has been applied to the slot's PCIDs.
    generation: TlbGen,
}

/// One CPU's PCID slots: which address space each holds, how current it is, and which one is
/// loaded.
///
/// Only its own CPU touches it, with interrupts off, so it needs no locks or atomics.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SlotCache<const N: usize = NR_SLOTS> {
    slots: [Option<Slot>; N],
    current: Option<SlotIndex>,
    next_victim: u8,
}

impl<const N: usize> SlotCache<N> {
    const VALID_SIZE: () = assert!(N >= 1 && N <= MAX_SLOTS, "1 to MAX_SLOTS slots");

    /// An empty cache, with nothing loaded.
    #[inline]
    pub const fn new() -> Self {
        let () = Self::VALID_SIZE;
        Self {
            slots: [None; N],
            current: None,
            next_victim: 0,
        }
    }

    /// Picks the slot for a switch to address space `ctx`, whose `generation` was read after
    /// this CPU was added to its `used_by`, and records it as loaded.
    ///
    /// `replay(from, to)` is called only when the slot holds `ctx` but is behind. It returns
    /// whether the caller can invalidate every page the log holds for generations `from`
    /// (exclusive) to `to` (inclusive) once CR3 is loaded: it must have copied them (and checked
    /// they fit its replay limit) before returning `true`. If it returns `false`, the slot is
    /// flushed instead.
    ///
    /// The caller must then load CR3 as the result says ([`Switch::flush`]).
    pub fn switch_to(
        &mut self,
        ctx: CtxId,
        generation: TlbGen,
        replay: impl FnOnce(TlbGen, TlbGen) -> bool,
    ) -> Switch {
        let (index, kind) = match self.find(ctx) {
            Some(index) => {
                let slot = self.slot_mut(index);
                let from = slot.generation;
                let kind = if from >= generation {
                    SwitchKind::Hit
                } else if replay(from, generation) {
                    SwitchKind::Replay {
                        from,
                        to: generation,
                    }
                } else {
                    SwitchKind::Stale {
                        from,
                        to: generation,
                    }
                };
                // Monotonic: a hit never moves the slot back.
                slot.generation = slot.generation.max(generation);
                (index, kind)
            }
            None => {
                let index = self.take_victim();
                let evicted = self.slots[index.get()].map(|slot| slot.ctx);
                // The slot's old generation counted another address space's flushes; the flush
                // of this load makes it current to `generation` exactly, never more.
                self.slots[index.get()] = Some(Slot { ctx, generation });
                (index, SwitchKind::Miss { evicted })
            }
        };
        self.current = Some(index);
        Switch { slot: index, kind }
    }

    /// Records a switch to no address space (the empty table, under PCID 0). The slots keep
    /// their contents.
    #[inline]
    pub fn switch_to_none(&mut self) {
        self.current = None;
    }

    /// The loaded slot, if an address space is loaded.
    #[inline]
    pub fn current(&self) -> Option<SlotIndex> {
        self.current
    }

    /// The loaded address space.
    #[inline]
    pub fn current_ctx(&self) -> Option<CtxId> {
        self.current.map(|index| self.slot(index).ctx)
    }

    /// The generation the loaded slot is current to: where a shootdown receiver starts catching
    /// up from.
    #[inline]
    pub fn current_gen(&self) -> Option<TlbGen> {
        self.current.map(|index| self.slot(index).generation)
    }

    /// Records that every invalidation of the loaded address space up to `generation` has been applied
    /// to the loaded slot's PCIDs, by a shootdown receiver's catch-up or by the flushing CPU
    /// itself. Never moves the slot's generation back; does nothing while nothing is loaded.
    #[inline]
    pub fn advance_current(&mut self, generation: TlbGen) {
        if let Some(index) = self.current {
            let slot = self.slot_mut(index);
            slot.generation = slot.generation.max(generation);
        }
    }

    /// What slot `index` holds: its address space and generation, or `None` if it is empty or
    /// out of range.
    #[inline]
    pub fn get(&self, index: usize) -> Option<(CtxId, TlbGen)> {
        self.slots
            .get(index)
            .copied()
            .flatten()
            .map(|slot| (slot.ctx, slot.generation))
    }

    /// The slot holding `ctx`.
    fn find(&self, ctx: CtxId) -> Option<SlotIndex> {
        self.slots
            .iter()
            .position(|slot| slot.is_some_and(|slot| slot.ctx == ctx))
            .map(Self::index)
    }

    /// The next slot to reuse, round-robin.
    fn take_victim(&mut self) -> SlotIndex {
        let index = Self::index(usize::from(self.next_victim));
        self.next_victim = if usize::from(self.next_victim) + 1 == N {
            0
        } else {
            self.next_victim + 1
        };
        index
    }

    /// A [`SlotIndex`] for a position below `N`.
    fn index(position: usize) -> SlotIndex {
        debug_assert!(position < N);
        // `N <= MAX_SLOTS` (checked in `new`), so every position fits.
        SlotIndex(position as u8)
    }

    /// The slot at `index`, which [`SlotCache`] only hands out for slots in use.
    fn slot(&self, index: SlotIndex) -> &Slot {
        self.slots[index.get()]
            .as_ref()
            .expect("the current slot is always in use")
    }

    fn slot_mut(&mut self, index: SlotIndex) -> &mut Slot {
        self.slots[index.get()]
            .as_mut()
            .expect("a found or current slot is always in use")
    }
}

impl<const N: usize> Default for SlotCache<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(n: u64) -> CtxId {
        CtxId::new(n).unwrap()
    }

    const NO_LOG: fn(TlbGen, TlbGen) -> bool = |_, _| false;

    #[test]
    fn hit_keeps_entries() {
        let mut cache = SlotCache::<2>::new();
        let first = cache.switch_to(ctx(1), TlbGen::new(3), NO_LOG);
        assert_eq!(first.kind, SwitchKind::Miss { evicted: None });
        assert!(first.flush());
        cache.switch_to_none();
        assert_eq!(cache.current(), None);
        let again = cache.switch_to(ctx(1), TlbGen::new(3), |_, _| panic!("no replay on a hit"));
        assert_eq!(
            again,
            Switch {
                slot: first.slot,
                kind: SwitchKind::Hit
            }
        );
        assert!(!again.flush());
        assert_eq!(cache.current_ctx(), Some(ctx(1)));
        assert_eq!(cache.current_gen(), Some(TlbGen::new(3)));
    }

    #[test]
    fn behind_replays_or_flushes() {
        let mut cache = SlotCache::<2>::new();
        cache.switch_to(ctx(1), TlbGen::new(3), NO_LOG);
        let mut asked = None;
        let replay = cache.switch_to(ctx(1), TlbGen::new(5), |from, to| {
            asked = Some((from, to));
            true
        });
        assert_eq!(asked, Some((TlbGen::new(3), TlbGen::new(5))));
        assert_eq!(
            replay.kind,
            SwitchKind::Replay {
                from: TlbGen::new(3),
                to: TlbGen::new(5)
            }
        );
        assert!(!replay.flush());
        assert_eq!(cache.current_gen(), Some(TlbGen::new(5)));

        let stale = cache.switch_to(ctx(1), TlbGen::new(9), NO_LOG);
        assert_eq!(
            stale.kind,
            SwitchKind::Stale {
                from: TlbGen::new(5),
                to: TlbGen::new(9)
            }
        );
        assert!(stale.flush());
        assert_eq!(cache.current_gen(), Some(TlbGen::new(9)));
    }

    #[test]
    fn round_robin_eviction() {
        let mut cache = SlotCache::<3>::new();
        let slots: [_; 3] = core::array::from_fn(|i| {
            cache
                .switch_to(ctx(i as u64 + 1), TlbGen::INITIAL, NO_LOG)
                .slot
                .get()
        });
        assert_eq!(slots, [0, 1, 2]);
        // The fourth takes slot 0, from the first.
        let fourth = cache.switch_to(ctx(4), TlbGen::INITIAL, NO_LOG);
        assert_eq!(fourth.slot.get(), 0);
        assert_eq!(
            fourth.kind,
            SwitchKind::Miss {
                evicted: Some(ctx(1))
            }
        );
        // The first is gone, so it misses and takes slot 1, the next victim.
        let first = cache.switch_to(ctx(1), TlbGen::INITIAL, NO_LOG);
        assert_eq!(first.slot.get(), 1);
        assert_eq!(
            first.kind,
            SwitchKind::Miss {
                evicted: Some(ctx(2))
            }
        );
        assert_eq!(cache.get(1), Some((ctx(1), TlbGen::INITIAL)));
        assert_eq!(cache.get(3), None);
    }

    #[test]
    fn miss_takes_the_new_generation_not_the_evicted_one() {
        // Generations of different address spaces are unrelated. Keeping the larger of the
        // evicted owner's and the new one would mark invalidations as applied that never were.
        let mut cache = SlotCache::<1>::new();
        cache.switch_to(ctx(1), TlbGen::new(100), NO_LOG);
        cache.switch_to(ctx(2), TlbGen::new(2), NO_LOG);
        assert_eq!(cache.current_gen(), Some(TlbGen::new(2)));
        let behind = cache.switch_to(ctx(2), TlbGen::new(3), NO_LOG);
        assert_eq!(
            behind.kind,
            SwitchKind::Stale {
                from: TlbGen::new(2),
                to: TlbGen::new(3)
            }
        );
    }

    #[test]
    fn the_loaded_slot_can_be_the_victim() {
        // Switching away from the only slot's owner reuses its slot: the load flushes it.
        let mut cache = SlotCache::<1>::new();
        cache.switch_to(ctx(1), TlbGen::INITIAL, NO_LOG);
        let next = cache.switch_to(ctx(2), TlbGen::INITIAL, NO_LOG);
        assert_eq!(
            next.kind,
            SwitchKind::Miss {
                evicted: Some(ctx(1))
            }
        );
        assert_eq!(cache.current_ctx(), Some(ctx(2)));
    }

    #[test]
    fn advance_is_monotonic_and_only_for_the_loaded_slot() {
        let mut cache = SlotCache::<2>::new();
        cache.advance_current(TlbGen::new(7));
        cache.switch_to(ctx(1), TlbGen::new(4), NO_LOG);
        cache.advance_current(TlbGen::new(6));
        assert_eq!(cache.current_gen(), Some(TlbGen::new(6)));
        cache.advance_current(TlbGen::new(5));
        assert_eq!(cache.current_gen(), Some(TlbGen::new(6)));
        cache.switch_to_none();
        cache.advance_current(TlbGen::new(9));
        assert_eq!(cache.get(0), Some((ctx(1), TlbGen::new(6))));
    }

    #[test]
    fn pcids() {
        let slots = SlotCache::<MAX_SLOTS>::new();
        assert_eq!(slots.current(), None);
        let first = SlotCache::<MAX_SLOTS>::index(0);
        let last = SlotCache::<MAX_SLOTS>::index(MAX_SLOTS - 1);
        assert_eq!((first.kernel_pcid(), first.user_pcid()), (1, 0x801));
        // Every kernel PCID stays below the user bit, so the KPTI entry and exit code, which
        // toggle bit 11, pair each slot's PCIDs.
        assert!(last.kernel_pcid() < USER_PCID_BIT);
        assert_eq!(last.user_pcid() & !USER_PCID_BIT, last.kernel_pcid());
        assert_eq!(CtxId::new(0), None);
    }
}
