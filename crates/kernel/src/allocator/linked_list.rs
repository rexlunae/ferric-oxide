//! Kernel global allocator: serialized TLSF with contiguous MiB growth.
//!
//! The former first-fit list made heap lock hold time grow with fragmentation.
//! Mapping and physical allocation still run under this lock and remain unbounded;
//! no allocator operation is permitted from a hard IRQ that could preempt its owner.

use crate::memory::KernelMapper;
use core::{
    alloc::{GlobalAlloc, Layout},
    ptr::NonNull,
};
use ferric_heap::Heap;
use spin::Mutex;

static HEAP: Mutex<Heap<'static>> = Mutex::new(Heap::new());

/// Kernel heap adapter with serialized TLSF bookkeeping and mapped growth.
pub struct Allocator;

impl Allocator {
    /// Claims the mapped initial heap, once during boot.
    ///
    /// # Safety
    /// The nonzero, page-aligned range must be exclusively writable and remain
    /// mapped for the kernel lifetime. No allocation may predate initialization.
    pub unsafe fn init(offset: usize, size: usize) {
        // SAFETY: boot mapped this exclusive permanent kernel virtual region.
        unsafe {
            let region = NonNull::new(core::ptr::slice_from_raw_parts_mut(offset as *mut u8, size))
                .expect("mapped kernel heap starts at a nonzero address");
            HEAP.lock()
                .claim(region)
                .expect("initial page-aligned MiB can hold TLSF metadata");
        }
    }
}

// SAFETY: HEAP serializes all access; the mapping path exclusively extends the
// reserved kernel heap region and keeps every live allocation mapped.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the locked heap owns its mapped region. Growth maps the next
        // contiguous MiB before transferring it to the same pool.
        unsafe {
            let mut heap = HEAP.lock();
            loop {
                if let Some(pointer) = heap.allocate(layout) {
                    return pointer.as_ptr();
                }
                let Some(offset) = heap.mapped_end() else {
                    panic!("__rust_allocate: heap not initialized");
                };
                if offset.checked_add(super::KERNEL_HEAP_SIZE).is_none() {
                    return core::ptr::null_mut();
                }
                super::map_heap(
                    &mut KernelMapper::lock_rw(),
                    offset,
                    super::KERNEL_HEAP_SIZE,
                );
                let region = NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(
                    offset as *mut u8,
                    super::KERNEL_HEAP_SIZE,
                ));
                heap.extend(region)
                    .expect("mapped MiB immediately follows the owned kernel heap");
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: GlobalAlloc requires a live non-null pointer and its original
        // layout. The lock excludes every competing heap operation.
        unsafe { HEAP.lock().deallocate(NonNull::new_unchecked(ptr), layout) }
    }
}
