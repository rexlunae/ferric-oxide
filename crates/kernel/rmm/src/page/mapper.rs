use core::{marker::PhantomData, num::NonZeroU8};

use crate::{
    Arch, FrameAllocator, PageEntry, PageFlags, PageFlush, PageTable, PhysicalAddress, TableKind,
    VirtualAddress,
};

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
        Arch, FrameAllocator, FrameCount, FrameUsage, PageFlags, PageMapper, PhysicalAddress,
        TableKind, VirtualAddress,
    };

    /// Hands out never-reused frames inside the emulator's 2 MiB physical map, so the mapper can
    /// reach every table it creates, and records every frame it gets back.
    struct RecordingAllocator {
        next: usize,
        freed: Vec<PhysicalAddress>,
    }

    // SAFETY: frames come from a bump pointer and are never handed out twice.
    unsafe impl FrameAllocator for RecordingAllocator {
        fn allocate(&mut self, count: FrameCount) -> Option<PhysicalAddress> {
            let base = self.next;
            self.next += count.data() * EmulateArch::PAGE_SIZE;
            assert!(
                self.next <= 2 * crate::MEGABYTE,
                "outside the emulator's physical map"
            );
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
        let mut allocator = RecordingAllocator {
            next: crate::MEGABYTE,
            freed: Vec::new(),
        };
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
}
