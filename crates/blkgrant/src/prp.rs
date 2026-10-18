//! NVMe data pointers for a buffer that is contiguous in the device's address space (a grant's
//! IOVA range), following the NVM Express Base Specification 2.0, section 4.1.1 ("Physical
//! Region Page Entry and List"), with a 4 KiB memory page size (`CC.MPS` = 0).
//!
//! A command names its buffer with two entries, PRP1 and PRP2. A buffer of one page needs only
//! PRP1; of two pages, PRP2 is the second page; of more, PRP2 points to a *PRP list*, one page
//! of 8-byte entries holding the address of every page after the first. One list page holds
//! [`LIST_ENTRIES`] entries, and this module never chains lists, so one command carries at
//! most `LIST_ENTRIES + 1` pages; the controller's own limit is [`max_transfer`].

use core::fmt;

/// The memory page size the driver programs (`CC.MPS` = 0).
pub const PAGE: u64 = 4096;

/// How many entries one PRP list page holds.
pub const LIST_ENTRIES: usize = (PAGE / 8) as usize;

/// Why a buffer cannot be described with one command's PRP entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PrpError {
    /// The buffer is empty.
    Empty,
    /// The buffer does not start on a page boundary.
    Unaligned,
    /// The buffer needs more list entries than the list given has room for.
    TooLong,
}

impl fmt::Display for PrpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty PRP buffer",
            Self::Unaligned => "PRP buffer not page-aligned",
            Self::TooLong => "PRP buffer longer than one list describes",
        })
    }
}

impl core::error::Error for PrpError {}

/// The pages a buffer of `len` bytes touches, when it starts on a page boundary.
const fn pages(len: u64) -> u64 {
    len.div_ceil(PAGE)
}

/// PRP1 and PRP2 for the `len` bytes at device address `addr`. When the buffer spans more than
/// two pages this writes the PRP list into `list` (whose device address is `list_addr`) and
/// points PRP2 at it.
///
/// # Errors
///
/// [`PrpError::Empty`] for no bytes, [`PrpError::Unaligned`] when `addr` is not page-aligned,
/// and [`PrpError::TooLong`] when the list needs more entries than `list` has.
pub fn data_pointers(
    addr: u64,
    len: u64,
    list: &mut [u64],
    list_addr: u64,
) -> Result<(u64, u64), PrpError> {
    if len == 0 {
        return Err(PrpError::Empty);
    }
    if !addr.is_multiple_of(PAGE) {
        return Err(PrpError::Unaligned);
    }
    let rest = pages(len) - 1;
    match rest {
        0 => Ok((addr, 0)),
        1 => Ok((addr, addr.checked_add(PAGE).ok_or(PrpError::TooLong)?)),
        _ => {
            let rest = usize::try_from(rest).map_err(|_| PrpError::TooLong)?;
            let entries = list.get_mut(..rest).ok_or(PrpError::TooLong)?;
            let mut page = addr;
            for entry in entries {
                page = page.checked_add(PAGE).ok_or(PrpError::TooLong)?;
                *entry = page;
            }
            Ok((addr, list_addr))
        }
    }
}

/// The most bytes one command may carry, from the controller's `MDTS` (Identify Controller
/// byte 77: a power of two in units of the minimum memory page size, 0 for no limit), capped
/// at what one PRP list describes.
pub fn max_transfer(mdts: u8, min_page: u64) -> u64 {
    let list_limit = (LIST_ENTRIES as u64 + 1) * PAGE;
    if mdts == 0 {
        return list_limit;
    }
    1u64.checked_shl(u32::from(mdts))
        .and_then(|n| n.checked_mul(min_page))
        .map_or(list_limit, |limit| limit.min(list_limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_and_two_pages_need_no_list() {
        let mut list = [0; 4];
        assert_eq!(
            data_pointers(0x8000, 512, &mut list, 0x1000),
            Ok((0x8000, 0))
        );
        assert_eq!(
            data_pointers(0x8000, 4096, &mut list, 0x1000),
            Ok((0x8000, 0))
        );
        assert_eq!(
            data_pointers(0x8000, 4097, &mut list, 0x1000),
            Ok((0x8000, 0x9000))
        );
        assert_eq!(list, [0; 4]);
    }

    #[test]
    fn longer_buffers_get_a_list_of_the_following_pages() {
        let mut list = [0; 4];
        let got = data_pointers(0x8000, 4 * 4096, &mut list, 0x1000);
        assert_eq!(got, Ok((0x8000, 0x1000)));
        assert_eq!(list, [0x9000, 0xa000, 0xb000, 0]);
    }

    #[test]
    fn buffers_outside_the_rules_are_refused() {
        let mut list = [0; 2];
        assert_eq!(data_pointers(0x8000, 0, &mut list, 0), Err(PrpError::Empty));
        assert_eq!(
            data_pointers(0x8200, 512, &mut list, 0),
            Err(PrpError::Unaligned)
        );
        assert_eq!(
            data_pointers(0x8000, 4 * 4096, &mut list, 0),
            Err(PrpError::TooLong)
        );
        assert_eq!(
            data_pointers(u64::MAX - 0xfff, 2 * 4096, &mut list, 0),
            Err(PrpError::TooLong)
        );
    }

    #[test]
    fn the_transfer_limit_follows_mdts_and_the_list() {
        assert_eq!(max_transfer(0, 4096), 513 * 4096);
        assert_eq!(max_transfer(5, 4096), 128 * 1024);
        assert_eq!(max_transfer(7, 4096), 512 * 1024);
        assert_eq!(max_transfer(12, 4096), 513 * 4096);
        assert_eq!(max_transfer(255, 4096), 513 * 4096);
    }
}
