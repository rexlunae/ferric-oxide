//! The anti-forensic information splitter's merge step (LUKS1 On-Disk Format Specification
//! 1.2.3, section 2.4, with the H1 diffusion function of section 2.4.1).
//!
//! A key of `n` bytes is stored as `stripes` blocks of `n` bytes. Merging XORs each block but the
//! last into a running value and diffuses it with H1; the key is the final value XOR the last
//! block. LUKS2 keyslots of AF type `luks1` use the same function.

use alloc::vec;

use zeroize::Zeroizing;

use crate::hash::{HashAlg, MAX_DIGEST_LEN};

/// Recovers a `key_len`-byte key from `split`, which holds `stripes` blocks of `key_len` bytes
/// (any bytes after them are ignored). Returns `None` when `hash` is not implemented, `key_len`
/// or `stripes` is 0, or `split` is too short.
pub fn merge(
    hash: &HashAlg,
    split: &[u8],
    key_len: usize,
    stripes: usize,
) -> Option<Zeroizing<alloc::vec::Vec<u8>>> {
    if key_len == 0 || stripes == 0 || split.len() < key_len.checked_mul(stripes)? {
        return None;
    }
    let mut value = Zeroizing::new(vec![0u8; key_len]);
    for block in split.chunks_exact(key_len).take(stripes - 1) {
        xor(&mut value, block);
        diffuse(hash, &mut value)?;
    }
    xor(
        &mut value,
        &split[(stripes - 1) * key_len..stripes * key_len],
    );
    Some(value)
}

fn xor(into: &mut [u8], from: &[u8]) {
    for (a, b) in into.iter_mut().zip(from) {
        *a ^= b;
    }
}

/// H1: each digest-sized chunk `d_i` becomes `P(i || d_i)`, cropped to the chunk's length, with
/// `i` a big-endian 32-bit counter from 0.
fn diffuse(hash: &HashAlg, value: &mut [u8]) -> Option<()> {
    let len = hash.output_len()?;
    let mut out = Zeroizing::new([0u8; MAX_DIGEST_LEN]);
    for (index, chunk) in value.chunks_mut(len).enumerate() {
        let counter = (index as u32).to_be_bytes();
        hash.digest(&[&counter, chunk], &mut out)?;
        let n = chunk.len();
        chunk.copy_from_slice(&out[..n]);
    }
    Some(())
}

/// The split step, for tests that build key material: `random` provides the first
/// `stripes - 1` blocks.
#[cfg(test)]
pub(crate) fn split(
    hash: &HashAlg,
    key: &[u8],
    random: &[u8],
    stripes: usize,
) -> alloc::vec::Vec<u8> {
    let n = key.len();
    let mut out = random[..n * (stripes - 1)].to_vec();
    let mut value = vec![0u8; n];
    for block in out.chunks_exact(n) {
        xor(&mut value, block);
        diffuse(hash, &mut value).expect("supported hash");
    }
    xor(&mut value, key);
    out.extend_from_slice(&value);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random(len: usize) -> alloc::vec::Vec<u8> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[test]
    fn merge_inverts_split() {
        let key: alloc::vec::Vec<u8> = (0..64).collect();
        for hash in [HashAlg::Sha1, HashAlg::Sha256, HashAlg::Sha512] {
            // 64-byte keys exercise a cropped final chunk with SHA-1 (64 = 3 * 20 + 4).
            for (len, stripes) in [(32, 4000), (64, 4000), (16, 3), (5, 1)] {
                let split = split(&hash, &key[..len], &random(len * stripes), stripes);
                let merged = merge(&hash, &split, len, stripes).expect("supported");
                assert_eq!(&merged[..], &key[..len], "{hash:?} {len} {stripes}");
            }
        }
    }

    #[test]
    fn diffusion_matters() {
        // With one stripe the key is stored in the clear; with two, H1 changes it.
        let key = [7u8; 32];
        let mut stored = [0u8; 64];
        stored[32..].copy_from_slice(&key);
        assert_eq!(
            &merge(&HashAlg::Sha256, &stored[32..], 32, 1).unwrap()[..],
            &key
        );
        assert_ne!(&merge(&HashAlg::Sha256, &stored, 32, 2).unwrap()[..], &key);
    }

    #[test]
    fn bad_input_is_refused() {
        assert!(merge(&HashAlg::Sha256, &[0; 10], 32, 1).is_none());
        assert!(merge(&HashAlg::Sha256, &[0; 64], 32, 0).is_none());
        assert!(merge(&HashAlg::Sha256, &[0; 64], 0, 2).is_none());
        assert!(merge(&HashAlg::from_name("md5"), &[0; 64], 32, 2).is_none());
    }
}
