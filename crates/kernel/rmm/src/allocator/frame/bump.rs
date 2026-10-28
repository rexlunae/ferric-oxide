//! Boot-time frame allocation across a list of disjoint physical memory areas.

use core::marker::PhantomData;

use crate::{Arch, FrameAllocator, FrameCount, FrameUsage, MemoryArea, PhysicalAddress};

/// A zeroing allocator that advances through memory areas without reusing frames.
#[derive(Debug)]
pub struct BumpAllocator<A> {
    orig_areas: (&'static [MemoryArea], usize),
    cur_areas: (&'static [MemoryArea], usize),
    _marker: PhantomData<fn() -> A>,
}

impl<A: Arch> BumpAllocator<A> {
    /// Skips `offset` bytes across `areas` before allocating frames.
    ///
    /// An offset past the end produces an exhausted allocator.
    ///
    /// # Safety
    ///
    /// Areas must be disjoint, page-aligned ranges of exclusively owned physical
    /// memory, writable through `A`'s physical mapping for the allocator's lifetime.
    /// Their sizes and `offset` must be page-aligned, their address ranges and
    /// total size must fit in `usize`, and no other allocator may own these frames.
    pub unsafe fn new(mut areas: &'static [MemoryArea], mut offset: usize) -> Self {
        while let Some(first) = areas.first()
            && first.size <= offset
        {
            offset -= first.size;
            areas = &areas[1..];
        }
        // No residual offset belongs to an empty area list. Keeping one would
        // underflow usage accounting after an offset beyond all supplied areas.
        if areas.is_empty() {
            offset = 0;
        }

        Self {
            orig_areas: (areas, offset),
            cur_areas: (areas, offset),
            _marker: PhantomData,
        }
    }
    /// Returns the areas remaining after the initial offset was skipped.
    pub fn areas(&self) -> &'static [MemoryArea] {
        self.orig_areas.0
    }
    /// Returns one semifree and the fully free areas. The offset is the number of bytes after
    /// which the first area is free.
    pub fn free_areas(&self) -> (&'static [MemoryArea], usize) {
        self.cur_areas
    }
    /// Returns the next physical position, or zero when no areas remain.
    pub fn abs_offset(&self) -> PhysicalAddress {
        let (areas, off) = self.cur_areas;
        areas
            .first()
            .map_or(PhysicalAddress::new(0), |a| a.base.add(off))
    }
    /// Returns bytes consumed since construction, including skipped area tails.
    pub fn offset(&self) -> usize {
        self.usage().used().data() * A::PAGE_SIZE
    }
}

// SAFETY: Under the memory-area ownership contract, advancing the cursor gives
// each allocation exclusive, disjoint frames. They are zeroed before publication.
unsafe impl<A: Arch> FrameAllocator for BumpAllocator<A> {
    fn allocate(&mut self, count: FrameCount) -> Option<PhysicalAddress> {
        let req_size = count.data().checked_mul(A::PAGE_SIZE)?;
        if req_size == 0 {
            return None;
        }

        let block = loop {
            let area = self.cur_areas.0.first()?;
            let off = self.cur_areas.1;
            if area.size - off < req_size {
                self.cur_areas = (&self.cur_areas.0[1..], 0);
                continue;
            }
            self.cur_areas.1 += req_size;

            break area.base.add(off);
        };
        // SAFETY: The cursor reserved this exclusively owned range in a mapped
        // memory area, and the checked size cannot wrap into another allocation.
        unsafe {
            A::write_bytes(A::phys_to_virt(block), 0, req_size);
        }
        Some(block)
    }

    /// # Panics
    ///
    /// Always panics: bump allocations cannot be individually reclaimed.
    unsafe fn free(&mut self, _address: PhysicalAddress, _count: FrameCount) {
        unimplemented!("BumpAllocator::free not implemented");
    }

    fn usage(&self) -> FrameUsage {
        let total = self.orig_areas.0.iter().map(|a| a.size).sum::<usize>() - self.orig_areas.1;
        let free = self.cur_areas.0.iter().map(|a| a.size).sum::<usize>() - self.cur_areas.1;
        FrameUsage::new(
            FrameCount::new((total - free) / A::PAGE_SIZE),
            FrameCount::new(total / A::PAGE_SIZE),
        )
    }
}

#[cfg(all(test, feature = "std", target_pointer_width = "64"))]
mod tests {
    extern crate std;

    use super::*;
    use crate::emulate::{fresh_machine as machine, EmulateArch};

    const PAGE: usize = EmulateArch::PAGE_SIZE;
    static AREAS: [MemoryArea; 2] = [
        MemoryArea {
            base: PhysicalAddress::new(4 * PAGE),
            size: PAGE,
        },
        MemoryArea {
            base: PhysicalAddress::new(8 * PAGE),
            size: 2 * PAGE,
        },
    ];

    #[test]
    fn rejects_empty_and_overflowing_requests_without_consuming_frames() {
        let _machine = machine();
        // SAFETY: The disjoint areas are exclusively owned by this test's machine
        // guard and lie inside the initial writable 2 MiB mapping.
        let mut allocator = unsafe { BumpAllocator::<EmulateArch>::new(&AREAS, 0) };
        for count in [0, usize::MAX / PAGE + 1, usize::MAX / PAGE + 2, usize::MAX] {
            assert!(allocator.allocate(FrameCount::new(count)).is_none());
            assert_eq!(allocator.usage().used().data(), 0);
            assert_eq!(allocator.abs_offset(), AREAS[0].base);
        }
    }

    #[test]
    fn skips_initial_offsets_across_area_boundaries() {
        let _machine = machine();
        {
            // SAFETY: The test owns the mapped areas, and PAGE preserves alignment.
            let allocator = unsafe { BumpAllocator::<EmulateArch>::new(&AREAS, PAGE) };
            assert_eq!(allocator.abs_offset(), AREAS[1].base);
            assert_eq!(allocator.usage().total().data(), 2);
        }
        for offset in [3 * PAGE, 4 * PAGE, usize::MAX & !(PAGE - 1)] {
            // SAFETY: The test owns the mapped areas; offsets are page aligned.
            // Each allocator is dropped before the next one takes ownership.
            let mut exhausted = unsafe { BumpAllocator::<EmulateArch>::new(&AREAS, offset) };
            assert!(exhausted.allocate_one().is_none());
            assert_eq!(exhausted.usage().free().data(), 0);
        }
    }

    #[test]
    fn allocations_are_zeroed_disjoint_and_exhaustible() {
        let _machine = machine();
        // SAFETY: The test owns the disjoint mapped areas under the machine guard.
        let mut allocator = unsafe { BumpAllocator::<EmulateArch>::new(&AREAS, 0) };
        for (index, address) in [4 * PAGE, 8 * PAGE, 9 * PAGE].into_iter().enumerate() {
            let physical = PhysicalAddress::new(address);
            let virtual_address = EmulateArch::phys_to_virt(physical);
            // SAFETY: This byte is mapped, writable emulated memory, not real
            // hardware. Setting it checks that allocation actually zeros it.
            unsafe { EmulateArch::write(virtual_address, 0xa5_u8) };
            assert_eq!(allocator.allocate_one(), Some(physical));
            // SAFETY: The frame remains owned and mapped in the emulated machine.
            assert_eq!(unsafe { EmulateArch::read::<u8>(virtual_address) }, 0);
            assert_eq!(allocator.usage().used().data(), index + 1);
        }
        assert!(allocator.allocate_one().is_none());
        assert_eq!(allocator.usage().free().data(), 0);
    }
}
