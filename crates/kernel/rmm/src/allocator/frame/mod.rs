//! Physical frame counts, usage accounting, and allocator ownership contracts.

use crate::PhysicalAddress;

pub use self::{buddy::*, bump::*};

mod buddy;
mod bump;

#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
/// A number of architecture-sized physical frames.
pub struct FrameCount(usize);

impl FrameCount {
    /// Wraps a frame count without converting it to bytes.
    pub fn new(count: usize) -> Self {
        Self(count)
    }

    /// Returns the number of frames.
    pub fn data(&self) -> usize {
        self.0
    }
}

/// Frame usage reported by an allocator.
#[derive(Debug)]
pub struct FrameUsage {
    used: FrameCount,
    total: FrameCount,
}

impl FrameUsage {
    /// Records usage; `used` must not exceed `total`.
    pub fn new(used: FrameCount, total: FrameCount) -> Self {
        Self { used, total }
    }

    /// Returns frames unavailable for allocation, including allocator overhead.
    pub fn used(&self) -> FrameCount {
        self.used
    }

    /// Returns frames still available for allocation.
    pub fn free(&self) -> FrameCount {
        FrameCount(self.total.0 - self.used.0)
    }

    /// Returns all frames managed by this allocator.
    pub fn total(&self) -> FrameCount {
        self.total
    }
}

/// Allocates and releases exclusive ranges of physical frames.
///
/// # Safety
///
/// Successful allocations must be aligned, contain at least the requested frame
/// count, and remain disjoint from every live allocation and reserved range.
/// Frames may be reused only after a matching `free` relinquishes ownership.
pub unsafe trait FrameAllocator {
    // TODO: must_be_zero
    /// Allocates at least `count` contiguous frames, or returns `None` on failure.
    fn allocate(&mut self, count: FrameCount) -> Option<PhysicalAddress>;

    /// Relinquishes a previous allocation.
    ///
    /// # Safety
    ///
    /// `address` and `count` must match a live allocation from this allocator.
    /// No mapping, pointer, CPU, or device may access the range after it is freed.
    unsafe fn free(&mut self, address: PhysicalAddress, count: FrameCount);

    /// Allocates one physical frame.
    fn allocate_one(&mut self) -> Option<PhysicalAddress> {
        self.allocate(FrameCount::new(1))
    }

    /// Relinquishes a single-frame allocation.
    ///
    /// # Safety
    ///
    /// The ownership and lifetime requirements of [`Self::free`] apply, with a
    /// count of one frame.
    unsafe fn free_one(&mut self, address: PhysicalAddress) {
        // SAFETY: The caller supplies the ownership guarantees required by free.
        unsafe {
            self.free(address, FrameCount::new(1));
        }
    }

    /// Returns frame accounting for this allocator.
    fn usage(&self) -> FrameUsage;
}

// SAFETY: Every operation forwards to the same allocator without changing the
// allocation's address, size, ownership, or lifetime.
unsafe impl<T> FrameAllocator for &mut T
where
    T: FrameAllocator,
{
    fn allocate(&mut self, count: FrameCount) -> Option<PhysicalAddress> {
        T::allocate(self, count)
    }
    unsafe fn free(&mut self, address: PhysicalAddress, count: FrameCount) {
        // SAFETY: The caller's ownership contract is forwarded unchanged.
        unsafe { T::free(self, address, count) }
    }
    fn allocate_one(&mut self) -> Option<PhysicalAddress> {
        T::allocate_one(self)
    }
    unsafe fn free_one(&mut self, address: PhysicalAddress) {
        // SAFETY: The caller's single-frame ownership contract is unchanged.
        unsafe { T::free_one(self, address) }
    }
    fn usage(&self) -> FrameUsage {
        T::usage(self)
    }
}
