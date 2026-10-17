//! Accounting of pinned pages.
//!
//! Registering a region pins its pages until the region closes, so pinning is limited: each
//! user ID other than root may pin [`Limits::DEFAULT_PER_USER`] pages, and everyone together a
//! quarter of memory ([`Limits::for_memory`]). The region charges its opener's user ID when it
//! registers and refunds it when it closes.

use alloc::collections::BTreeMap;
use core::fmt;

use crate::abi::Usage;

/// The user ID that no per-user limit applies to.
const ROOT: u32 = 0;

/// How many pages may be pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Pages each user ID other than root may pin.
    pub per_user: u64,
    /// Pages everyone together may pin.
    pub total: u64,
}

impl Limits {
    /// The per-user limit: 16 MiB of 4 KiB pages.
    pub const DEFAULT_PER_USER: u64 = 4096;

    /// The default limits on a machine with `memory_pages` pages of memory: the per-user
    /// default, and a quarter of memory in total.
    pub const fn for_memory(memory_pages: u64) -> Self {
        Self {
            per_user: Self::DEFAULT_PER_USER,
            total: memory_pages / 4,
        }
    }
}

/// Why pages cannot be pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BudgetError {
    /// The user ID would pass its limit.
    User {
        /// The user ID.
        uid: u32,
        /// Pages it has pinned.
        pinned: u64,
        /// Its limit.
        limit: u64,
    },
    /// Everyone together would pass the total limit.
    Total {
        /// Pages pinned by everyone.
        pinned: u64,
        /// The limit.
        limit: u64,
    },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User { uid, pinned, limit } => {
                write!(
                    f,
                    "user {uid} has {pinned} of {limit} pinnable pages in use"
                )
            }
            Self::Total { pinned, limit } => {
                write!(f, "{pinned} of {limit} pinnable pages are in use")
            }
        }
    }
}

impl core::error::Error for BudgetError {}

/// Pinned pages, per user ID and in total.
#[derive(Debug, Default)]
pub struct Budget {
    by_user: BTreeMap<u32, u64>,
    total: u64,
}

impl Budget {
    /// Nothing pinned.
    pub const fn new() -> Self {
        Self {
            by_user: BTreeMap::new(),
            total: 0,
        }
    }

    /// Charges `pages` to `uid`, unless that passes a limit.
    ///
    /// # Errors
    ///
    /// [`BudgetError::User`] when `uid` is not root and would pass `limits.per_user`, and
    /// [`BudgetError::Total`] when everyone together would pass `limits.total`. Nothing is
    /// charged then.
    pub fn charge(&mut self, uid: u32, pages: u64, limits: Limits) -> Result<(), BudgetError> {
        let pinned = self.pinned(uid);
        let user = pinned.checked_add(pages);
        if uid != ROOT && user.is_none_or(|n| n > limits.per_user) {
            return Err(BudgetError::User {
                uid,
                pinned,
                limit: limits.per_user,
            });
        }
        let total = self.total.checked_add(pages);
        let (Some(user), Some(total)) = (user, total.filter(|&n| n <= limits.total)) else {
            return Err(BudgetError::Total {
                pinned: self.total,
                limit: limits.total,
            });
        };
        if pages != 0 {
            self.by_user.insert(uid, user);
        }
        self.total = total;
        Ok(())
    }

    /// Gives back `pages` that [`Budget::charge`] charged to `uid`.
    pub fn refund(&mut self, uid: u32, pages: u64) {
        let left = self.pinned(uid).saturating_sub(pages);
        if left == 0 {
            self.by_user.remove(&uid);
        } else {
            self.by_user.insert(uid, left);
        }
        self.total = self.total.saturating_sub(pages);
    }

    /// Pages `uid` has pinned.
    pub fn pinned(&self, uid: u32) -> u64 {
        self.by_user.get(&uid).copied().unwrap_or(0)
    }

    /// Pages everyone has pinned.
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// What [`crate::abi::Request::Usage`] reports to `uid`.
    pub fn usage(&self, uid: u32, limits: Limits) -> Usage {
        Usage {
            pinned: self.pinned(uid),
            limit: (uid != ROOT).then_some(limits.per_user),
            total: self.total,
            total_limit: limits.total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        per_user: 10,
        total: 25,
    };

    #[test]
    fn default_limits() {
        let limits = Limits::for_memory(1 << 20);
        assert_eq!(limits.per_user, 4096);
        assert_eq!(limits.total, 1 << 18);
    }

    #[test]
    fn per_user_limit() {
        let mut b = Budget::new();
        assert_eq!(b.charge(1000, 6, LIMITS), Ok(()));
        assert_eq!(
            b.charge(1000, 5, LIMITS),
            Err(BudgetError::User {
                uid: 1000,
                pinned: 6,
                limit: 10
            })
        );
        assert_eq!(b.charge(1000, 4, LIMITS), Ok(()));
        assert_eq!(b.charge(1001, 10, LIMITS), Ok(()));
        assert_eq!(b.pinned(1000), 10);
        assert_eq!(b.total(), 20);
    }

    #[test]
    fn root_has_only_the_total_limit() {
        let mut b = Budget::new();
        assert_eq!(b.charge(0, 20, LIMITS), Ok(()));
        assert_eq!(b.charge(1000, 5, LIMITS), Ok(()));
        assert_eq!(
            b.charge(0, 1, LIMITS),
            Err(BudgetError::Total {
                pinned: 25,
                limit: 25
            })
        );
        assert_eq!(b.usage(0, LIMITS).limit, None);
        assert_eq!(b.usage(1000, LIMITS).limit, Some(10));
        assert_eq!(b.usage(1000, LIMITS).total, 25);
    }

    #[test]
    fn refund_returns_pages() {
        let mut b = Budget::new();
        b.charge(7, 8, LIMITS).expect("within limits");
        b.refund(7, 3);
        assert_eq!(b.pinned(7), 5);
        b.refund(7, 5);
        assert_eq!(b.pinned(7), 0);
        assert_eq!(b.total(), 0);
        b.refund(7, 1);
        assert_eq!(b.total(), 0);
    }

    #[test]
    fn overflow_is_refused() {
        let mut b = Budget::new();
        let big = Limits {
            per_user: u64::MAX,
            total: u64::MAX,
        };
        b.charge(0, u64::MAX, big).expect("exactly the limit");
        assert!(b.charge(0, 1, big).is_err());
        assert!(b.charge(1, 1, big).is_err());
    }
}
