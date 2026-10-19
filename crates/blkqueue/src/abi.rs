//! The call that sets a disk's [`QueuePolicy`]: `SYS_CALL` on a disk or partition descriptor
//! of a disk scheme, with no payload and one metadata word, [`VERB`] in bits 0 to 7 and the
//! policy's number in bits 8 to 15. It applies to the whole disk, for every descriptor.
//!
//! The verb shares the first metadata word with the registered-buffer grant calls
//! (`ferric-blkgrant`'s `abi`, verbs 1 to 3), so a scheme tries [`decode`] first and passes
//! the words on when it returns `Ok(None)`.
//!
//! # Errors
//!
//! `EOPNOTSUPP` when the disk has no per-CPU queue pairs, `EINVAL` for an unknown policy or
//! extra words.

use core::fmt;

use crate::QueuePolicy;

/// The verb of the set-policy call, outside the grant calls' verbs.
pub const VERB: u64 = 0x80;

/// The call's metadata word for `policy`.
pub const fn encode(policy: QueuePolicy) -> [u64; 1] {
    [VERB | (policy as u64) << 8]
}

/// The policy a call's metadata words set, or `None` when they are some other call.
///
/// The kernel pads the words with zeros up to the three it passes a scheme, so trailing zero
/// words are accepted.
///
/// # Errors
///
/// [`DecodeError::Policy`] for an unknown policy number, and [`DecodeError::Length`] for
/// bits set above the policy or nonzero words after the first.
pub fn decode(words: &[u64]) -> Result<Option<QueuePolicy>, DecodeError> {
    let Some((&head, rest)) = words.split_first() else {
        return Ok(None);
    };
    if head & 0xff != VERB {
        return Ok(None);
    }
    if head >> 16 != 0 || rest.iter().any(|&word| word != 0) {
        return Err(DecodeError::Length);
    }
    QueuePolicy::from_raw((head >> 8) & 0xff)
        .map(Some)
        .ok_or(DecodeError::Policy)
}

/// Why a set-policy call's words are not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The policy number is unknown.
    Policy,
    /// Bits above the policy, or words after the first, are set.
    Length,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Policy => "unknown disk queue policy",
            Self::Length => "metadata words do not fit the disk queue policy call",
        })
    }
}

impl core::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_round_trips_with_padding() {
        for policy in [
            QueuePolicy::Adaptive,
            QueuePolicy::MainQueue,
            QueuePolicy::Workers,
        ] {
            let [word] = encode(policy);
            assert_eq!(decode(&[word]), Ok(Some(policy)));
            assert_eq!(decode(&[word, 0, 0]), Ok(Some(policy)));
        }
    }

    #[test]
    fn other_calls_pass_through() {
        assert_eq!(decode(&[]), Ok(None));
        // A grant read: verb 1, then LBA and length.
        assert_eq!(decode(&[1, 2048, 4096]), Ok(None));
        assert_eq!(decode(&[3, 0, 0]), Ok(None));
    }

    #[test]
    fn malformed_calls_are_refused() {
        assert_eq!(decode(&[VERB | 9 << 8]), Err(DecodeError::Policy));
        assert_eq!(decode(&[VERB | 1 << 16]), Err(DecodeError::Length));
        assert_eq!(decode(&[VERB, 1]), Err(DecodeError::Length));
    }
}
