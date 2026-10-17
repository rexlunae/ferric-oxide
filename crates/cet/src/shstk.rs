//! Supervisor shadow-stack layout: page encoding, tokens and slot arithmetic.
//!
//! With shadow stacks on, `call` pushes its return address to the normal stack and to the shadow
//! stack (at `SSP`), and `ret` pops both and raises #CP when they differ. The kernel needs a few
//! pieces of arithmetic for that, all pure and tested here:
//!
//! - **Page encoding.** A shadow-stack page is a present leaf page with R/W = 0 and Dirty = 1
//!   ([`SHADOW_STACK_PTE_FLAGS`], [`is_supervisor_shadow_stack_pte`]). Ordinary writes to it
//!   fault; only shadow-stack operations (`call`, `ret`, interrupt delivery, the token
//!   instructions) may write it.
//! - **Supervisor shadow-stack token** ([`supervisor_token`]). The 8-byte word at the top of a
//!   ring-0 shadow stack holding its own address, with bit 0 as the busy flag. The CPU checks
//!   and sets it when it loads `SSP` from `IA32_PL0_SSP` or the interrupt SSP table.
//! - **Restore token** ([`restore_token`]). What `RSTORSSP` expects at the address it switches
//!   to: the address just above the token, with bit 0 set for 64-bit mode. `SAVEPREVSSP` leaves
//!   one on the stack being switched away from, 8 bytes below its `SSP`
//!   ([`restore_token_address`]).
//! - **Initial contents** of a new context's shadow stack ([`InitialShadowStack`]), so that its
//!   first returns match the hand-built frames on its normal stack.
//! - **Slot windows** ([`SlotWindow`]): one virtual slot per kernel stack, found from the stack's
//!   physical address, with an unmapped guard below each shadow stack.
//!
//! Specification: Intel 64 and IA-32 Architectures Software Developer's Manual, Volume 1,
//! chapter 17 ("Control-flow Enforcement Technology", sections on shadow-stack pages, the
//! supervisor shadow-stack token and switching shadow stacks); Volume 2, `RSTORSSP`,
//! `SAVEPREVSSP` and `RDSSP`; Volume 3A, chapter 4 (shadow-stack accesses in paging) and
//! section 6.14.5 (interrupt SSP table); Volume 4, table 2-2 (`IA32_PL0_SSP`,
//! `IA32_INTERRUPT_SSP_TABLE_ADDR`).
//!
//! # Examples
//!
//! ```
//! use ferric_cet::shstk::{InitialShadowStack, restore_token, supervisor_token};
//!
//! let top = 0xFFFF_FF80_0000_4000;
//! // A new kernel thread: `ret` from the switch code lands on `func`.
//! let image = InitialShadowStack::new(top, &[0xFFFF_FFFF_8010_0000]).unwrap();
//! assert_eq!(image.ssp(), top - 24);
//! assert_eq!(
//!     image.entries(),
//!     &[
//!         restore_token(top - 24).unwrap(),
//!         0xFFFF_FFFF_8010_0000,
//!         supervisor_token(top - 8).unwrap(),
//!     ]
//! );
//! ```

use core::fmt;

/// `IA32_PL0_SSP`: the shadow-stack pointer the CPU loads on a transition to ring 0 that does not
/// use an IST stack.
pub const IA32_PL0_SSP: u32 = 0x6A4;

/// `IA32_PL1_SSP`: the ring-1 shadow-stack pointer. The kernel keeps it at zero.
pub const IA32_PL1_SSP: u32 = 0x6A5;

/// `IA32_PL2_SSP`: the ring-2 shadow-stack pointer. The kernel keeps it at zero.
pub const IA32_PL2_SSP: u32 = 0x6A6;

/// `IA32_PL3_SSP`: the user-mode shadow-stack pointer, saved by the CPU on entry to ring 0.
pub const IA32_PL3_SSP: u32 = 0x6A7;

/// `IA32_INTERRUPT_SSP_TABLE_ADDR`: linear address of the interrupt SSP table, eight 8-byte
/// entries of which entry `n` (1 to 7) is the shadow-stack pointer for IST stack `n`.
pub const IA32_INTERRUPT_SSP_TABLE_ADDR: u32 = 0x6A8;

/// Number of entries in the interrupt SSP table, including the unused entry 0.
pub const INTERRUPT_SSP_TABLE_ENTRIES: usize = 8;

/// Size of one shadow-stack entry (a return address or a token) in 64-bit mode.
pub const ENTRY_BYTES: u64 = 8;

/// Page-table entry bit 0: present.
pub const PTE_PRESENT: u64 = 1 << 0;
/// Page-table entry bit 1: writable (R/W).
pub const PTE_WRITABLE: u64 = 1 << 1;
/// Page-table entry bit 2: user-accessible (U/S).
pub const PTE_USER: u64 = 1 << 2;
/// Page-table entry bit 5: accessed.
pub const PTE_ACCESSED: u64 = 1 << 5;
/// Page-table entry bit 6: dirty.
pub const PTE_DIRTY: u64 = 1 << 6;
/// Page-table entry bit 63: execute disable.
pub const PTE_NO_EXECUTE: u64 = 1 << 63;

/// Flags of a supervisor shadow-stack page: present, R/W = 0 and Dirty = 1, which is what makes
/// it a shadow-stack page, plus accessed (so the CPU never has to set it) and execute-disable.
/// The user bit stays clear.
pub const SHADOW_STACK_PTE_FLAGS: u64 = PTE_PRESENT | PTE_ACCESSED | PTE_DIRTY | PTE_NO_EXECUTE;

/// Bit 0 of a supervisor shadow-stack token: the stack is in use on some CPU.
pub const SUPERVISOR_TOKEN_BUSY: u64 = 1 << 0;

/// Bit 0 of a restore token: the token was made in 64-bit mode.
pub const RESTORE_TOKEN_64BIT: u64 = 1 << 0;

/// The most return addresses [`InitialShadowStack::new`] takes: `func` and `enter_usermode`.
pub const MAX_INITIAL_RETURNS: usize = 2;

/// Whether a leaf page-table entry maps a shadow-stack page: present, R/W = 0, Dirty = 1.
///
/// The CPU also requires R/W = 1 in every higher-level entry on the way, which the kernel's
/// page-table entries always have.
#[inline]
pub const fn is_shadow_stack_pte(pte: u64) -> bool {
    pte & PTE_PRESENT != 0 && pte & PTE_WRITABLE == 0 && pte & PTE_DIRTY != 0
}

/// Whether a leaf page-table entry maps a supervisor shadow-stack page: a shadow-stack page
/// ([`is_shadow_stack_pte`]) without the user bit.
#[inline]
pub const fn is_supervisor_shadow_stack_pte(pte: u64) -> bool {
    is_shadow_stack_pte(pte) && pte & PTE_USER == 0
}

/// Why a shadow-stack address or layout is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutError {
    /// A token or shadow-stack pointer is not 8-byte aligned.
    Misaligned {
        /// The address.
        addr: u64,
    },
    /// The layout would wrap around the address space.
    Overflow,
    /// More return addresses than [`MAX_INITIAL_RETURNS`] were given.
    TooManyReturns {
        /// How many were given.
        count: usize,
    },
    /// A slot key is not aligned to the window's key size.
    KeyMisaligned {
        /// The key (a physical address).
        key: u64,
    },
    /// A slot key lies beyond what the window covers.
    OutsideWindow {
        /// The key (a physical address).
        key: u64,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Misaligned { addr } => {
                write!(f, "shadow-stack address {addr:#x} is not 8-byte aligned")
            }
            Self::Overflow => f.write_str("shadow-stack layout wraps around the address space"),
            Self::TooManyReturns { count } => write!(
                f,
                "{count} initial return addresses given, at most {MAX_INITIAL_RETURNS} fit"
            ),
            Self::KeyMisaligned { key } => {
                write!(
                    f,
                    "shadow-stack slot key {key:#x} is not aligned to the key size"
                )
            }
            Self::OutsideWindow { key } => write!(
                f,
                "shadow-stack slot key {key:#x} lies beyond the shadow-stack window"
            ),
        }
    }
}

impl core::error::Error for LayoutError {}

/// Checks that `addr` is 8-byte aligned.
const fn aligned(addr: u64) -> Result<u64, LayoutError> {
    if addr.is_multiple_of(ENTRY_BYTES) {
        Ok(addr)
    } else {
        Err(LayoutError::Misaligned { addr })
    }
}

/// The supervisor shadow-stack token for a token stored at `addr`, not busy: the address itself.
///
/// # Errors
///
/// [`LayoutError::Misaligned`] when `addr` is not 8-byte aligned.
pub const fn supervisor_token(addr: u64) -> Result<u64, LayoutError> {
    aligned(addr)
}

/// The restore token `RSTORSSP` accepts at `addr` in 64-bit mode: the address of the entry just
/// above it, with [`RESTORE_TOKEN_64BIT`] set. After `RSTORSSP` and `SAVEPREVSSP`, `SSP` is
/// `addr + 8`.
///
/// # Errors
///
/// [`LayoutError::Misaligned`] when `addr` is not 8-byte aligned, [`LayoutError::Overflow`] when
/// `addr` is the last entry of the address space.
pub const fn restore_token(addr: u64) -> Result<u64, LayoutError> {
    match aligned(addr) {
        Ok(addr) => match addr.checked_add(ENTRY_BYTES) {
            Some(above) => Ok(above | RESTORE_TOKEN_64BIT),
            None => Err(LayoutError::Overflow),
        },
        Err(err) => Err(err),
    }
}

/// Where `SAVEPREVSSP` leaves the restore token of a shadow stack whose `SSP` was `ssp` when
/// `RSTORSSP` switched away from it: 8 bytes below. That address is what a context saves, and
/// what `RSTORSSP` takes to switch back.
///
/// # Errors
///
/// [`LayoutError::Misaligned`] when `ssp` is not 8-byte aligned, [`LayoutError::Overflow`] when
/// `ssp` is 0.
pub const fn restore_token_address(ssp: u64) -> Result<u64, LayoutError> {
    match aligned(ssp) {
        Ok(ssp) => match ssp.checked_sub(ENTRY_BYTES) {
            Some(addr) => Ok(addr),
            None => Err(LayoutError::Overflow),
        },
        Err(err) => Err(err),
    }
}

/// The initial contents of a new context's shadow stack.
///
/// From the top down: the supervisor shadow-stack token in the highest entry, then the return
/// addresses the context's hand-built normal stack will `ret` to, last-popped first, then a
/// restore token. The context's saved `SSP` is the restore token's address, so the context
/// switch's `RSTORSSP` and `SAVEPREVSSP` leave `SSP` at the first return address, and once every
/// return has popped, `SSP` points at the supervisor token, as it must when the context leaves
/// for user space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialShadowStack {
    ssp: u64,
    entries: [u64; MAX_INITIAL_RETURNS + 2],
    len: usize,
}

impl InitialShadowStack {
    /// Lays out a shadow stack that ends (exclusively) at `top`, for a context whose first
    /// returns pop `returns` in order: `returns[0]` first.
    ///
    /// # Errors
    ///
    /// [`LayoutError::TooManyReturns`] for more than [`MAX_INITIAL_RETURNS`] addresses,
    /// [`LayoutError::Misaligned`] when `top` is not 8-byte aligned and
    /// [`LayoutError::Overflow`] when the stack would reach below address 0.
    pub fn new(top: u64, returns: &[u64]) -> Result<Self, LayoutError> {
        if returns.len() > MAX_INITIAL_RETURNS {
            return Err(LayoutError::TooManyReturns {
                count: returns.len(),
            });
        }
        let top = aligned(top)?;
        let len = returns.len() + 2;
        // `len` is at most 4, so neither the cast nor the product can overflow.
        let bytes = len as u64 * ENTRY_BYTES;
        let ssp = top.checked_sub(bytes).ok_or(LayoutError::Overflow)?;
        let mut entries = [0; MAX_INITIAL_RETURNS + 2];
        let (restore, rest) = entries.split_at_mut(1);
        let (middle, token) = rest.split_at_mut(returns.len());
        restore[0] = restore_token(ssp)?;
        middle.copy_from_slice(returns);
        token[0] = supervisor_token(top - ENTRY_BYTES)?;
        Ok(Self { ssp, entries, len })
    }

    /// The `SSP` to save for the context: the address of the restore token, the lowest entry.
    pub const fn ssp(&self) -> u64 {
        self.ssp
    }

    /// The entries to write, from [`ssp`](Self::ssp) upwards: entry `i` goes at `ssp + 8 * i`.
    pub fn entries(&self) -> &[u64] {
        &self.entries[..self.len]
    }
}

/// A virtual window of equal-sized shadow-stack slots, one for each naturally aligned physical
/// block of `1 << key_shift` bytes (a kernel stack), so a slot needs no allocator: its address
/// follows from the stack's.
///
/// Slot `i`, for key `i << key_shift`, spans `base + (i << slot_shift)` up to the next slot. The
/// kernel maps the shadow stack in the upper part of a slot and leaves the lower part unmapped
/// as a guard, so an overflowing shadow stack faults instead of running into its neighbour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotWindow {
    /// The window's first virtual address.
    pub base: u64,
    /// The window's size in bytes.
    pub len: u64,
    /// Log2 of the size and alignment of the physical blocks that key the slots.
    pub key_shift: u32,
    /// Log2 of a slot's size in bytes.
    pub slot_shift: u32,
}

impl SlotWindow {
    /// One past the highest key the window has a slot for: every key below it that is a
    /// multiple of `1 << key_shift` has one. Saturates at `u64::MAX`.
    pub const fn key_limit(&self) -> u64 {
        let slots = match self.len.checked_shr(self.slot_shift) {
            Some(slots) => slots,
            None => 0,
        };
        match slots.checked_shl(self.key_shift) {
            // A shift that loses bits overflowed; `checked_shl` only rejects large shift counts.
            Some(limit) if limit >> self.key_shift == slots => limit,
            Some(_) | None => u64::MAX,
        }
    }

    /// The end (exclusive) of the slot for `key`, i.e. the top of the shadow stack in it.
    ///
    /// # Errors
    ///
    /// [`LayoutError::KeyMisaligned`] when `key` is not a multiple of `1 << key_shift`, and
    /// [`LayoutError::OutsideWindow`] when its slot does not fit in the window (or the shifts
    /// are out of range).
    pub const fn slot_top(&self, key: u64) -> Result<u64, LayoutError> {
        let Some(key_size) = 1u64.checked_shl(self.key_shift) else {
            return Err(LayoutError::OutsideWindow { key });
        };
        if !key.is_multiple_of(key_size) {
            return Err(LayoutError::KeyMisaligned { key });
        }
        let index = key >> self.key_shift;
        let Some(slot_size) = 1u64.checked_shl(self.slot_shift) else {
            return Err(LayoutError::OutsideWindow { key });
        };
        // The slot's end offset, (index + 1) * slot_size, must stay within the window.
        let Some(end) = index.checked_add(1) else {
            return Err(LayoutError::OutsideWindow { key });
        };
        let Some(end) = end.checked_mul(slot_size) else {
            return Err(LayoutError::OutsideWindow { key });
        };
        if end > self.len {
            return Err(LayoutError::OutsideWindow { key });
        }
        match self.base.checked_add(end) {
            Some(top) => Ok(top),
            None => Err(LayoutError::OutsideWindow { key }),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::ToString;

    use super::*;

    #[test]
    fn msr_numbers() {
        // SDM Volume 4, table 2-2.
        assert_eq!(IA32_PL0_SSP, 0x6A4);
        assert_eq!(IA32_PL3_SSP, 0x6A7);
        assert_eq!(IA32_INTERRUPT_SSP_TABLE_ADDR, 0x6A8);
    }

    #[test]
    fn pte_encoding() {
        assert!(is_supervisor_shadow_stack_pte(SHADOW_STACK_PTE_FLAGS));
        assert!(is_supervisor_shadow_stack_pte(
            SHADOW_STACK_PTE_FLAGS | 0x0000_0012_3456_7000
        ));
        // Writable pages are never shadow-stack pages.
        assert!(!is_shadow_stack_pte(SHADOW_STACK_PTE_FLAGS | PTE_WRITABLE));
        // Read-only but clean pages are ordinary read-only pages.
        assert!(!is_shadow_stack_pte(SHADOW_STACK_PTE_FLAGS & !PTE_DIRTY));
        assert!(!is_shadow_stack_pte(SHADOW_STACK_PTE_FLAGS & !PTE_PRESENT));
        // A user shadow-stack page is not a supervisor one.
        assert!(is_shadow_stack_pte(SHADOW_STACK_PTE_FLAGS | PTE_USER));
        assert!(!is_supervisor_shadow_stack_pte(
            SHADOW_STACK_PTE_FLAGS | PTE_USER
        ));
        assert_eq!(SHADOW_STACK_PTE_FLAGS, 0x8000_0000_0000_0061);
    }

    #[test]
    fn tokens() {
        let addr = 0xFFFF_FF80_0001_3FF8;
        assert_eq!(supervisor_token(addr), Ok(addr));
        assert_eq!(supervisor_token(addr).unwrap() & SUPERVISOR_TOKEN_BUSY, 0);
        assert_eq!(
            supervisor_token(addr + 4),
            Err(LayoutError::Misaligned { addr: addr + 4 })
        );
        assert_eq!(restore_token(addr - 8), Ok(addr | 1));
        assert_eq!(
            restore_token(addr - 1),
            Err(LayoutError::Misaligned { addr: addr - 1 })
        );
        assert_eq!(restore_token(u64::MAX - 7), Err(LayoutError::Overflow));
        assert_eq!(restore_token_address(addr), Ok(addr - 8));
        assert_eq!(restore_token_address(0), Err(LayoutError::Overflow));
        assert_eq!(
            restore_token_address(addr + 2),
            Err(LayoutError::Misaligned { addr: addr + 2 })
        );
    }

    /// Replays RSTORSSP's token check (SDM Volume 2): the token, masked of bit 0 and minus 8,
    /// rounded down to 8, must be the token's own address, and bit 1 must be clear.
    fn rstorssp_accepts(addr: u64, token: u64) -> bool {
        token & 0b11 == RESTORE_TOKEN_64BIT && ((token & !1) - 8) & !7 == addr
    }

    #[test]
    fn restore_token_round_trip() {
        // What SAVEPREVSSP leaves for an SSP is what RSTORSSP takes back.
        for ssp in [0x1000u64, 0xFFFF_FF80_0000_4000, 0xFFFF_FFFF_FFFF_FFF8] {
            let addr = restore_token_address(ssp).unwrap();
            let token = restore_token(addr).unwrap();
            assert!(rstorssp_accepts(addr, token));
            assert_eq!(token & !RESTORE_TOKEN_64BIT, ssp);
        }
    }

    #[test]
    fn initial_kernel_thread() {
        let top = 0xFFFF_FF80_0000_4000;
        let image = InitialShadowStack::new(top, &[0xAAAA]).unwrap();
        assert_eq!(image.ssp(), top - 24);
        assert_eq!(image.entries(), &[(top - 16) | 1, 0xAAAA, top - 8]);
        assert!(rstorssp_accepts(image.ssp(), image.entries()[0]));
    }

    #[test]
    fn initial_user_context_unwinds_to_the_token() {
        let top = 0xFFFF_FF80_0000_8000;
        let func = 0xFFFF_FFFF_8000_1000;
        let enter_usermode = 0xFFFF_FFFF_8000_2000;
        let image = InitialShadowStack::new(top, &[func, enter_usermode]).unwrap();
        assert_eq!(image.entries().len(), 4);
        // Simulate RSTORSSP + SAVEPREVSSP, then the two returns.
        let entry = |ssp: u64| image.entries()[((ssp - image.ssp()) / 8) as usize];
        assert!(rstorssp_accepts(image.ssp(), entry(image.ssp())));
        let mut ssp = image.ssp() + 8;
        assert_eq!(entry(ssp), func);
        ssp += 8;
        assert_eq!(entry(ssp), enter_usermode);
        ssp += 8;
        // Back at the supervisor token, which holds its own address and is not busy.
        assert_eq!(ssp, top - 8);
        assert_eq!(entry(ssp), ssp);
    }

    #[test]
    fn initial_layout_errors() {
        assert_eq!(
            InitialShadowStack::new(0x1000, &[1, 2, 3]),
            Err(LayoutError::TooManyReturns { count: 3 })
        );
        assert_eq!(
            InitialShadowStack::new(0x1004, &[]),
            Err(LayoutError::Misaligned { addr: 0x1004 })
        );
        assert_eq!(
            InitialShadowStack::new(16, &[1]),
            Err(LayoutError::Overflow)
        );
        let empty = InitialShadowStack::new(0x1000, &[]).unwrap();
        assert_eq!(empty.entries(), &[(0x1000 - 8) | 1, 0x1000 - 8]);
    }

    const WINDOW: SlotWindow = SlotWindow {
        base: 0xFFFF_FF80_0000_0000,
        len: 1 << 38,
        key_shift: 16,
        slot_shift: 14,
    };

    #[test]
    fn slots() {
        assert_eq!(WINDOW.slot_top(0), Ok(WINDOW.base + 0x4000));
        assert_eq!(WINDOW.slot_top(0x1_0000), Ok(WINDOW.base + 0x8000));
        assert_eq!(
            WINDOW.slot_top(0x1234_0000),
            Ok(WINDOW.base + (0x1234 + 1) * 0x4000)
        );
        // The last 64 KiB block below 1 TiB still fits; the next does not.
        let last = (1u64 << 40) - 0x1_0000;
        assert_eq!(WINDOW.slot_top(last), Ok(WINDOW.base + WINDOW.len));
        assert_eq!(
            WINDOW.slot_top(1 << 40),
            Err(LayoutError::OutsideWindow { key: 1 << 40 })
        );
        assert_eq!(
            WINDOW.slot_top(0x8000),
            Err(LayoutError::KeyMisaligned { key: 0x8000 })
        );
    }

    #[test]
    fn key_limit() {
        assert_eq!(WINDOW.key_limit(), 1 << 40);
        let last = WINDOW.key_limit() - (1 << WINDOW.key_shift);
        assert!(WINDOW.slot_top(last).is_ok());
        assert!(WINDOW.slot_top(WINDOW.key_limit()).is_err());
        let huge = SlotWindow {
            len: u64::MAX,
            slot_shift: 0,
            ..WINDOW
        };
        assert_eq!(huge.key_limit(), u64::MAX);
    }

    #[test]
    fn slots_never_overlap() {
        let a = WINDOW.slot_top(0x10_0000).unwrap();
        let b = WINDOW.slot_top(0x11_0000).unwrap();
        assert_eq!(b - a, 1 << WINDOW.slot_shift);
    }

    #[test]
    fn slot_overflow_is_an_error() {
        let wide = SlotWindow {
            base: u64::MAX - 0x3FFF,
            len: u64::MAX,
            key_shift: 16,
            slot_shift: 14,
        };
        assert_eq!(
            wide.slot_top(0x1_0000),
            Err(LayoutError::OutsideWindow { key: 0x1_0000 })
        );
        let bad_shift = SlotWindow {
            key_shift: 64,
            ..WINDOW
        };
        assert_eq!(
            bad_shift.slot_top(0),
            Err(LayoutError::OutsideWindow { key: 0 })
        );
    }

    #[test]
    fn messages() {
        assert_eq!(
            LayoutError::Misaligned { addr: 0x1004 }.to_string(),
            "shadow-stack address 0x1004 is not 8-byte aligned"
        );
        assert!(
            LayoutError::OutsideWindow { key: 1 << 40 }
                .to_string()
                .contains("0x10000000000")
        );
    }
}
