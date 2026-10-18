//! The kernel's events: the one definition of their ids, names, levels and arguments, shared by
//! the kernel (which emits them) and every decoder.
//!
//! Ids are part of the record format: append new events at the end and never renumber.

crate::events! {
    /// Every kernel event, by id.
    pub catalog CATALOG;

    /// A system call starts: its number and its first four arguments.
    pub const SYSCALL_ENTER = 0 {
        name: "syscall.enter",
        subsystem: Syscall,
        level: Info,
        args: ["nr", "a", "b", "c", "d"],
    }

    /// A system call returns: its number and the raw return value (a negated errno on failure,
    /// as `Error::mux` returns it).
    pub const SYSCALL_EXIT = 1 {
        name: "syscall.exit",
        subsystem: Syscall,
        level: Info,
        args: ["nr", "ret"],
    }

    /// The scheduler switches this CPU from one context to another: the process ids and the
    /// contexts' debug ids.
    pub const SCHED_SWITCH = 2 {
        name: "sched.switch",
        subsystem: Sched,
        level: Info,
        args: ["prev_pid", "next_pid", "prev_id", "next_id"],
    }

    /// The kernel queues a request to a user-space scheme: the scheme's id, the request's SQE
    /// tag, its opcode (`schemev2::Opcode`), the caller's pid, and the request's first and
    /// third arguments (for reads and writes, the scheme's file number and the byte count).
    ///
    /// A tag is unique among a scheme's requests in flight and is reused afterwards, so a
    /// request is matched with the next `scheme.response` or `scheme.cancel` of the same scheme
    /// and tag.
    pub const SCHEME_REQUEST = 3 {
        name: "scheme.request",
        subsystem: Scheme,
        level: Info,
        args: ["scheme", "tag", "opcode", "caller", "arg0", "arg2"],
    }

    /// A scheme answers a request (its CQE): the scheme's id, the tag, the raw result (a
    /// negated errno on failure, 0 for a response that carries file descriptors) and the
    /// responding process's pid.
    pub const SCHEME_RESPONSE = 4 {
        name: "scheme.response",
        subsystem: Scheme,
        level: Info,
        args: ["scheme", "tag", "ret", "responder"],
    }

    /// The kernel asks a scheme to cancel a request whose caller was interrupted: the scheme's
    /// id and the tag. The scheme still answers it.
    pub const SCHEME_CANCEL = 5 {
        name: "scheme.cancel",
        subsystem: Scheme,
        level: Info,
        args: ["scheme", "tag"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Subsystem;

    #[test]
    fn names_start_with_their_subsystem() {
        for event in CATALOG.events() {
            let (subsystem, rest) = event.name().split_once('.').expect("a dotted name");
            assert_eq!(subsystem, event.subsystem().name(), "{}", event.name());
            assert!(!rest.is_empty());
        }
    }

    #[test]
    fn ids_are_stable() {
        assert_eq!(SYSCALL_ENTER.id().get(), 0);
        assert_eq!(SYSCALL_EXIT.id().get(), 1);
        assert_eq!(SCHED_SWITCH.id().get(), 2);
        assert_eq!(SCHED_SWITCH.subsystem(), Subsystem::Sched);
        assert_eq!(SCHEME_REQUEST.id().get(), 3);
        assert_eq!(SCHEME_RESPONSE.id().get(), 4);
        assert_eq!(SCHEME_CANCEL.id().get(), 5);
        assert_eq!(SCHEME_CANCEL.subsystem(), Subsystem::Scheme);
    }
}
