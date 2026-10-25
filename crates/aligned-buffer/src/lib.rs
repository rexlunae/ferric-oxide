//! Owned, zero-initialized byte buffers with a caller-selected alignment.
//!
//! This crate uses `alloc` without requiring an operating system. It retains the
//! allocation layout so that destruction uses exactly the size and alignment
//! passed to the allocator, including for dynamically sized CPU state buffers.
//!
//! # Examples
//!
//! ```
//! use ferric_aligned_buffer::AlignedBuffer;
//!
//! let mut state = AlignedBuffer::<64>::try_zeroed(4096)?;
//! assert_eq!(state.as_ptr().addr() % 64, 0);
//! state[0] = 42;
//! assert_eq!(state.try_clone()?[0], 42);
//! # Ok::<(), ferric_aligned_buffer::AllocError>(())
//! ```

#![no_std]
#![deny(missing_docs, unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::alloc::{alloc_zeroed, dealloc, handle_alloc_error};
use core::{
    alloc::{Layout, LayoutError},
    fmt,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    slice,
};

/// A failure to construct an aligned buffer.
#[derive(Debug)]
#[non_exhaustive]
pub enum AllocError {
    /// The alignment is zero or not a power of two, or the size is too large.
    InvalidLayout(LayoutError),
    /// The global allocator could not satisfy the requested layout.
    Exhausted {
        /// Size and alignment of the failed allocation.
        layout: Layout,
    },
}

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLayout(source) => write!(f, "invalid buffer layout: {source}"),
            Self::Exhausted { layout } => write!(
                f,
                "cannot allocate {} bytes aligned to {} bytes",
                layout.size(),
                layout.align()
            ),
        }
    }
}

impl core::error::Error for AllocError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::InvalidLayout(source) => Some(source),
            Self::Exhausted { .. } => None,
        }
    }
}

/// An owned byte slice whose starting address is a multiple of `ALIGN`.
///
/// Empty buffers retain that alignment and do not call the allocator. No typed
/// values or padding are exposed: every byte is initialized and mutable.
pub struct AlignedBuffer<const ALIGN: usize> {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: The buffer exclusively owns its allocation, contains only bytes, and
// destruction can run on any thread using the global allocator.
unsafe impl<const ALIGN: usize> Send for AlignedBuffer<ALIGN> {}
// SAFETY: Shared access exposes only an immutable byte slice; mutation requires
// an exclusive borrow of the buffer.
unsafe impl<const ALIGN: usize> Sync for AlignedBuffer<ALIGN> {}

impl<const ALIGN: usize> AlignedBuffer<ALIGN> {
    /// Allocates `len` zeroed bytes aligned to `ALIGN`.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::InvalidLayout`] for an invalid alignment or an
    /// unrepresentable layout, or [`AllocError::Exhausted`] on allocation failure.
    pub fn try_zeroed(len: usize) -> Result<Self, AllocError> {
        let layout = Layout::from_size_align(len, ALIGN).map_err(AllocError::InvalidLayout)?;
        let ptr = if len == 0 {
            // A non-null, aligned dangling pointer is sufficient for an empty slice.
            NonNull::without_provenance(
                core::num::NonZeroUsize::new(layout.align())
                    .expect("Layout guarantees nonzero alignment"),
            )
        } else {
            // SAFETY: Layout validated the size and alignment, and size is nonzero.
            NonNull::new(unsafe { alloc_zeroed(layout) }).ok_or(AllocError::Exhausted { layout })?
        };
        Ok(Self { ptr, layout })
    }

    /// Copies the bytes into a distinct allocation with the same alignment.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::Exhausted`] if the new allocation fails.
    pub fn try_clone(&self) -> Result<Self, AllocError> {
        let mut copy = Self::try_zeroed(self.len())?;
        copy.copy_from_slice(self);
        Ok(copy)
    }
}

impl<const ALIGN: usize> Deref for AlignedBuffer<ALIGN> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: The pointer is non-null and aligned, len is layout-validated,
        // and the owned allocation is initialized and lives for this borrow.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl<const ALIGN: usize> DerefMut for AlignedBuffer<ALIGN> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: The same allocation invariants as Deref hold, and this exclusive
        // borrow prevents other references to its bytes for the slice's lifetime.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl<const ALIGN: usize> Clone for AlignedBuffer<ALIGN> {
    fn clone(&self) -> Self {
        self.try_clone()
            .unwrap_or_else(|_| handle_alloc_error(self.layout))
    }
}

impl<const ALIGN: usize> fmt::Debug for AlignedBuffer<ALIGN> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlignedBuffer")
            .field("ptr", &self.ptr)
            .field("layout", &self.layout)
            .finish()
    }
}

impl<const ALIGN: usize> Drop for AlignedBuffer<ALIGN> {
    fn drop(&mut self) {
        if self.layout.size() != 0 {
            // SAFETY: This pointer was allocated with exactly this layout by the
            // global allocator, is still owned by us, and will not be used again.
            unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::error::Error;

    #[test]
    fn zeroed_and_aligned_at_varied_sizes() {
        for len in [0, 1, 7, 63, 64, 65, 512, 4096, 12289] {
            let mut buffer = AlignedBuffer::<4096>::try_zeroed(len).unwrap();
            assert_eq!(buffer.len(), len);
            assert_eq!(buffer.as_ptr().addr() % 4096, 0);
            assert!(buffer.iter().all(|&byte| byte == 0));
            buffer.fill(0xa5);
            assert!(buffer.iter().all(|&byte| byte == 0xa5));
        }
    }

    #[test]
    fn invalid_layouts_preserve_the_source() {
        let error = AlignedBuffer::<0>::try_zeroed(16).unwrap_err();
        assert!(matches!(error, AllocError::InvalidLayout(_)));
        assert!(error.source().is_some());
        assert!(AlignedBuffer::<3>::try_zeroed(0).is_err());
        assert!(AlignedBuffer::<64>::try_zeroed(usize::MAX).is_err());
        assert!(AlignedBuffer::<64>::try_zeroed(isize::MAX as usize).is_err());
    }

    #[test]
    fn clone_preserves_contents_and_owns_independent_storage() {
        let mut original = AlignedBuffer::<64>::try_zeroed(2049).unwrap();
        original.fill(0x5a);
        let mut copy = original.try_clone().unwrap();
        assert_eq!(&*original, &*copy);
        assert_ne!(original.as_ptr(), copy.as_ptr());
        assert_eq!(copy.as_ptr().addr() % 64, 0);
        original[0] = 1;
        copy[2048] = 2;
        assert_eq!(copy[0], 0x5a);
        assert_eq!(original[2048], 0x5a);
        drop(original);
        assert_eq!(copy[2048], 2);
        assert_eq!(&*copy.clone(), &*copy);
    }

    #[test]
    fn empty_buffers_clone_and_drop() {
        let original = AlignedBuffer::<64>::try_zeroed(0).unwrap();
        assert!(original.try_clone().unwrap().is_empty());
        assert!(original.clone().is_empty());
    }

    #[test]
    fn send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AlignedBuffer<64>>();
    }
}
