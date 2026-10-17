use core::{fmt, marker::PhantomData};

use crate::{page::PageEntry, Arch, PhysicalAddress, VirtualAddress};

pub struct PageTable<A> {
    base: VirtualAddress,
    phys: PhysicalAddress,
    level: usize,
    // Ferric Oxide (KPTI): the page after this top-level table is a second table whose user half
    // must stay identical to this one's, so writes to the user half go to both.
    user_shadow: bool,
    phantom: PhantomData<A>,
}

impl<A: Arch> PageTable<A> {
    pub(super) unsafe fn new(base: VirtualAddress, phys: PhysicalAddress, level: usize) -> Self {
        Self {
            base,
            phys,
            level,
            user_shadow: false,
            phantom: PhantomData,
        }
    }

    /// Ferric Oxide (KPTI): marks a top-level table as followed by a shadow table whose user half
    /// mirrors this one. Writes to entries in the user half then update both tables.
    ///
    /// # Safety
    ///
    /// The page right after this table must be a top-level table owned by the same address space.
    pub(super) unsafe fn with_user_shadow(mut self, user_shadow: bool) -> Self {
        self.user_shadow = user_shadow && self.level == A::PAGE_LEVELS - 1;
        self
    }

    pub fn base(&self) -> VirtualAddress {
        self.base
    }

    pub fn phys(&self) -> PhysicalAddress {
        self.phys
    }

    pub fn level(&self) -> usize {
        self.level
    }

    pub fn entry_base(&self, i: usize) -> Option<VirtualAddress> {
        if i < A::PAGE_ENTRIES {
            let level_shift = self.level * A::PAGE_ENTRY_SHIFT + A::PAGE_SHIFT;
            Some(self.base.add(i << level_shift))
        } else {
            None
        }
    }

    unsafe fn entry_virt(&self, i: usize) -> Option<VirtualAddress> {
        if i < A::PAGE_ENTRIES {
            Some(A::phys_to_virt(self.phys).add(i * A::PAGE_ENTRY_SIZE))
        } else {
            None
        }
    }

    pub unsafe fn entry(&self, i: usize) -> Option<PageEntry<A>> {
        unsafe {
            let addr = self.entry_virt(i)?;
            Some(PageEntry::from_data(A::read::<usize>(addr)))
        }
    }

    pub(super) unsafe fn set_entry(&mut self, i: usize, entry: PageEntry<A>) -> Option<()> {
        unsafe {
            let addr = self.entry_virt(i)?;
            A::write::<usize>(addr, entry.data());
            if self.user_shadow && i < A::PAGE_ENTRIES / 2 {
                A::write::<usize>(addr.add(A::PAGE_SIZE), entry.data());
            }
            Some(())
        }
    }

    pub(super) fn index_of(&self, address: VirtualAddress) -> Option<usize> {
        // Canonicalize address first
        let address = VirtualAddress::new(address.data() & A::PAGE_ADDRESS_MASK);
        let level_shift = self.level * A::PAGE_ENTRY_SHIFT + A::PAGE_SHIFT;
        // Intentionally wraps around at last-level table to get all-ones mask on architectures
        // where addressable physical address space covers entire usized space (e.g. x86)
        let level_mask = A::PAGE_ENTRIES
            .wrapping_shl(level_shift as u32)
            .wrapping_sub(1);
        if address >= self.base && address <= self.base.add(level_mask) {
            Some((address.data() >> level_shift) & A::PAGE_ENTRY_MASK)
        } else {
            None
        }
    }

    pub unsafe fn next(&self, i: usize) -> Option<Self> {
        if self.level == 0 {
            return None;
        }

        unsafe {
            Some(PageTable::new(
                self.entry_base(i)?,
                self.entry(i)?.address().ok()?,
                self.level - 1,
            ))
        }
    }

    pub fn debug_entries(&self, f: impl Fn(fmt::Arguments<'_>)) {
        for i in 0..A::PAGE_ENTRIES {
            if let Some(entry) = unsafe { self.entry(i) }
                && entry.present()
            {
                f(format_args!("{}: {:X}", i, entry.data()));
            }
        }
    }
}
