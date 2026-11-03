//! Classification of user page faults against the page-table entry already present.
//!
//! A fault can arrive for a page whose entry already permits the access: another CPU of the
//! same address space resolved it first while this one waited for the address-space lock, or
//! this CPU still caches an older translation. AArch64 can walk a stale invalid descriptor until
//! a DSB and context synchronization, and RISC-V harts may cache invalid entries until an
//! `SFENCE.VMA`. Such a fault is spurious: `correct_inner` must only invalidate the local
//! translation, and must not ask the grant's provider again, since `External` providers take a
//! page reference and `FmapBorrowed` providers send a request to the scheme each time.
//!
//! This module holds the architecture-independent decision so the host tests in
//! `crates/kernel/tests` exercise it; `context::memory` converts `rmm` page flags into
//! [`PtePermissions`].

/// The access that caused a user page fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// A data read.
    Read,
    /// A data write.
    Write,
    /// An instruction fetch.
    InstrFetch,
}

/// The permissions of a present user page-table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtePermissions {
    /// User mode may access the page.
    pub user: bool,
    /// The page is writable.
    pub write: bool,
    /// The page is executable.
    pub execute: bool,
}

impl AccessMode {
    /// Returns whether a present entry with `pte` permits this access from user mode.
    pub fn permitted_by(self, pte: PtePermissions) -> bool {
        pte.user
            && match self {
                Self::Read => true,
                Self::Write => pte.write,
                Self::InstrFetch => pte.execute,
            }
    }
}

/// Returns whether a fault for `access` is spurious, given the entry `pte` currently present for
/// the page (`None` if it is not present).
///
/// A spurious fault is resolved by invalidating the page's local translation alone, without
/// consulting the grant's provider.
pub fn is_spurious(access: AccessMode, pte: Option<PtePermissions>) -> bool {
    pte.is_some_and(|pte| access.permitted_by(pte))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [AccessMode; 3] = [AccessMode::Read, AccessMode::Write, AccessMode::InstrFetch];

    fn pte(user: bool, write: bool, execute: bool) -> PtePermissions {
        PtePermissions {
            user,
            write,
            execute,
        }
    }

    #[test]
    fn missing_entry_is_never_spurious() {
        for access in MODES {
            assert!(!is_spurious(access, None), "{access:?}");
        }
    }

    #[test]
    fn supervisor_entry_is_never_spurious() {
        for access in MODES {
            assert!(
                !is_spurious(access, Some(pte(false, true, true))),
                "{access:?}"
            );
        }
    }

    #[test]
    fn read_only_entry_satisfies_reads_only() {
        let ro = Some(pte(true, false, false));
        assert!(is_spurious(AccessMode::Read, ro));
        // A write to a read-only entry is a CoW or `mprotect` upgrade, not a spurious fault.
        assert!(!is_spurious(AccessMode::Write, ro));
        assert!(!is_spurious(AccessMode::InstrFetch, ro));
    }

    #[test]
    fn writable_entry_satisfies_reads_and_writes() {
        let rw = Some(pte(true, true, false));
        assert!(is_spurious(AccessMode::Read, rw));
        assert!(is_spurious(AccessMode::Write, rw));
        assert!(!is_spurious(AccessMode::InstrFetch, rw));
    }

    #[test]
    fn executable_entry_satisfies_fetches() {
        let rx = Some(pte(true, false, true));
        assert!(is_spurious(AccessMode::Read, rx));
        assert!(is_spurious(AccessMode::InstrFetch, rx));
        assert!(!is_spurious(AccessMode::Write, rx));
    }

    #[test]
    fn full_entry_satisfies_every_access() {
        for access in MODES {
            assert!(
                is_spurious(access, Some(pte(true, true, true))),
                "{access:?}"
            );
        }
    }
}
