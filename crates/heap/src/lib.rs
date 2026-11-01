//! Contiguous kernel heap storage with segregated-fit bookkeeping.
//!
//! [`Heap`] adapts the MIT/Apache-2.0 [`rlsf`] TLSF implementation to a growing
//! virtual region. It tracks both mapped bytes and bytes incorporated by TLSF,
//! so an extension includes any unincorporated tail. The caller owns mapping,
//! synchronization, allocation failure policy and the memory lifetime. No Rust
//! global allocation is used by this adapter or by its TLSF metadata.
//!
//! TLSF allocation/deallocation bookkeeping has constant complexity; lock waits,
//! mapping, physical allocation and TLB shootdowns do not acquire such a bound.
//! This is not permission to allocate from hard IRQ context.
//!
//! Algorithm/API reference: [`rlsf::Tlsf`] and Masmano et al., *TLSF: a new
//! dynamic memory allocator for real-time systems*, ECRTS 2004,
//! DOI: 10.1109/EMRTS.2004.1311009. No OS implementation source was used.
//!
//! # Examples
//!
//! ```
//! use core::{alloc::Layout, ptr::NonNull};
//! use ferric_heap::Heap;
//!
//! let mut arena = [0u8; 4096];
//! let mut heap = Heap::new();
//! // SAFETY: the arena is exclusively owned here, outlives the heap and its
//! // allocation, and is not used again until after the heap is dropped.
//! unsafe { heap.claim(NonNull::from(&mut arena[..]))?; }
//! let layout = Layout::new::<u64>();
//! let allocation = heap.allocate(layout).expect("the arena is large enough");
//! // SAFETY: allocation is live, belongs to this heap and has this layout.
//! unsafe { heap.deallocate(allocation, layout); }
//! # Ok::<(), ferric_heap::RegionError>(())
//! ```

#![no_std]
#![deny(missing_docs, unsafe_op_in_unsafe_fn)]

use core::{alloc::Layout, fmt, ptr::NonNull};
use rlsf::Tlsf;

// Kernel targets and host validation are 64-bit. 32-byte granularity plus 59
// first levels spans usize, enabling TLSF's contiguous-pool coalescing path.
const _: () = assert!(usize::BITS == 64);
type Inner<'pool> = Tlsf<'pool, u64, u32, 59, 32>;

/// An invalid initial region or heap extension, rejected before ownership changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RegionError {
    /// The initial region has already been claimed.
    AlreadyInitialized,
    /// An extension was supplied before an initial region.
    Uninitialized,
    /// The region cannot hold TLSF's minimum metadata and payload.
    TooSmall,
    /// The region's exclusive end address is not representable.
    AddressOverflow,
    /// The extension does not immediately follow all previously mapped bytes.
    NonContiguous {
        /// Required starting address.
        expected: usize,
        /// Supplied starting address.
        provided: usize,
    },
}

impl fmt::Display for RegionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitialized => f.write_str("heap region already initialized"),
            Self::Uninitialized => f.write_str("heap region is not initialized"),
            Self::TooSmall => f.write_str("heap region is too small"),
            Self::AddressOverflow => f.write_str("heap region address overflow"),
            Self::NonContiguous { expected, provided } => {
                write!(
                    f,
                    "heap extension starts at {provided:#x}, expected {expected:#x}"
                )
            }
        }
    }
}

impl core::error::Error for RegionError {}

#[derive(Debug)]
struct Region {
    start: NonNull<u8>,
    mapped_len: usize,
    incorporated_len: usize,
}

/// An allocation-free TLSF header and ownership record for one growing region.
///
/// External synchronization is required for concurrent use. Memory is never
/// unmapped or returned to the caller by this type; dropping it only drops the
/// header. All outstanding allocations must be retired before reclaiming its
/// backing storage. Regions and allocations must remain valid for `'pool`.
#[derive(Debug)]
pub struct Heap<'pool> {
    inner: Inner<'pool>,
    region: Option<Region>,
}

impl Default for Heap<'_> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: the raw region pointer is only accessed under exclusive &mut Heap;
// transferring its owner does not alter the region lifetime or its ownership.
unsafe impl Send for Region {}

impl<'pool> Heap<'pool> {
    /// Creates an empty header without allocating or claiming memory.
    pub const fn new() -> Self {
        Self {
            inner: Inner::new(),
            region: None,
        }
    }

    /// Returns the number of mapped bytes owned by this heap.
    pub fn mapped_size(&self) -> usize {
        self.region.as_ref().map_or(0, |region| region.mapped_len)
    }

    /// Returns the address immediately after all owned mapped bytes.
    pub fn mapped_end(&self) -> Option<usize> {
        self.region.as_ref().map(|region| {
            region
                .start
                .as_ptr()
                .addr()
                .saturating_add(region.mapped_len)
        })
    }

    /// Claims the initial mapped region.
    ///
    /// # Errors
    /// Returns [`RegionError::AlreadyInitialized`] if already claimed,
    /// [`RegionError::AddressOverflow`] if the exclusive end overflows, or
    /// [`RegionError::TooSmall`] if TLSF cannot use the region. On error no
    /// ownership transfers and no live allocation is changed.
    ///
    /// # Safety
    /// The region must be exclusively writable, with valid pointer provenance,
    /// for `'pool`. The caller must not otherwise access its bytes after success.
    /// If extended later, all ranges must be parts of one contiguous allocation
    /// or reserved virtual region, with provenance permitting the combined pool.
    pub unsafe fn claim(&mut self, memory: NonNull<[u8]>) -> Result<(), RegionError> {
        if self.region.is_some() {
            return Err(RegionError::AlreadyInitialized);
        }
        let start = memory.cast::<u8>();
        start
            .as_ptr()
            .addr()
            .checked_add(memory.len())
            .ok_or(RegionError::AddressOverflow)?;
        // SAFETY: the caller transfers exclusive ownership for 'pool; this is
        // the first claim, so it cannot overlap an existing pool in this header.
        let incorporated =
            unsafe { self.inner.insert_free_block_ptr(memory) }.ok_or(RegionError::TooSmall)?;
        self.region = Some(Region {
            start,
            mapped_len: memory.len(),
            incorporated_len: incorporated.get(),
        });
        Ok(())
    }

    /// Extends this heap with the immediately following mapped region.
    ///
    /// Any unused trailing bytes from the preceding region are included. This
    /// allows allocations larger than a mapping increment to span increments.
    ///
    /// # Errors
    /// Returns [`RegionError::Uninitialized`], [`RegionError::NonContiguous`] or
    /// [`RegionError::AddressOverflow`] without changing the heap or its ownership.
    ///
    /// # Safety
    /// The new region must be exclusively writable for `'pool`. On success its
    /// ownership transfers to this heap. Its provenance must permit access to
    /// the combined pool, as specified by [`Self::claim`]. Existing allocations
    /// and all preceding mapped bytes must remain mapped and unchanged.
    pub unsafe fn extend(&mut self, memory: NonNull<[u8]>) -> Result<(), RegionError> {
        let region = self.region.as_mut().ok_or(RegionError::Uninitialized)?;
        let expected = region
            .start
            .as_ptr()
            .addr()
            .checked_add(region.mapped_len)
            .ok_or(RegionError::AddressOverflow)?;
        let provided = memory.cast::<u8>().as_ptr().addr();
        if provided != expected {
            return Err(RegionError::NonContiguous { expected, provided });
        }
        let mapped_len = region
            .mapped_len
            .checked_add(memory.len())
            .ok_or(RegionError::AddressOverflow)?;
        region
            .start
            .as_ptr()
            .addr()
            .checked_add(mapped_len)
            .ok_or(RegionError::AddressOverflow)?;
        let available = mapped_len.saturating_sub(region.incorporated_len);
        // SAFETY: incorporated_len is TLSF's previous returned pool extent.
        // The caller owns the original region and this contiguous extension
        // with provenance for the combined range; all available bytes are mapped.
        let incorporated = unsafe {
            let tail = region.start.as_ptr().add(region.incorporated_len);
            let tail = NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(tail, available));
            self.inner.append_free_block_ptr(tail)
        };
        region.mapped_len = mapped_len;
        region.incorporated_len = region.incorporated_len.saturating_add(incorporated);
        Ok(())
    }

    /// Allocates from claimed storage, returning `None` for zero size or exhaustion.
    ///
    /// Bookkeeping does not allocate externally or traverse the free-block count.
    pub fn allocate(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        if layout.size() == 0 {
            return None;
        }
        self.inner.allocate(layout)
    }

    /// Returns one allocation to this heap, coalescing adjacent free blocks.
    ///
    /// # Safety
    /// `pointer` must be a live allocation from this heap with exactly `layout`.
    /// All accesses to it must have ended; it must not be freed more than once.
    pub unsafe fn deallocate(&mut self, pointer: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller supplies a live allocation and its original layout.
        unsafe {
            self.inner.deallocate(pointer, layout.align());
        }
    }
}

#[cfg(test)]
mod tests;
