//! Context-optional interrupt dispatch for CPUs reserved by the profiler.
//!
//! The profiler CPU never initializes current/idle contexts. Keep the safe
//! acknowledgement decision host-testable while architecture callbacks perform
//! TLB maintenance and APIC EOI on the target.

use core::cell::{Ref, RefCell};

/// Borrows an initialized idle context, releasing the borrow if it is absent.
///
/// # Panics
/// Panics if the slot is already mutably borrowed.
pub(crate) fn get<T>(slot: &RefCell<Option<T>>) -> Option<Ref<'_, T>> {
    Ref::filter_map(slot.borrow(), Option::as_ref).ok()
}

/// Whether interrupt acknowledgement permits considering a scheduler switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WakeupAction {
    /// Return without accessing current-context or scheduler state.
    Return,
    /// An idle context was interrupted; the caller may schedule queued wakeups.
    Schedule,
}

/// Acknowledges a wakeup, including a routed TLB shootdown without contexts.
///
/// With no idle context, services the shootdown before EOI and never calls
/// `is_current`. Otherwise acknowledges and classifies the interrupted context.
pub(crate) fn acknowledge<T>(
    idle: Option<T>,
    is_current: impl FnOnce(T) -> bool,
    shootdown: impl FnOnce(),
    eoi: impl FnOnce(),
) -> WakeupAction {
    let Some(idle) = idle else {
        shootdown();
        eoi();
        return WakeupAction::Return;
    };
    let idling = is_current(idle);
    eoi();
    if idling {
        WakeupAction::Schedule
    } else {
        WakeupAction::Return
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    #[test]
    fn an_uninitialized_idle_slot_returns_none_without_holding_a_borrow() {
        let slot = RefCell::<Option<u8>>::new(None);
        assert!(get(&slot).is_none());
        *slot.borrow_mut() = Some(7);
        assert_eq!(*get(&slot).unwrap(), 7);
    }

    #[test]
    fn no_context_acknowledges_the_pending_shootdown_before_eoi() {
        let idle = RefCell::<Option<u8>>::new(None);
        let pending = Cell::new(true);
        let eois = Cell::new(0);
        let action = acknowledge(
            get(&idle),
            |_| panic!("must not read a current context on the profiler CPU"),
            || {
                assert!(pending.replace(false));
            },
            || {
                assert!(!pending.get());
                eois.set(eois.get() + 1);
            },
        );
        assert_eq!(action, WakeupAction::Return);
        assert_eq!(eois.get(), 1);
        assert!(!pending.get());
    }

    #[test]
    fn ordinary_cpus_keep_the_idle_only_schedule_decision() {
        for (idling, expected) in [
            (true, WakeupAction::Schedule),
            (false, WakeupAction::Return),
        ] {
            let eois = Cell::new(0);
            assert_eq!(
                acknowledge(
                    Some(7),
                    |idle| {
                        assert_eq!(idle, 7);
                        idling
                    },
                    || panic!("ordinary wakeup does not service the routed profiler TLB path"),
                    || eois.set(eois.get() + 1)
                ),
                expected
            );
            assert_eq!(eois.get(), 1);
        }
    }
}
