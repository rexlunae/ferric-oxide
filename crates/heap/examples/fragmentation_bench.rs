//! Backend-only cost with many small holes preceding a usable large free tail.
//!
//! The old kernel backend is a dev dependency solely for this comparison.
//! Setup/free order is excluded; both backends retain all barrier allocations.

use ferric_heap::Heap;
use std::{alloc::Layout, hint::black_box, ptr::NonNull, time::Instant};

const ARENA_BYTES: usize = 16 * 1024 * 1024;
const ITERATIONS: usize = 1024;

fn main() {
    println!(
        "header_bytes first_fit={} tlsf={}",
        std::mem::size_of::<linked_list_allocator::Heap>(),
        std::mem::size_of::<Heap<'static>>()
    );
    println!("holes first_fit_ns/pair tlsf_ns/pair");
    for holes in [1, 64, 4096, 32768] {
        let old = first_fit(holes);
        let new = tlsf(holes);
        println!("{holes} {old:.2} {new:.2}");
    }
}

fn first_fit(count: usize) -> f64 {
    let mut arena = vec![0u8; ARENA_BYTES];
    // SAFETY: the fixed arena remains exclusively owned by heap for this function.
    let mut heap =
        unsafe { linked_list_allocator::Heap::new(arena.as_mut_ptr() as usize, arena.len()) };
    let small = Layout::from_size_align(80, 8).unwrap();
    let barrier = Layout::from_size_align(24, 8).unwrap();
    let large = Layout::from_size_align(8192, 64).unwrap();
    let mut holes = Vec::new();
    let mut barriers = Vec::new();
    for _ in 0..count {
        holes.push(heap.allocate_first_fit(small).unwrap());
        barriers.push(heap.allocate_first_fit(barrier).unwrap());
    }
    for pointer in holes.into_iter().rev() {
        // SAFETY: every small allocation is live, with exactly this layout.
        unsafe {
            heap.deallocate(pointer, small);
        }
    }
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let pointer = heap.allocate_first_fit(black_box(large)).unwrap();
        black_box(pointer);
        // SAFETY: the matching allocation is retired once with its original layout.
        unsafe {
            heap.deallocate(pointer, large);
        }
    }
    let elapsed = start.elapsed().as_nanos() as f64 / ITERATIONS as f64;
    for pointer in barriers {
        // SAFETY: all retained barriers remain live until this matching free.
        unsafe {
            heap.deallocate(pointer, barrier);
        }
    }
    elapsed
}

fn tlsf(count: usize) -> f64 {
    let mut arena = vec![0u8; ARENA_BYTES];
    let mut heap = Heap::new();
    // SAFETY: the arena is exclusively owned and outlives heap and every allocation.
    unsafe {
        heap.claim(NonNull::from(arena.as_mut_slice())).unwrap();
    }
    let small = Layout::from_size_align(80, 8).unwrap();
    let barrier = Layout::from_size_align(24, 8).unwrap();
    let large = Layout::from_size_align(8192, 64).unwrap();
    let mut holes = Vec::new();
    let mut barriers = Vec::new();
    for _ in 0..count {
        holes.push(heap.allocate(small).unwrap());
        barriers.push(heap.allocate(barrier).unwrap());
    }
    for pointer in holes.into_iter().rev() {
        // SAFETY: every small allocation is live, with exactly this layout.
        unsafe {
            heap.deallocate(pointer, small);
        }
    }
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let pointer = heap.allocate(black_box(large)).unwrap();
        black_box(pointer);
        // SAFETY: the matching allocation is retired once with its original layout.
        unsafe {
            heap.deallocate(pointer, large);
        }
    }
    let elapsed = start.elapsed().as_nanos() as f64 / ITERATIONS as f64;
    for pointer in barriers {
        // SAFETY: all retained barriers remain live until this matching free.
        unsafe {
            heap.deallocate(pointer, barrier);
        }
    }
    elapsed
}
