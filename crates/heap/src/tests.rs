//! Exact-layout, ownership, growth and fragmentation regressions.

extern crate std;

use super::*;
use std::{boxed::Box, sync::Mutex, vec, vec::Vec};

#[test]
fn mapped_regions_do_not_need_zeroed_or_initialized_contents() {
    let mut arena = Box::<[u8]>::new_uninit_slice(16 * 1024);
    let start = arena.as_mut_ptr().cast::<u8>();
    let mut heap = Heap::new();
    // SAFETY: both ranges have the same allocation-wide provenance and are
    // exclusively owned here. MaybeUninit storage is writable without being
    // initialized; no uninitialized payload is read by this test.
    unsafe {
        let initial = NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(start, 4096));
        heap.claim(initial).unwrap();
        let layout = Layout::from_size_align(80, 64).unwrap();
        let allocation = heap.allocate(layout).unwrap();
        allocation.as_ptr().write_bytes(0xa5, layout.size());
        let extension = NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(
            start.add(4096),
            12 * 1024,
        ));
        heap.extend(extension).unwrap();
        assert!(
            core::slice::from_raw_parts(allocation.as_ptr(), layout.size())
                .iter()
                .all(|&b| b == 0xa5)
        );
        heap.deallocate(allocation, layout);
        assert!(
            heap.allocate(Layout::from_size_align(12 * 1024, 128).unwrap())
                .is_some()
        );
    }
}

fn region(arena: &mut Vec<u8>, offset: usize, length: usize) -> NonNull<[u8]> {
    assert!(offset <= arena.len() && length <= arena.len() - offset);
    // SAFETY: the checked range is within this single arena allocation. All
    // regions derive from the same arena-wide pointer/provenance.
    unsafe {
        NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(
            arena.as_mut_ptr().add(offset),
            length,
        ))
    }
}

#[test]
fn uninitialized_and_zero_sized_allocations_fail_without_mutation() {
    let mut heap = Heap::new();
    assert_eq!(heap.mapped_size(), 0);
    assert_eq!(heap.mapped_end(), None);
    assert!(heap.allocate(Layout::new::<u64>()).is_none());
    assert!(heap.allocate(Layout::new::<()>()).is_none());
    let mut arena = vec![0; 1024];
    // SAFETY: arena is exclusively available and outlives heap. This error
    // transfers no ownership and never enters TLSF.
    assert_eq!(
        unsafe { heap.extend(region(&mut arena, 0, 1024)) },
        Err(RegionError::Uninitialized)
    );
}

#[test]
fn failed_small_claim_can_be_replaced_by_a_valid_claim() {
    let mut arena = vec![0; 4096];
    let mut heap = Heap::new();
    // SAFETY: arena is owned here and outlives heap; failed claim keeps ownership.
    assert_eq!(
        unsafe { heap.claim(region(&mut arena, 0, 1)) },
        Err(RegionError::TooSmall)
    );
    assert_eq!(heap.mapped_size(), 0);
    // SAFETY: the previous failed claim left the whole arena available.
    unsafe {
        heap.claim(region(&mut arena, 0, 4096)).unwrap();
    }
    assert!(heap.allocate(Layout::new::<u64>()).is_some());
}

#[test]
fn alignment_and_live_contents_survive_other_allocations_and_frees() {
    let mut arena = vec![0; 128 * 1024];
    let mut heap = Heap::new();
    // SAFETY: this exclusive arena outlives heap and its allocations.
    unsafe {
        heap.claim(region(&mut arena, 0, 128 * 1024)).unwrap();
    }
    let mut allocations = Vec::new();
    for (index, (size, align)) in [
        (1, 1),
        (24, 8),
        (80, 16),
        (48, 64),
        (4096, 4096),
        (8192, 128),
    ]
    .into_iter()
    .enumerate()
    {
        let layout = Layout::from_size_align(size, align).unwrap();
        let pointer = heap.allocate(layout).unwrap();
        assert_eq!(pointer.as_ptr().addr() % align, 0);
        // SAFETY: each allocation has size initialized bytes, is writable and
        // distinct from all other still-live allocations.
        unsafe {
            pointer.as_ptr().write_bytes(index as u8, size);
        }
        allocations.push((pointer, layout, index as u8));
    }
    for (pointer, layout, byte) in allocations.into_iter().rev() {
        // SAFETY: pointer is still live with layout.size() initialized bytes;
        // its original layout is supplied exactly once when freeing it.
        unsafe {
            assert!(
                core::slice::from_raw_parts(pointer.as_ptr(), layout.size())
                    .iter()
                    .all(|&b| b == byte)
            );
            heap.deallocate(pointer, layout);
        }
    }
    assert!(
        heap.allocate(Layout::from_size_align(96 * 1024, 8).unwrap())
            .is_some()
    );
}

#[test]
fn contiguous_extensions_coalesce_across_mapping_boundaries() {
    let mut arena = vec![0; 16 * 1024];
    let base = arena.as_mut_ptr().addr();
    let mut heap = Heap::new();
    // SAFETY: all claims/growth are disjoint contiguous ranges of this one
    // exclusive arena allocation, which outlives the combined heap.
    unsafe {
        heap.claim(region(&mut arena, 0, 4096)).unwrap();
        let small_layout = Layout::from_size_align(512, 64).unwrap();
        let small = heap.allocate(small_layout).unwrap();
        small.as_ptr().write_bytes(0x5a, small_layout.size());
        assert!(
            heap.allocate(Layout::from_size_align(8192, 8).unwrap())
                .is_none()
        );
        heap.extend(region(&mut arena, 4096, 4096)).unwrap();
        heap.extend(region(&mut arena, 8192, 8192)).unwrap();
        assert_eq!(heap.mapped_size(), 16 * 1024);
        assert_eq!(heap.mapped_end(), Some(base + 16 * 1024));
        let large_layout = Layout::from_size_align(12 * 1024, 8).unwrap();
        let large = heap.allocate(large_layout).unwrap();
        large.as_ptr().write_bytes(0x27, large_layout.size());
        assert!(
            core::slice::from_raw_parts(small.as_ptr(), small_layout.size())
                .iter()
                .all(|&b| b == 0x5a)
        );
        heap.deallocate(large, large_layout);
        heap.deallocate(small, small_layout);
    }
}

#[test]
fn unaligned_claim_and_small_growth_keep_the_unincorporated_tail() {
    let mut arena = vec![0; 8192];
    let mut heap = Heap::new();
    // SAFETY: every range is a contiguous disjoint piece of this single arena;
    // the initial unaligned prefix and final suffix are outside the heap.
    unsafe {
        heap.claim(region(&mut arena, 3, 1001)).unwrap();
        heap.extend(region(&mut arena, 1004, 1)).unwrap();
        heap.extend(region(&mut arena, 1005, 7)).unwrap();
        heap.extend(region(&mut arena, 1012, 7000)).unwrap();
        assert_eq!(heap.mapped_size(), 8009);
        let layout = Layout::from_size_align(7000, 8).unwrap();
        let pointer = heap.allocate(layout).unwrap();
        pointer.as_ptr().write_bytes(0xf3, layout.size());
        heap.deallocate(pointer, layout);
    }
}

#[test]
fn rejected_growth_and_reinitialization_preserve_a_live_allocation() {
    let mut arena = vec![0; 8192];
    let base = arena.as_mut_ptr().addr();
    let mut heap = Heap::new();
    // SAFETY: the first 4096 bytes are exclusive to heap; rejected ranges are
    // valid unused memory and never transfer their ownership.
    unsafe {
        heap.claim(region(&mut arena, 0, 4096)).unwrap();
        let layout = Layout::new::<u64>();
        let pointer = heap.allocate(layout).unwrap().cast::<u64>();
        pointer.as_ptr().write(0x1234_5678_9abc_def0);
        assert_eq!(
            heap.claim(region(&mut arena, 4096, 4096)),
            Err(RegionError::AlreadyInitialized)
        );
        assert_eq!(
            heap.extend(region(&mut arena, 4097, 4095)),
            Err(RegionError::NonContiguous {
                expected: base + 4096,
                provided: base + 4097
            })
        );
        assert_eq!(heap.mapped_size(), 4096);
        assert_eq!(pointer.as_ptr().read(), 0x1234_5678_9abc_def0);
        heap.extend(region(&mut arena, 4096, 4096)).unwrap();
        heap.deallocate(pointer.cast(), layout);
    }
}

#[test]
fn fragmented_small_holes_do_not_prevent_reuse_of_a_large_tail() {
    let mut arena = vec![0; 128 * 1024];
    let mut heap = Heap::new();
    // SAFETY: arena is exclusively available for the heap lifetime.
    unsafe {
        heap.claim(region(&mut arena, 0, 128 * 1024)).unwrap();
    }
    let small = Layout::from_size_align(80, 8).unwrap();
    let barrier = Layout::from_size_align(24, 8).unwrap();
    let mut holes = Vec::new();
    let mut barriers = Vec::new();
    for _ in 0..256 {
        holes.push(heap.allocate(small).unwrap());
        barriers.push(heap.allocate(barrier).unwrap());
    }
    for hole in holes.into_iter().rev() {
        // Every live barrier keeps two freed regions from coalescing.
        // SAFETY: each hole is still live, with exactly the small layout.
        unsafe {
            heap.deallocate(hole, small);
        }
    }
    let large = Layout::from_size_align(8192, 64).unwrap();
    for _ in 0..128 {
        let pointer = heap.allocate(large).unwrap();
        // SAFETY: each loop retires exactly its live large allocation.
        unsafe {
            heap.deallocate(pointer, large);
        }
    }
    for pointer in barriers {
        // SAFETY: each live barrier is retired once with its original layout.
        unsafe {
            heap.deallocate(pointer, barrier);
        }
    }
    assert!(
        heap.allocate(Layout::from_size_align(96 * 1024, 8).unwrap())
            .is_some()
    );
}

#[test]
fn externally_locked_threads_preserve_disjoint_live_contents() {
    let mut arena = vec![0; 128 * 1024];
    let mut heap = Heap::new();
    // SAFETY: arena remains exclusively owned by heap through the scoped joins.
    unsafe {
        heap.claim(region(&mut arena, 0, 128 * 1024)).unwrap();
    }
    let heap = Mutex::new(heap);
    std::thread::scope(|scope| {
        for thread in 0..4u8 {
            let heap = &heap;
            scope.spawn(move || {
                for round in 0..64 {
                    let layout = Layout::from_size_align(80 + round % 32, 32).unwrap();
                    let pointer = heap.lock().unwrap().allocate(layout).unwrap();
                    // SAFETY: this thread owns a distinct live allocation.
                    // Metadata for other allocations cannot alias its payload.
                    unsafe {
                        pointer.as_ptr().write_bytes(thread, layout.size());
                        assert!(
                            core::slice::from_raw_parts(pointer.as_ptr(), layout.size())
                                .iter()
                                .all(|&b| b == thread)
                        );
                        heap.lock().unwrap().deallocate(pointer, layout);
                    }
                }
            });
        }
    });
}
