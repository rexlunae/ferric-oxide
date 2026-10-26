//! Ferric Oxide: which `fchown` calls the kernel passes on to a user scheme.
//!
//! A caller other than root may only name its own effective user and group. Either one
//! may also be [`ferric_abi::id::UNCHANGED`] (`-1` as `uid_t`/`gid_t`, as POSIX `chown`
//! defines it), which keeps that ID as it is. The scheme still makes its own checks, such as
//! whether the caller owns the file.

use ferric_abi::id::requested;

/// Whether a caller with effective user `euid` and group `egid` may ask a scheme to give a
/// file the owner `uid` and group `gid`. Root may ask for anything. Anyone else may ask
/// only for its own effective IDs, or [`ferric_abi::id::UNCHANGED`] in either position.
pub fn chown_allowed(euid: u32, egid: u32, uid: u32, gid: u32) -> bool {
    euid == 0
        || (requested(uid).is_none_or(|uid| uid == euid)
            && requested(gid).is_none_or(|gid| gid == egid))
}

#[cfg(test)]
mod tests {
    use super::chown_allowed;
    use ferric_abi::id::UNCHANGED;

    #[test]
    fn root_may_ask_for_any_owner_and_group() {
        for (uid, gid) in [(0, 0), (1000, 100), (UNCHANGED, 5), (7, UNCHANGED)] {
            assert!(chown_allowed(0, 0, uid, gid), "{uid} {gid}");
        }
    }

    #[test]
    fn others_may_ask_only_for_their_own_ids_or_unchanged() {
        let (euid, egid) = (1000, 100);
        for (uid, gid) in [
            (1000, 100),
            (UNCHANGED, 100),
            (1000, UNCHANGED),
            (UNCHANGED, UNCHANGED),
        ] {
            assert!(chown_allowed(euid, egid, uid, gid), "{uid} {gid}");
        }
        for (uid, gid) in [
            (0, 100),
            (1001, UNCHANGED),
            (1000, 0),
            (UNCHANGED, 101),
            (UNCHANGED - 1, UNCHANGED),
        ] {
            assert!(!chown_allowed(euid, egid, uid, gid), "{uid} {gid}");
        }
    }
}
