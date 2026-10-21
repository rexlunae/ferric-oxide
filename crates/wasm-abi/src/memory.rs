//! Checking guest pointers against linear memory.
//!
//! A guest passes offsets into its memory 0. The runtime checks each `(pointer, length)`
//! pair here before it touches the bytes, with every addition overflow-checked, and then
//! reads or writes through the returned range of its own view of that memory. A pointer
//! past the memory's current size is `EFAULT`, so a guest can never name the runtime's
//! memory.

use core::ops::Range;

use crate::errno::Errno;

/// The host page size windows are aligned to.
pub const PAGE_SIZE: u64 = 4096;

/// The size of a wasm page, the unit `memory.grow` works in.
pub const WASM_PAGE_SIZE: u64 = 65536;

/// A `(pointer, length)` pair from a guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestRange {
    /// The offset into linear memory.
    pub ptr: u64,
    /// The length in bytes.
    pub len: u64,
}

impl GuestRange {
    /// A guest range, as passed to an import.
    pub const fn new(ptr: u64, len: u64) -> Self {
        Self { ptr, len }
    }

    /// The byte range inside a linear memory of `memory_len` bytes.
    ///
    /// # Errors
    ///
    /// [`Errno::EFAULT`] if any byte of the range is outside the memory, including when
    /// `ptr + len` overflows. An empty range at `memory_len` is allowed, as in wasm.
    pub fn check(self, memory_len: u64) -> Result<Range<usize>, Errno> {
        let end = self.ptr.checked_add(self.len).ok_or(Errno::EFAULT)?;
        if end > memory_len {
            return Err(Errno::EFAULT);
        }
        let start = usize::try_from(self.ptr).map_err(|_| Errno::EFAULT)?;
        let end = usize::try_from(end).map_err(|_| Errno::EFAULT)?;
        Ok(start..end)
    }

    /// As [`GuestRange::check`], and the range must start on an `align`-byte boundary
    /// (a power of two), as the `u32` of a futex or a structure the runtime writes.
    ///
    /// # Errors
    ///
    /// [`Errno::EFAULT`] as for [`GuestRange::check`], and [`Errno::EINVAL`] if it is
    /// misaligned or `align` is not a power of two.
    pub fn check_aligned(self, memory_len: u64, align: u64) -> Result<Range<usize>, Errno> {
        if !align.is_power_of_two() || self.ptr & (align - 1) != 0 {
            return Err(Errno::EINVAL);
        }
        self.check(memory_len)
    }
}

/// A page-aligned range of linear memory that a host object is mapped over (`mmap`, ring
/// memory, a registered buffer). Windows are carved out of memory the guest has grown and
/// never shrink, because wasm memories cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    start: u64,
    len: u64,
}

impl Window {
    /// The window at `addr` of `len` bytes, checked against a memory of `memory_len` bytes.
    ///
    /// # Errors
    ///
    /// [`Errno::EINVAL`] if `addr` or `len` is not a multiple of [`PAGE_SIZE`] or `len` is
    /// zero, and [`Errno::EFAULT`] if the window is not inside the memory.
    pub fn new(addr: u64, len: u64, memory_len: u64) -> Result<Self, Errno> {
        if len == 0 || !addr.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        GuestRange::new(addr, len).check(memory_len)?;
        Ok(Self { start: addr, len })
    }

    /// Its first byte, as a linear-memory offset.
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Its size in bytes, never zero.
    pub const fn size(self) -> u64 {
        self.len
    }

    /// Whether it overlaps `other`.
    pub const fn overlaps(self, other: Self) -> bool {
        self.start < other.start + other.len && other.start < self.start + self.len
    }

    /// Translates a buffer named by a guest (for example in a ring entry it submitted) into
    /// a host address, given the host address of linear memory's first byte: the buffer must
    /// lie wholly inside the window.
    ///
    /// # Errors
    ///
    /// [`Errno::EFAULT`] if it does not, or the host address overflows.
    pub fn translate(self, buffer: GuestRange, memory_base: u64) -> Result<u64, Errno> {
        let end = buffer.ptr.checked_add(buffer.len).ok_or(Errno::EFAULT)?;
        if buffer.ptr < self.start || end > self.start + self.len {
            return Err(Errno::EFAULT);
        }
        memory_base.checked_add(buffer.ptr).ok_or(Errno::EFAULT)
    }
}

/// Splits a list of NUL-terminated strings (`argv`, `envp`) into its strings.
///
/// # Errors
///
/// [`Errno::EINVAL`] if the buffer is non-empty and does not end with a NUL, or a string is
/// not UTF-8, and [`Errno::E2BIG`] if it holds more than `max` strings.
pub fn split_strings(bytes: &[u8], max: usize) -> Result<impl Iterator<Item = &str>, Errno> {
    let body: &[u8] = match bytes.split_last() {
        None => &[],
        Some((0, body)) => body,
        Some(_) => return Err(Errno::EINVAL),
    };
    let count = if bytes.is_empty() {
        0
    } else {
        body.iter().filter(|byte| **byte == 0).count() + 1
    };
    if count > max {
        return Err(Errno::E2BIG);
    }
    let parts = || body.split(|byte| *byte == 0).take(count);
    if parts().any(|part| core::str::from_utf8(part).is_err()) {
        return Err(Errno::EINVAL);
    }
    // Every part was checked above, so the fallback is never used.
    Ok(parts().map(|part| core::str::from_utf8(part).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    #[test]
    fn ranges_stay_inside_memory() {
        let memory = 2 * WASM_PAGE_SIZE;
        assert_eq!(GuestRange::new(0, 0).check(memory), Ok(0..0));
        assert_eq!(GuestRange::new(memory, 0).check(memory), Ok(131072..131072));
        assert_eq!(GuestRange::new(10, 20).check(memory), Ok(10..30));
        assert_eq!(GuestRange::new(memory, 1).check(memory), Err(Errno::EFAULT));
        assert_eq!(
            GuestRange::new(1, u64::MAX).check(memory),
            Err(Errno::EFAULT)
        );
        assert_eq!(
            GuestRange::new(u64::MAX, 1).check(u64::MAX),
            Err(Errno::EFAULT)
        );
        assert_eq!(
            GuestRange::new(6, 4).check_aligned(memory, 4),
            Err(Errno::EINVAL)
        );
        assert_eq!(GuestRange::new(8, 4).check_aligned(memory, 4), Ok(8..12));
        assert_eq!(
            GuestRange::new(8, 4).check_aligned(memory, 3),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn windows_are_page_aligned_and_translate_only_inside() {
        let memory = 16 * PAGE_SIZE;
        assert_eq!(
            Window::new(PAGE_SIZE + 1, PAGE_SIZE, memory),
            Err(Errno::EINVAL)
        );
        assert_eq!(Window::new(0, 0, memory), Err(Errno::EINVAL));
        assert_eq!(Window::new(0, 100, memory), Err(Errno::EINVAL));
        assert_eq!(
            Window::new(15 * PAGE_SIZE, 2 * PAGE_SIZE, memory),
            Err(Errno::EFAULT)
        );
        let window = Window::new(4 * PAGE_SIZE, 2 * PAGE_SIZE, memory).unwrap();
        let other = Window::new(5 * PAGE_SIZE, PAGE_SIZE, memory).unwrap();
        let apart = Window::new(6 * PAGE_SIZE, PAGE_SIZE, memory).unwrap();
        assert!(window.overlaps(other) && other.overlaps(window));
        assert!(!window.overlaps(apart));

        let base = 0x7000_0000_0000;
        let inside = GuestRange::new(4 * PAGE_SIZE + 8, 64);
        assert_eq!(window.translate(inside, base), Ok(base + 4 * PAGE_SIZE + 8));
        let whole = GuestRange::new(4 * PAGE_SIZE, 2 * PAGE_SIZE);
        assert_eq!(window.translate(whole, base), Ok(base + 4 * PAGE_SIZE));
        for outside in [
            GuestRange::new(4 * PAGE_SIZE - 1, 2),
            GuestRange::new(6 * PAGE_SIZE - 1, 2),
            GuestRange::new(0, 8),
            GuestRange::new(u64::MAX, 2),
        ] {
            assert_eq!(
                window.translate(outside, base),
                Err(Errno::EFAULT),
                "{outside:?}"
            );
        }
        assert_eq!(window.translate(inside, u64::MAX - 8), Err(Errno::EFAULT));
    }

    #[test]
    fn string_lists_split_on_nul() {
        let parts: Vec<&str> = split_strings(b"ls\0-l\0\0", 8).unwrap().collect();
        assert_eq!(parts, ["ls", "-l", ""]);
        assert_eq!(split_strings(b"", 8).unwrap().count(), 0);
        assert_eq!(split_strings(b"\0", 8).unwrap().collect::<Vec<_>>(), [""]);
        assert!(matches!(split_strings(b"ls", 8), Err(Errno::EINVAL)));
        assert!(matches!(split_strings(b"\xff\0", 8), Err(Errno::EINVAL)));
        assert!(matches!(split_strings(b"a\0b\0c\0", 2), Err(Errno::E2BIG)));
    }
}
