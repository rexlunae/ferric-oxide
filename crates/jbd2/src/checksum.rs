//! The CRC32C that every JBD2 version 2 and 3 checksum (and ext4's own) uses.
//!
//! JBD2 starts the CRC at `0xFFFF_FFFF` and does not invert the result, then feeds the
//! journal UUID followed by the covered bytes. That is the standard CRC32C (which also starts
//! at `0xFFFF_FFFF` but inverts at the end) of the concatenation, inverted once more. The
//! convention was checked against volumes made by e2fsprogs 1.47 (see the crate docs).

use crc::{CRC_32_ISCSI, Crc};

static CRC32C: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

/// CRC32C over `parts`, one after another, in the form JBD2 and ext4 store: started at
/// `0xFFFF_FFFF` and not inverted at the end.
///
/// A JBD2 checksum is this over the journal UUID and then the covered bytes; ext4's
/// superblock checksum (`s_checksum`) is this over the superblock's first 1020 bytes.
///
/// # Examples
///
/// ```
/// // The standard CRC32C of "123456789" is 0xE3069283; the stored form is its inverse.
/// assert_eq!(ferric_jbd2::crc32c(&[b"1234", b"56789"]), !0xE306_9283);
/// ```
pub fn crc32c(parts: &[&[u8]]) -> u32 {
    let mut digest = CRC32C.digest();
    for part in parts {
        digest.update(part);
    }
    !digest.finalize()
}

/// [`crc32c`] over `parts`, carried on from `seed`, a value [`crc32c`] (or this function)
/// returned: `crc32c_seeded(crc32c(&[a]), &[b])` is `crc32c(&[a, b])`.
///
/// ext4 keeps such a seed in the superblock (`s_checksum_seed`, with the `metadata_csum_seed`
/// feature) and starts every metadata checksum from it rather than from the volume UUID.
///
/// # Examples
///
/// ```
/// use ferric_jbd2::{crc32c, crc32c_seeded};
///
/// let seed = crc32c(&[b"volume uuid"]);
/// assert_eq!(crc32c_seeded(seed, &[b"inode"]), crc32c(&[b"volume uuid", b"inode"]));
/// ```
pub fn crc32c_seeded(seed: u32, parts: &[&[u8]]) -> u32 {
    // The stored form is the bit-reflected register itself; `digest_with_initial` takes the
    // initial value unreflected and reflects it.
    let mut digest = CRC32C.digest_with_initial(seed.reverse_bits());
    for part in parts {
        digest.update(part);
    }
    // The algorithm XORs the register with 0xFFFF_FFFF at the end; undo that.
    !digest.finalize()
}

/// [`crc32c`] over `block` with the four bytes at `at` read as zero, after `prefix`: how
/// every checksum stored inside the block it covers is computed.
pub(crate) fn crc32c_without(prefix: &[&[u8]], block: &[u8], at: usize) -> u32 {
    let mut digest = CRC32C.digest();
    for part in prefix {
        digest.update(part);
    }
    digest.update(&block[..at]);
    digest.update(&[0; 4]);
    digest.update(&block[at + 4..]);
    !digest.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_unfinalized_castagnoli_crc() {
        // CRC32C("123456789") is 0xE3069283; JBD2's form is its inverse.
        assert_eq!(crc32c(&[b"123456789"]), !0xE306_9283);
        assert_eq!(crc32c(&[b"1234", b"56789"]), crc32c(&[b"123456789"]));
    }

    #[test]
    fn a_seeded_crc_carries_on_from_the_seed() {
        for split in 0..=9 {
            let (a, b) = b"123456789".split_at(split);
            assert_eq!(crc32c_seeded(crc32c(&[a]), &[b]), crc32c(&[b"123456789"]));
        }
        assert_eq!(crc32c_seeded(!0, &[b"123456789"]), crc32c(&[b"123456789"]));
    }

    #[test]
    fn a_stored_checksum_reads_as_zero() {
        let mut block = *b"abcdWXYZefgh";
        let expected = {
            block[4..8].copy_from_slice(&[0; 4]);
            crc32c(&[b"uuid", &block])
        };
        block[4..8].copy_from_slice(b"WXYZ");
        assert_eq!(crc32c_without(&[b"uuid"], &block, 4), expected);
    }
}
