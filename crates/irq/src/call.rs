//! The `SYS_CALL`s on an `irq:` handle (Ferric Oxide, `kernel.irq_percpu`), in one place for
//! the kernel and the drivers.
//!
//! | Call | On | Metadata | Returns |
//! | --- | --- | --- | --- |
//! | [`Call::Move`] | a handle for a vector reserved on a CPU (`irq:cpu-XX/N`) | `[MOVE, cpu]` | [`Moved::to_word`] |
//!
//! [`Call::Move`] steers the vector to another CPU. The kernel reserves a free vector there,
//! rewrites every interrupt remapping entry that delivered the old vector to the old CPU so it
//! delivers the new one, and re-keys every handle for the old vector to the new one, so the
//! caller keeps its descriptor. The old vector is then given back. The handles report one
//! interrupt pending afterwards, so one that arrived on the old vector during the move is not
//! lost (it may show up as one spurious wakeup).
//!
//! [`Moved::remapped`] says how many remapping entries moved. When it is 0 the device's
//! interrupts are not remapped, and its MSI or MSI-X message names the CPU and vector itself:
//! the driver must then program the message for the new vector, which only the driver that owns
//! the reservation can do. A handle opened with `O_STAT` (which reserves nothing) can move a
//! vector only when remapping entries carry it.
//!
//! Errors: `EINVAL` for a legacy IRQ (0 to 15 on the BSP) or a vector nobody reserved,
//! `ENOENT` for a CPU that is not running, `EEXIST` when the target CPU has no free vector,
//! `EOPNOTSUPP` from an `O_STAT` handle when no remapping entry delivers the vector, or in a
//! kernel built without `kernel.irq_percpu`, and `EIO` when a remapping unit fails the
//! invalidation.

use core::fmt;

use crate::{CpuId, Irq};

/// The verb of [`Call::Move`].
const MOVE: u64 = 1;

/// A call on an `irq:` handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Call {
    /// Steer the handle's vector to logical CPU `cpu`.
    Move {
        /// The CPU to move to.
        cpu: CpuId,
    },
}

impl Call {
    /// The most metadata words a call takes.
    pub const MAX_WORDS: usize = 2;

    /// The metadata words, and how many of them are used.
    pub const fn encode(self) -> ([u64; Self::MAX_WORDS], usize) {
        match self {
            Self::Move { cpu } => ([MOVE, cpu.get() as u64], 2),
        }
    }

    /// The call in metadata `words`.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Verb`] for a missing or unknown verb, [`DecodeError::Length`] when the
    /// number of words does not match it, and [`DecodeError::Value`] for a CPU above 32 bits.
    pub fn decode(words: &[u64]) -> Result<Self, DecodeError> {
        match words {
            [MOVE, cpu] => u32::try_from(*cpu)
                .map(|cpu| Self::Move {
                    cpu: CpuId::new(cpu),
                })
                .map_err(|_| DecodeError::Value),
            [MOVE, ..] => Err(DecodeError::Length),
            _ => Err(DecodeError::Verb),
        }
    }
}

/// What [`Call::Move`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moved {
    /// The interrupt number on the new CPU (`irq:cpu-XX/N`): the handle's vector is now
    /// `irq + 32` there.
    pub irq: Irq,
    /// How many interrupt remapping entries now deliver to the new vector; 0 when the device's
    /// interrupts are not remapped.
    pub remapped: u32,
}

impl Moved {
    /// The call's return value: the IRQ in bits 7:0, the remapped count in bits 39:8.
    pub const fn to_word(self) -> u64 {
        self.irq.get() as u64 | ((self.remapped as u64) << 8)
    }

    /// The result in return value `word`, or `None` when it has bits above 39 set or an IRQ
    /// number out of range.
    pub const fn from_word(word: u64) -> Option<Self> {
        if word >> 40 != 0 {
            return None;
        }
        match Irq::new(word as u8) {
            Some(irq) => Some(Self {
                irq,
                remapped: (word >> 8) as u32,
            }),
            None => None,
        }
    }
}

/// Why metadata words are not a valid [`Call`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The verb is missing or unknown.
    Verb,
    /// The number of words does not match the verb.
    Length,
    /// A word has a value the call does not allow.
    Value,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Verb => "missing or unknown irq call verb",
            Self::Length => "wrong number of metadata words for the irq call",
            Self::Value => "an irq call word is out of range",
        })
    }
}

impl core::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    #[test]
    fn moves_round_trip() {
        let call = Call::Move { cpu: CpuId::new(3) };
        let (words, len) = call.encode();
        assert_eq!(&words[..len], [1, 3]);
        assert_eq!(Call::decode(&words[..len]), Ok(call));
        let moved = Moved {
            irq: Irq::new(223).unwrap(),
            remapped: u32::MAX,
        };
        assert_eq!(Moved::from_word(moved.to_word()), Some(moved));
        let none = Moved {
            irq: Irq::new(16).unwrap(),
            remapped: 0,
        };
        assert_eq!(none.to_word(), 16);
    }

    #[test]
    fn bad_calls_and_results_are_refused() {
        assert_eq!(Call::decode(&[]), Err(DecodeError::Verb));
        assert_eq!(Call::decode(&[2, 0]), Err(DecodeError::Verb));
        assert_eq!(Call::decode(&[1]), Err(DecodeError::Length));
        assert_eq!(Call::decode(&[1, 2, 3]), Err(DecodeError::Length));
        assert_eq!(Call::decode(&[1, 1 << 32]), Err(DecodeError::Value));
        // IRQ 224 does not exist, and nothing sits above bit 39.
        assert_eq!(Moved::from_word(224), None);
        assert_eq!(Moved::from_word(1 << 40), None);
        assert_ne!(
            DecodeError::Verb.to_string(),
            DecodeError::Value.to_string()
        );
        assert_ne!(
            DecodeError::Length.to_string(),
            DecodeError::Value.to_string()
        );
    }
}
