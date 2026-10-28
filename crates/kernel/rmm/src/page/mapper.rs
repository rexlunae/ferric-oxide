use core::{marker::PhantomData, num::NonZeroU8};

use crate::{
    Arch, FrameAllocator, PageEntry, PageFlags, PageFlush, PageTable, PhysicalAddress, TableKind,
    VirtualAddress,
};

/// A frame or page-table page that [`PageMapper::unmap_span_deferring_tables`],
/// [`PageMapper::map_phys_or_unlink`] or the undo of [`PageMapper::map_span_or_none`] removed
/// from a page table.
///
/// The mapper has unlinked it but not freed or invalidated anything: other CPUs may still cache
/// the translation or walks through the table, so the caller must not reuse either kind until a
/// TLB shootdown covering every CPU that may use the table has completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unmapped {
    /// The frame that an unmapped 4 KiB page mapped.
    Page(PhysicalAddress),
    /// A page-table page left empty and unlinked from its parent. It came from the mapper's
    /// frame allocator, and the caller now owns it.
    Table(PhysicalAddress),
}

/// Why [`PageMapper::map_span_or_none`] mapped nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MapSpanError {
    /// The index of the page that could not be mapped.
    pub page: usize,
}

impl core::fmt::Display for MapSpanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "page {} of the span needs a page table that could not be allocated",
            self.page
        )
    }
}

impl core::error::Error for MapSpanError {}

pub struct PageMapper<A, F> {
    table_kind: TableKind,
    table_addr: PhysicalAddress,
    allocator: F,
    // Ferric Oxide (KPTI): see `new_with_user_shadow`.
    user_shadow: bool,
    _phantom: PhantomData<fn() -> A>,
}

impl<A: Arch, F> PageMapper<A, F> {
    unsafe fn new(table_kind: TableKind, table_addr: PhysicalAddress, allocator: F) -> Self {
        Self {
            table_kind,
            table_addr,
            allocator,
            user_shadow: false,
            _phantom: PhantomData,
        }
    }

    /// Ferric Oxide (KPTI): a mapper for a top-level table that is followed, in the next page, by
    /// a shadow table loaded while user space runs. Every change to the user half of the table is
    /// made to the shadow too, so the two always map the same user memory.
    ///
    /// # Safety
    ///
    /// `table_addr` and the page after it must both be top-level tables of this address space,
    /// and the shadow's user half must start out identical to the table's.
    pub unsafe fn new_with_user_shadow(
        table_kind: TableKind,
        table_addr: PhysicalAddress,
        allocator: F,
    ) -> Self {
        // SAFETY: the caller upholds `new`'s requirements and the shadow's.
        let mut this = unsafe { Self::new(table_kind, table_addr, allocator) };
        this.user_shadow = true;
        this
    }

    pub unsafe fn current(table_kind: TableKind, allocator: F) -> Self {
        unsafe {
            let table_addr = A::table(table_kind);
            Self::new(table_kind, table_addr, allocator)
        }
    }

    pub fn is_current(&self) -> bool {
        self.table().phys() == A::table(self.table_kind)
    }

    pub unsafe fn make_current(&self) {
        unsafe {
            A::set_table(self.table_kind, self.table_addr);
        }
    }

    pub fn table(&self) -> PageTable<A> {
        // SAFETY: The only way to initialize a PageMapper is via new(), and we assume it upholds
        // all necessary invariants for this to be safe.
        unsafe {
            PageTable::new(VirtualAddress::new(0), self.table_addr, A::PAGE_LEVELS - 1)
                .with_user_shadow(self.user_shadow)
        }
    }

    pub fn allocator(&self) -> &F {
        &self.allocator
    }

    pub fn allocator_mut(&mut self) -> &mut F {
        &mut self.allocator
    }

    fn visit<T>(
        &self,
        virt: VirtualAddress,
        f: impl FnOnce(&mut PageTable<A>, usize) -> T,
    ) -> Option<T> {
        let mut table = self.table();
        loop {
            let i = table.index_of(virt)?;

            match (NonZeroU8::new(table.level().try_into().unwrap()), unsafe {
                table.entry(i)
            }) {
                // leaf
                (None, _) => return Some(f(&mut table, i)),
                // leaf (large/huge page)
                (Some(level), Some(entry))
                    if A::entry_flag_large(level).is_some_and(|f| f & entry.data() != 0) =>
                {
                    return Some(f(&mut table, i))
                }
                // intermediate; continue to the next-level paging structure
                _ => table = unsafe { table.next(i)? },
            }
        }
    }

    pub fn translate(&self, virt: VirtualAddress) -> Option<(PhysicalAddress, PageFlags<A>)> {
        let entry = self.visit(virt, |p1, i| unsafe { p1.entry(i) })??;
        Some((entry.address().ok()?, entry.flags()))
    }

    pub unsafe fn remap_with_full(
        &mut self,
        virt: VirtualAddress,
        f: impl FnOnce(PhysicalAddress, PageFlags<A>) -> Option<(PhysicalAddress, PageFlags<A>)>,
    ) -> Option<(PageFlags<A>, PhysicalAddress, PageFlush<A>)> {
        unsafe {
            self.visit(virt, |p1, i| {
                let old_entry = p1.entry(i)?;
                let old_phys = old_entry.address().ok()?;
                let old_flags = old_entry.flags();
                let (new_phys, new_flags) = f(old_phys, old_flags)?;
                // TODO: Higher-level PageEntry::new interface?
                // TODO: check this can't break alignment for large/huge pages
                let new_entry = PageEntry::new(new_phys.data(), new_flags.data());
                p1.set_entry(i, new_entry);
                Some((old_flags, old_phys, PageFlush::new(virt)))
            })
            .flatten()
        }
    }

    pub unsafe fn remap_with(
        &mut self,
        virt: VirtualAddress,
        map_flags: impl FnOnce(PageFlags<A>) -> PageFlags<A>,
    ) -> Option<(PageFlags<A>, PhysicalAddress, PageFlush<A>)> {
        unsafe {
            self.remap_with_full(virt, |same_phys, old_flags| {
                Some((same_phys, map_flags(old_flags)))
            })
        }
    }

    pub unsafe fn remap(
        &mut self,
        virt: VirtualAddress,
        flags: PageFlags<A>,
    ) -> Option<PageFlush<A>> {
        unsafe { self.remap_with(virt, |_| flags).map(|(_, _, flush)| flush) }
    }
}

impl<A: Arch, F: FrameAllocator> PageMapper<A, F> {
    pub unsafe fn create(table_kind: TableKind, mut allocator: F) -> Option<Self> {
        unsafe {
            let table_addr = allocator.allocate_one()?;
            let mut table = Self::new(table_kind, table_addr, allocator);

            match (table_kind, A::KERNEL_SEPARATE_TABLE) {
                (TableKind::Kernel, false) => {
                    // Pre-allocate all kernel top-level page table entries so that when
                    // the page table is copied, these entries are synced between processes.
                    for i in A::PAGE_ENTRIES / 2..A::PAGE_ENTRIES {
                        let phys = table
                            .allocator
                            .allocate_one()
                            .expect("failed to map page table");
                        let flags = A::ENTRY_FLAG_DEFAULT_TABLE;
                        table
                            .table()
                            .set_entry(i, PageEntry::new(phys.data(), flags));
                    }
                }
                (TableKind::User, false) => {
                    // Copy higher half (kernel) mappings
                    let active_ktable = PageMapper::current(TableKind::Kernel, ());
                    for i in A::PAGE_ENTRIES / 2..A::PAGE_ENTRIES {
                        if let Some(entry) = active_ktable.table().entry(i) {
                            table.table().set_entry(i, entry);
                        }
                    }
                }
                (_, true) => {
                    // There is a separate page table for the kernel. No need to copy the kernel
                    // mappings to the user page table.
                }
            }

            Some(table)
        }
    }

    pub unsafe fn map_phys(
        &mut self,
        virt: VirtualAddress,
        phys: PhysicalAddress,
        flags: PageFlags<A>,
        requested_level: u8,
    ) -> Option<PageFlush<A>> {
        unsafe {
            // TODO: verify flags have correct bits, and that these are valid for the specific
            // level

            let required_align = A::PAGE_SHIFT + usize::from(requested_level) * A::PAGE_ENTRY_SHIFT;
            assert_eq!(
                (virt.data() >> required_align) << required_align,
                virt.data(),
                "unaligned virtual address"
            );
            assert_eq!(
                (phys.data() >> required_align) << required_align,
                phys.data(),
                "unaligned physical address"
            );

            let mut entry = PageEntry::new(phys.data(), flags.data());
            if let Some(level_nz) = NonZeroU8::new(requested_level) {
                let flag = A::entry_flag_large(level_nz)
                    .expect("trying to map at a level not supported by hw");
                entry.set_flags(entry.flags().custom_flag(flag, true));
            }

            let mut table = self.table();
            loop {
                let i = table.index_of(virt)?;
                if table.level() == usize::from(requested_level) {
                    //TODO: check for overwriting entry
                    table.set_entry(i, entry);
                    return Some(PageFlush::new(virt));
                }

                if let Some(entry) = table.entry(i)
                    && let Some(flag) = NonZeroU8::new(table.level().try_into().unwrap())
                        .and_then(A::entry_flag_large)
                {
                    assert_eq!(
                        entry.flags().data() & flag,
                        0,
                        "trying to interpret huge page as subtable"
                    );
                }

                let next = match table.next(i) {
                    Some(some) => some,
                    None => {
                        let next_phys = self.allocator.allocate_one()?;
                        //TODO: correct flags?
                        let flags = A::ENTRY_FLAG_DEFAULT_TABLE
                            | if virt.kind() == TableKind::User {
                                A::ENTRY_FLAG_TABLE_USER
                            } else {
                                0
                            };
                        table.set_entry(i, PageEntry::new(next_phys.data(), flags));
                        table.next(i)?
                    }
                };
                table = next;
            }
        }
    }

    pub unsafe fn map_linearly(
        &mut self,
        phys: PhysicalAddress,
        flags: PageFlags<A>,
        requested_level: u8,
    ) -> Option<(VirtualAddress, PageFlush<A>)> {
        unsafe {
            let virt = A::phys_to_virt(phys);
            self.map_phys(virt, phys, flags, requested_level)
                .map(|flush| (virt, flush))
        }
    }

    /// Unmaps the page or huge page containing `virt` and returns its frame, level, flags and a
    /// flush for the local TLB.
    ///
    /// For user addresses (and every address where [`Arch::KERNEL_SEPARATE_TABLE`] is true), the
    /// page-table pages that the unmap leaves empty are unlinked and returned to the allocator
    /// immediately. Use [`Self::unmap_phys_deferring_tables`] when another CPU might still be
    /// using this table.
    ///
    /// # Safety
    ///
    /// Besides the usual requirements for changing the page table, no CPU may hold
    /// paging-structure-cache (page-walk cache) entries that point at a page-table page this call
    /// frees: those let a CPU keep walking through the freed frame after it has been reused. This
    /// holds when the table is not loaded on any other CPU, and for kernel-half addresses on
    /// architectures that share the kernel half (no page-table page is freed there).
    pub unsafe fn unmap_phys(
        &mut self,
        virt: VirtualAddress,
    ) -> Option<(PhysicalAddress, u8, PageFlags<A>, PageFlush<A>)> {
        let table = self.table();
        let allocator = &mut self.allocator;
        // SAFETY: the caller guarantees that no CPU still caches walks through freed tables.
        unsafe {
            unmap_phys_with(table, virt, &mut |table| {
                allocator.free_one(table);
            })
        }
    }

    /// Maps the 4 KiB pages `base + i * PAGE_SIZE` for `i` in `0..page_count`, page `i` to the
    /// frame `frame_for(self, i)` returns, skipping the pages it returns `None` for. `frame_for`
    /// gets the mapper so that it can look frames up in this same table.
    ///
    /// Either every page is mapped or none is. If a page needs a page table that the allocator
    /// cannot supply, the page tables allocated for that page and the pages already mapped are
    /// removed again, each reported to `unmapped` as by
    /// [`Self::unmap_span_deferring_tables`]. Nothing is freed, so this is safe on a table that
    /// other CPUs use.
    ///
    /// New mappings need no local TLB flush on the supported architectures, so the flushes
    /// [`Self::map_phys`] returns are dropped.
    ///
    /// # Errors
    ///
    /// [`MapSpanError`] when a page table could not be allocated (or the span leaves the table).
    ///
    /// # Safety
    ///
    /// The usual requirements for changing the page table apply, and every frame `frame_for`
    /// returns must be valid to map with `flags`. No page of the span may be mapped beforehand:
    /// the undo unmaps whatever is mapped there. `frame_for` must return the same answers when
    /// asked again, and `unmapped` must not access this mapper's table.
    pub unsafe fn map_span_or_none(
        &mut self,
        base: VirtualAddress,
        page_count: usize,
        flags: PageFlags<A>,
        mut frame_for: impl FnMut(&Self, usize) -> Option<PhysicalAddress>,
        unmapped: &mut impl FnMut(Unmapped),
    ) -> Result<(), MapSpanError> {
        for page in 0..page_count {
            let Some(phys) = frame_for(self, page) else {
                continue;
            };
            let virt = base.add(page * A::PAGE_SIZE);
            // SAFETY: forwarded from the caller.
            match unsafe { self.map_phys_or_unlink(virt, phys, flags, unmapped) } {
                // SAFETY: the page was not mapped, so no TLB holds a translation for it.
                Some(flush) => unsafe { flush.ignore() },
                None => {
                    // SAFETY: forwarded from the caller; nothing is freed.
                    unsafe { self.unmap_span_deferring_tables(base, page, unmapped) };
                    return Err(MapSpanError { page });
                }
            }
        }
        Ok(())
    }

    /// Maps the 4 KiB page at `virt` to `phys` as [`Self::map_phys`] does. If a page table it
    /// needs cannot be allocated, it unlinks the empty page tables on `virt`'s path, including
    /// those it linked before the allocation failed, and reports each to `unmapped` as an
    /// [`Unmapped::Table`] instead of leaving them in the table, where nothing would free them.
    ///
    /// # Safety
    ///
    /// As for [`Self::map_phys`]; `unmapped` must not access this mapper's table, and the caller
    /// must not reuse what it reports before the shootdown [`Unmapped`] describes.
    pub unsafe fn map_phys_or_unlink(
        &mut self,
        virt: VirtualAddress,
        phys: PhysicalAddress,
        flags: PageFlags<A>,
        unmapped: &mut impl FnMut(Unmapped),
    ) -> Option<PageFlush<A>> {
        // SAFETY: forwarded from the caller.
        let flush = unsafe { self.map_phys(virt, phys, flags, 0) };
        if flush.is_none() {
            // SAFETY: forwarded from the caller; the tables are only handed over.
            unsafe {
                prune_empty_tables(self.table(), virt, &mut |table| {
                    unmapped(Unmapped::Table(table))
                })
            };
        }
        flush
    }
}

impl<A: Arch, F> PageMapper<A, F> {
    /// Like [`Self::unmap_phys`], but does not free the page-table pages that the unmap empties.
    /// Each one is unlinked from its parent and then passed to `free_table`, which owns it from
    /// then on.
    ///
    /// Other CPUs that run on this table may still hold paging-structure-cache entries pointing
    /// at those pages until their TLBs are invalidated. The caller must therefore free them only
    /// after a TLB shootdown that covers every such CPU has completed, the same as for the
    /// unmapped frame itself.
    ///
    /// # Safety
    ///
    /// The usual requirements for changing the page table apply. `free_table` must not access
    /// this mapper's page table.
    pub unsafe fn unmap_phys_deferring_tables(
        &mut self,
        virt: VirtualAddress,
        free_table: &mut impl FnMut(PhysicalAddress),
    ) -> Option<(PhysicalAddress, u8, PageFlags<A>, PageFlush<A>)> {
        // SAFETY: forwarded from the caller; the tables are only handed over, not freed.
        unsafe { unmap_phys_with(self.table(), virt, free_table) }
    }

    /// Unmaps the 4 KiB pages `base + i * PAGE_SIZE` for `i` in `0..page_count` that are mapped,
    /// and reports each one's frame and each page-table page left empty to `unmapped`, as
    /// [`Self::unmap_phys_deferring_tables`] does for one page. A page's tables are reported
    /// before the page.
    ///
    /// # Safety
    ///
    /// The usual requirements for changing the page table apply. `unmapped` must not access this
    /// mapper's page table, and the caller must not reuse what it reports before the shootdown
    /// [`Unmapped`] describes.
    pub unsafe fn unmap_span_deferring_tables(
        &mut self,
        base: VirtualAddress,
        page_count: usize,
        unmapped: &mut impl FnMut(Unmapped),
    ) {
        for page in 0..page_count {
            let virt = base.add(page * A::PAGE_SIZE);
            // SAFETY: forwarded from the caller; the tables are only handed over.
            let Some((phys, _, _, flush)) = (unsafe {
                self.unmap_phys_deferring_tables(virt, &mut |table| {
                    unmapped(Unmapped::Table(table))
                })
            }) else {
                continue;
            };
            // SAFETY: the shootdown the caller runs for `Unmapped::Page` covers this CPU too.
            unsafe { flush.ignore() };
            unmapped(Unmapped::Page(phys));
        }
    }
}

/// Unlinks the page tables on `virt`'s path below `table` that hold no present entry, deepest
/// first, and hands each to `free_table`. Only the user half is pruned, as in
/// [`unmap_phys_with`]; `table` itself is never unlinked.
///
/// # Safety
///
/// The usual requirements for changing the page table apply; `free_table` must not access the
/// table.
unsafe fn prune_empty_tables<A: Arch>(
    mut table: PageTable<A>,
    virt: VirtualAddress,
    free_table: &mut impl FnMut(PhysicalAddress),
) {
    let Some(i) = table.index_of(virt) else {
        return;
    };
    if A::KERNEL_SEPARATE_TABLE || i < A::PAGE_ENTRIES / 2 {
        // SAFETY: forwarded from the caller.
        unsafe { prune_below(&mut table, virt, free_table) };
    }
}

/// [`prune_empty_tables`] without the user-half check: returns whether `table` is empty.
///
/// # Safety
///
/// As for [`prune_empty_tables`].
unsafe fn prune_below<A: Arch>(
    table: &mut PageTable<A>,
    virt: VirtualAddress,
    free_table: &mut impl FnMut(PhysicalAddress),
) -> bool {
    // SAFETY: `index_of` and `entry` stay within the table, and `next` follows only an entry
    // that is a table (not a leaf or huge page), as `PageMapper::visit` does.
    unsafe {
        let Some(i) = table.index_of(virt) else {
            return false;
        };
        let level = u8::try_from(table.level()).ok().and_then(NonZeroU8::new);
        let subtable = match (level, table.entry(i)) {
            (Some(lvl), Some(entry))
                if !A::entry_flag_large(lvl).is_some_and(|f| f & entry.data() != 0) =>
            {
                table.next(i)
            }
            _ => None,
        };
        if let Some(mut subtable) = subtable
            && prune_below(&mut subtable, virt, free_table)
        {
            // Unlink first, as `unmap_phys_inner` does.
            table.set_entry(i, PageEntry::new(0, 0));
            free_table(subtable.phys());
        }
        (0..A::PAGE_ENTRIES).all(|j| !table.entry(j).is_some_and(|e| e.present()))
    }
}

unsafe fn unmap_phys_with<A: Arch>(
    mut table: PageTable<A>,
    virt: VirtualAddress,
    free_table: &mut impl FnMut(PhysicalAddress),
) -> Option<(PhysicalAddress, u8, PageFlags<A>, PageFlush<A>)> {
    //TODO: verify virt is aligned
    let unmap_parents = A::KERNEL_SEPARATE_TABLE || table.index_of(virt)? < A::PAGE_ENTRIES / 2; // Is a userspace mapping

    unsafe {
        unmap_phys_inner(virt, &mut table, unmap_parents, free_table)
            // FIXME: may need to construct multiple page flushes if the level is higher
            .map(|(pa, lvl, pf)| (pa, lvl, pf, PageFlush::new(virt)))
    }
}

unsafe fn unmap_phys_inner<A: Arch>(
    virt: VirtualAddress,
    table: &mut PageTable<A>,
    unmap_parents: bool,
    free_table: &mut impl FnMut(PhysicalAddress),
) -> Option<(PhysicalAddress, u8, PageFlags<A>)> {
    unsafe {
        let i = table.index_of(virt)?;
        let entry_opt = table.entry(i);

        let mut subtable = match (NonZeroU8::new(table.level().try_into().unwrap()), entry_opt) {
            (None, _) => {
                table.set_entry(i, PageEntry::new(0, 0));
                let entry = entry_opt?;

                return Some((entry.address().ok()?, 0, entry.flags()));
            }
            (Some(lvl), Some(entry))
                if A::entry_flag_large(lvl).is_some_and(|f| f & entry.flags().data() != 0) =>
            {
                table.set_entry(i, PageEntry::new(0, 0));
                let entry = entry_opt?;

                return Some((entry.address().ok()?, lvl.get(), entry.flags()));
            }
            (Some(_), _) => table.next(i)?,
        };

        let res = unmap_phys_inner(virt, &mut subtable, unmap_parents, free_table)?;

        if unmap_parents {
            // TODO: Use a counter? This would reduce the remaining number of available bits, but could be
            // faster (benchmark is needed).
            let is_still_populated = (0..A::PAGE_ENTRIES)
                .map(|j| subtable.entry(j).expect("must be within bounds"))
                .any(|e| e.present());

            if !is_still_populated {
                // Unlink first, so the frame is unreachable from this table before it is handed
                // over; `free_table` may run a TLB shootdown and free it right away.
                table.set_entry(i, PageEntry::new(0, 0));
                free_table(subtable.phys());
            }
        }

        Some(res)
    }
}

impl<A, F: core::fmt::Debug> core::fmt::Debug for PageMapper<A, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageMapper")
            .field("frame", &self.table_addr)
            .field("allocator", &self.allocator)
            .finish()
    }
}

#[cfg(all(test, feature = "std", target_pointer_width = "64"))]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use crate::{
        emulate::{fresh_machine, EmulateArch},
        Arch, FrameAllocator, FrameCount, FrameUsage, MapSpanError, PageFlags, PageMapper,
        PhysicalAddress, TableKind, Unmapped, VirtualAddress,
    };

    /// Hands out never-reused frames from `next` up to `end`, inside the emulator's 2 MiB
    /// physical map so the mapper can reach every table it creates, and records every frame it
    /// gets back. Past `end` it is out of memory.
    struct RecordingAllocator {
        next: usize,
        end: usize,
        freed: Vec<PhysicalAddress>,
    }

    impl RecordingAllocator {
        fn new(start: usize, frames: usize) -> Self {
            let end = start + frames * EmulateArch::PAGE_SIZE;
            assert!(
                end <= 2 * crate::MEGABYTE,
                "outside the emulator's physical map"
            );
            Self {
                next: start,
                end,
                freed: Vec::new(),
            }
        }
    }

    // SAFETY: frames come from a bump pointer and are never handed out twice.
    unsafe impl FrameAllocator for RecordingAllocator {
        fn allocate(&mut self, count: FrameCount) -> Option<PhysicalAddress> {
            let base = self.next;
            let next = base + count.data() * EmulateArch::PAGE_SIZE;
            if next > self.end {
                return None;
            }
            self.next = next;
            Some(PhysicalAddress::new(base))
        }

        unsafe fn free(&mut self, address: PhysicalAddress, count: FrameCount) {
            assert_eq!(count.data(), 1);
            self.freed.push(address);
        }

        fn usage(&self) -> FrameUsage {
            FrameUsage::new(FrameCount::new(0), FrameCount::new(0))
        }
    }

    fn map(mapper: &mut PageMapper<EmulateArch, &mut RecordingAllocator>, virt: VirtualAddress) {
        let flags = PageFlags::<EmulateArch>::new().user(true).write(true);
        // SAFETY: the emulated user half is otherwise unused.
        let flush = unsafe { mapper.map_phys(virt, PhysicalAddress::new(0x1F_F000), flags, 0) }
            .expect("map user page");
        // SAFETY: nothing reads through this mapping.
        unsafe { flush.ignore() };
    }

    #[test]
    fn emptied_tables_are_deferred_or_freed() {
        let _machine = fresh_machine();
        let mut allocator = RecordingAllocator::new(crate::MEGABYTE, 256);
        // SAFETY: `init` loaded a valid table.
        let mut mapper = unsafe { PageMapper::current(TableKind::User, &mut allocator) };

        let a = VirtualAddress::new(0x1_0000_0000);
        let b = VirtualAddress::new(0x1_0000_1000);
        map(&mut mapper, a);
        map(&mut mapper, b);

        let mut deferred = Vec::new();
        // SAFETY: the mappings were created above and nothing uses them.
        let (phys, level, _, flush) =
            unsafe { mapper.unmap_phys_deferring_tables(a, &mut |t| deferred.push(t)) }
                .expect("a was mapped");
        // SAFETY: nothing reads through the mapping.
        unsafe { flush.ignore() };
        assert_eq!((phys, level), (PhysicalAddress::new(0x1F_F000), 0));
        assert!(deferred.is_empty(), "the PT still maps b");

        // SAFETY: as above.
        let (_, _, _, flush) =
            unsafe { mapper.unmap_phys_deferring_tables(b, &mut |t| deferred.push(t)) }
                .expect("b was mapped");
        // SAFETY: nothing reads through the mapping.
        unsafe { flush.ignore() };
        // PT, then PD, then PDPT, each unlinked before it is handed over.
        assert_eq!(deferred.len(), 3);
        // SAFETY: entry 0 of the top-level table is within bounds.
        assert!(unsafe { mapper.table().entry(0) }.is_some_and(|e| !e.present()));
        assert!(mapper.translate(b).is_none());
        assert!(mapper.allocator().freed.is_empty(), "nothing freed early");

        map(&mut mapper, a);
        // SAFETY: as above, and no other CPU exists.
        let (_, _, _, flush) = unsafe { mapper.unmap_phys(a) }.expect("a was mapped");
        // SAFETY: nothing reads through the mapping.
        unsafe { flush.ignore() };
        assert_eq!(
            mapper.allocator().freed.len(),
            3,
            "unmap_phys frees at once"
        );
        assert!(mapper
            .allocator()
            .freed
            .iter()
            .all(|f| !deferred.contains(f)));
    }

    const PAGE: usize = EmulateArch::PAGE_SIZE;

    fn user_flags() -> PageFlags<EmulateArch> {
        PageFlags::<EmulateArch>::new().user(true).write(true)
    }

    /// A data frame for page `i`. Below the allocators' ranges; nothing reads it.
    fn frame(i: usize) -> PhysicalAddress {
        PhysicalAddress::new(0x8_0000 + i * PAGE)
    }

    fn map_frame(
        mapper: &mut PageMapper<EmulateArch, &mut RecordingAllocator>,
        virt: VirtualAddress,
        phys: PhysicalAddress,
    ) {
        // SAFETY: the emulated user half is otherwise unused.
        let flush = unsafe { mapper.map_phys(virt, phys, user_flags(), 0) }.expect("map page");
        // SAFETY: nothing reads through this mapping.
        unsafe { flush.ignore() };
    }

    fn page_at(base: VirtualAddress, i: usize) -> VirtualAddress {
        base.add(i * PAGE)
    }

    #[test]
    fn map_span_maps_every_source_page_and_skips_holes() {
        let _machine = fresh_machine();
        let mut src_alloc = RecordingAllocator::new(crate::MEGABYTE, 64);
        let mut dst_alloc = RecordingAllocator::new(crate::MEGABYTE + 64 * PAGE, 64);
        // SAFETY: `init` loaded a valid table.
        let mut src = unsafe { PageMapper::current(TableKind::User, &mut src_alloc) };
        // SAFETY: the new table is only used through this mapper.
        let mut dst = unsafe { PageMapper::create(TableKind::User, &mut dst_alloc) }
            .expect("allocate the destination table");

        let src_base = VirtualAddress::new(0x1_0000_0000);
        let dst_base = VirtualAddress::new(0x2_0000_0000);
        map_frame(&mut src, page_at(src_base, 0), frame(0));
        map_frame(&mut src, page_at(src_base, 2), frame(2));

        let mut unmapped = Vec::new();
        // SAFETY: the destination span is unmapped and nothing else uses the table.
        let result = unsafe {
            dst.map_span_or_none(
                dst_base,
                3,
                user_flags(),
                |_, i| src.translate(page_at(src_base, i)).map(|(phys, _)| phys),
                &mut |u| unmapped.push(u),
            )
        };

        assert_eq!(result, Ok(()));
        assert!(unmapped.is_empty());
        assert_eq!(
            dst.translate(page_at(dst_base, 0)).map(|m| m.0),
            Some(frame(0))
        );
        assert!(
            dst.translate(page_at(dst_base, 1)).is_none(),
            "the hole stays unmapped"
        );
        assert_eq!(
            dst.translate(page_at(dst_base, 2)).map(|m| m.0),
            Some(frame(2))
        );
    }

    /// `Grant::transfer` in the kernel: the destination runs out of frames for its page tables
    /// part way through the span. Nothing may change in either address space, and every page
    /// and table the undo removed must be handed back for the deferred shootdown, not freed.
    #[test]
    fn map_span_out_of_frames_leaves_both_tables_unchanged() {
        let _machine = fresh_machine();
        let mut src_alloc = RecordingAllocator::new(crate::MEGABYTE, 64);
        // The top-level table, then PDPT, PD and PT for pages 0 and 1, then the PD for pages 2
        // and 3, which sit past a 1 GiB boundary. Their PT does not fit.
        let mut dst_alloc = RecordingAllocator::new(crate::MEGABYTE + 64 * PAGE, 5);
        // SAFETY: `init` loaded a valid table.
        let mut src = unsafe { PageMapper::current(TableKind::User, &mut src_alloc) };
        // SAFETY: the new table is only used through this mapper.
        let mut dst = unsafe { PageMapper::create(TableKind::User, &mut dst_alloc) }
            .expect("allocate the destination table");

        let src_base = VirtualAddress::new(0x1_0000_0000);
        let dst_base = VirtualAddress::new(0x8000_0000 - 2 * PAGE);
        for i in 0..4 {
            map_frame(&mut src, page_at(src_base, i), frame(i));
        }

        let mut unmapped = Vec::new();
        // SAFETY: the destination span is unmapped and nothing else uses the table.
        let result = unsafe {
            dst.map_span_or_none(
                dst_base,
                4,
                user_flags(),
                |_, i| src.translate(page_at(src_base, i)).map(|(phys, _)| phys),
                &mut |u| unmapped.push(u),
            )
        };

        assert_eq!(result, Err(MapSpanError { page: 2 }));
        for i in 0..4 {
            assert!(
                dst.translate(page_at(dst_base, i)).is_none(),
                "page {i} undone"
            );
            assert_eq!(
                src.translate(page_at(src_base, i)).map(|m| m.0),
                Some(frame(i)),
                "source page {i} untouched"
            );
        }
        // SAFETY: entry 0 of the top-level table is within bounds.
        assert!(unsafe { dst.table().entry(0) }.is_some_and(|e| !e.present()));

        let tables: Vec<_> = unmapped
            .iter()
            .filter_map(|u| match u {
                Unmapped::Table(t) => Some(*t),
                Unmapped::Page(_) => None,
            })
            .collect();
        let pages: Vec<_> = unmapped
            .iter()
            .filter_map(|u| match u {
                Unmapped::Page(p) => Some(*p),
                Unmapped::Table(_) => None,
            })
            .collect();
        assert_eq!(pages, [frame(0), frame(1)]);
        // Every table but the top level, including the PD linked for page 2 before its PT
        // failed, is handed back once.
        let mut expected: Vec<_> = (1..5)
            .map(|i| PhysicalAddress::new(crate::MEGABYTE + (64 + i) * PAGE))
            .collect();
        let mut tables_sorted = tables.clone();
        tables_sorted.sort();
        expected.sort();
        assert_eq!(tables_sorted, expected);
        assert!(
            dst.allocator().freed.is_empty(),
            "nothing freed before the shootdown"
        );
    }

    /// The destination runs out of frames while mapping the first page, before any page is
    /// mapped: there is no page to undo, but the tables linked for it are handed back.
    #[test]
    fn map_span_out_of_frames_on_the_first_page() {
        let _machine = fresh_machine();
        let mut src_alloc = RecordingAllocator::new(crate::MEGABYTE, 64);
        // The top-level table, then the PDPT and PD for page 0. Its PT does not fit.
        let mut dst_alloc = RecordingAllocator::new(crate::MEGABYTE + 64 * PAGE, 3);
        // SAFETY: `init` loaded a valid table.
        let mut src = unsafe { PageMapper::current(TableKind::User, &mut src_alloc) };
        // SAFETY: the new table is only used through this mapper.
        let mut dst = unsafe { PageMapper::create(TableKind::User, &mut dst_alloc) }
            .expect("allocate the destination table");

        let src_base = VirtualAddress::new(0x1_0000_0000);
        let dst_base = VirtualAddress::new(0x2_0000_0000);
        for i in 0..2 {
            map_frame(&mut src, page_at(src_base, i), frame(i));
        }

        let mut unmapped = Vec::new();
        // SAFETY: the destination span is unmapped and nothing else uses the table.
        let result = unsafe {
            dst.map_span_or_none(
                dst_base,
                2,
                user_flags(),
                |_, i| src.translate(page_at(src_base, i)).map(|(phys, _)| phys),
                &mut |u| unmapped.push(u),
            )
        };

        assert_eq!(result, Err(MapSpanError { page: 0 }));
        for i in 0..2 {
            assert!(dst.translate(page_at(dst_base, i)).is_none());
            assert_eq!(
                src.translate(page_at(src_base, i)).map(|m| m.0),
                Some(frame(i)),
                "source page {i} untouched"
            );
        }
        let mut tables: Vec<_> = unmapped
            .iter()
            .map(|u| match u {
                Unmapped::Table(t) => *t,
                Unmapped::Page(p) => panic!("no page was mapped, but {p:?} was undone"),
            })
            .collect();
        tables.sort();
        let expected: Vec<_> = (1..3)
            .map(|i| PhysicalAddress::new(crate::MEGABYTE + (64 + i) * PAGE))
            .collect();
        assert_eq!(tables, expected);
        assert!(dst.allocator().freed.is_empty());
    }

    /// Moving a page within one table, the way `Grant::transfer` does it: the destination is
    /// mapped before the source is unmapped, so a page table both share is never left empty
    /// (and handed back) in between, and the move needs no frame once the mapping succeeded.
    #[test]
    fn move_within_one_table_keeps_the_shared_page_table() {
        let _machine = fresh_machine();
        // Only the PDPT, PD and PT for the source page: the move itself may allocate nothing.
        let mut allocator = RecordingAllocator::new(crate::MEGABYTE, 3);
        // SAFETY: `init` loaded a valid table.
        let mut mapper = unsafe { PageMapper::current(TableKind::User, &mut allocator) };

        let src_page = VirtualAddress::new(0x1_0000_0000);
        let dst_page = VirtualAddress::new(0x1_0000_1000);
        map_frame(&mut mapper, src_page, frame(0));

        let mut unmapped = Vec::new();
        // SAFETY: the destination page is unmapped and nothing else uses the table.
        let result = unsafe {
            mapper.map_span_or_none(
                dst_page,
                1,
                user_flags(),
                |m, _| m.translate(src_page).map(|(phys, _)| phys),
                &mut |u| unmapped.push(u),
            )
        };
        assert_eq!(result, Ok(()));
        // SAFETY: as above.
        unsafe { mapper.unmap_span_deferring_tables(src_page, 1, &mut |u| unmapped.push(u)) };

        assert_eq!(unmapped, [Unmapped::Page(frame(0))], "no table emptied");
        assert_eq!(mapper.translate(dst_page).map(|m| m.0), Some(frame(0)));
        assert!(mapper.translate(src_page).is_none());
    }
}
