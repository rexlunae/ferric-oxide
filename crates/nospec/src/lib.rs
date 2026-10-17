//! Spectre variant 1 (bounds check bypass) hardening for index lookups.
//!
//! A bounds check such as `if index >= len { return None }` stops the
//! architectural execution of an out-of-range access, but the CPU may still
//! speculate past the branch and load from `base + index` with an
//! attacker-chosen `index`, leaving a trace in the cache. The usual fix is to
//! clamp the index with a data dependency the CPU cannot predict around: after
//! the check, the index is ANDed with a mask that is all ones when
//! `index < len` and zero otherwise, computed without a branch. This is the
//! same technique as Linux's `array_index_nospec`, described in Intel's
//! "Bounds Check Bypass" guidance and AMD's "Software Techniques for Managing
//! Speculation" white paper.
//!
//! Use [`clamp`] directly after a bounds check on an index that comes from
//! user space, or [`get`] / [`get_mut`] for slices.
//!
//! # Examples
//!
//! ```
//! let table = [10, 20, 30];
//! let user_index = 1;
//! assert_eq!(ferric_nospec::get(&table, user_index), Some(&20));
//! assert_eq!(ferric_nospec::get(&table, 7), None);
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

/// Returns all ones when `index < len` and zero otherwise, without a branch
/// the CPU can speculate past.
#[inline(always)]
pub fn mask(index: usize, len: usize) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        let mask: usize;
        // SAFETY: `cmp` and `sbb` only read the two registers and write
        // `mask` and the flags, which the asm block declares; nothing touches
        // memory or the stack.
        unsafe {
            core::arch::asm!(
                "cmp {index}, {len}",
                "sbb {mask}, {mask}",
                index = in(reg) index,
                len = in(reg) len,
                mask = out(reg) mask,
                options(pure, nomem, nostack),
            );
        }
        mask
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        // Sign bit of `index | (len - 1 - index)` is set exactly when
        // `index >= len` (for `len <= isize::MAX`), turned into a full mask
        // by an arithmetic shift. `black_box` stops the compiler from turning
        // this back into a branch.
        let x = core::hint::black_box(index | len.wrapping_sub(1).wrapping_sub(index));
        (!(x as isize) >> (usize::BITS - 1)) as usize
    }
}

/// Returns `index` when `index < len` and zero otherwise, also under
/// speculation. Call it after the architectural bounds check so a mispredicted
/// check cannot load out of range.
#[inline(always)]
pub fn clamp(index: usize, len: usize) -> usize {
    index & mask(index, len)
}

/// [`slice::get`] with the index clamped against speculative out-of-bounds
/// loads.
#[inline(always)]
pub fn get<T>(slice: &[T], index: usize) -> Option<&T> {
    if index >= slice.len() {
        return None;
    }
    slice.get(clamp(index, slice.len()))
}

/// [`slice::get_mut`] with the index clamped against speculative
/// out-of-bounds loads.
#[inline(always)]
pub fn get_mut<T>(slice: &mut [T], index: usize) -> Option<&mut T> {
    let len = slice.len();
    if index >= len {
        return None;
    }
    slice.get_mut(clamp(index, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_matches_comparison() {
        let lens = [0, 1, 2, 3, 63, 64, 1 << 20, usize::MAX >> 1];
        for &len in &lens {
            for index in [
                0,
                1,
                2,
                len.wrapping_sub(1),
                len,
                len.wrapping_add(1),
                usize::MAX,
            ] {
                let want = if index < len { usize::MAX } else { 0 };
                assert_eq!(mask(index, len), want, "index={index} len={len}");
            }
        }
    }

    #[test]
    fn clamp_zeroes_out_of_range() {
        assert_eq!(clamp(5, 10), 5);
        assert_eq!(clamp(10, 10), 0);
        assert_eq!(clamp(usize::MAX, 10), 0);
    }

    #[test]
    fn get_mut_works() {
        let mut v = [1, 2, 3];
        *get_mut(&mut v, 2).unwrap() = 9;
        assert_eq!(v, [1, 2, 9]);
        assert!(get_mut(&mut v, 3).is_none());
    }
}
