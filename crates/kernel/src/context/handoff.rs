//! Translate timeslice selection into a physical context handoff.
//!
//! A scheme handler can run on a client's timeslice. Retaining that timeslice
//! still requires switching back to the client when the handler parks. Carry
//! the selected guard through this decision so a wakeup cannot invalidate it.

#![forbid(unsafe_code)]

/// A scheduler decision, with ownership of the selected context guards.
pub(crate) enum Scheduled<C, A> {
    /// Keep the current timeslice owner, which may differ from the running context.
    Retained(C),
    /// Assign the timeslice to a new owner and retain its address-space guard.
    Replaced {
        previous: C,
        next: C,
        address_space: A,
    },
}

/// The physical handoff required by a scheduler decision.
pub(crate) enum Handoff<C, A> {
    /// Both the timeslice owner and the running context stay the same.
    RetainedCurrent,
    /// The new timeslice owner is already running after a direct switch.
    SelectedCurrent,
    /// Switch to the owner, retaining any guards obtained during selection.
    Switch {
        previous: Option<C>,
        next: C,
        /// Absent for a retained owner; its address space has not been acquired yet.
        address_space: Option<A>,
    },
}

impl<C, A> Scheduled<C, A> {
    /// Determines whether the selected owner is already the physical context.
    pub(crate) fn handoff(self, is_current: impl FnOnce(&C) -> bool) -> Handoff<C, A> {
        match self {
            Self::Retained(next) => {
                if is_current(&next) {
                    Handoff::RetainedCurrent
                } else {
                    Handoff::Switch {
                        previous: None,
                        next,
                        address_space: None,
                    }
                }
            }
            Self::Replaced {
                previous,
                next,
                address_space,
            } => {
                if is_current(&next) {
                    Handoff::SelectedCurrent
                } else {
                    Handoff::Switch {
                        previous: Some(previous),
                        next,
                        address_space: Some(address_space),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Handoff, Scheduled};
    use core::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn a_client_woken_after_the_direct_switch_check_reclaims_its_timeslice() {
        let client_runnable = Rc::new(Cell::new(false));
        let handler_runnable = Rc::new(Cell::new(false));
        // The first direct-switch check cannot resume the blocked client.
        assert!(!client_runnable.get());
        // A worker answers while the scheduler is selecting its next owner.
        client_runnable.set(true);
        let selection: Scheduled<_, ()> = Scheduled::Retained(Rc::clone(&client_runnable));
        let Handoff::Switch {
            previous,
            next,
            address_space,
        } = selection.handoff(|owner| Rc::ptr_eq(owner, &handler_runnable))
        else {
            panic!("retaining the client's timeslice must not resume the blocked handler");
        };
        assert!(Rc::ptr_eq(&next, &client_runnable));
        assert!(next.get());
        assert!(!handler_runnable.get());
        assert!(previous.is_none());
        assert!(address_space.is_none());
    }

    #[test]
    fn retaining_the_running_owner_needs_no_physical_switch() {
        let selection: Scheduled<_, ()> = Scheduled::Retained(7);
        assert!(matches!(
            selection.handoff(|owner| *owner == 7),
            Handoff::RetainedCurrent
        ));
    }

    #[test]
    fn a_directly_running_handler_can_become_the_new_timeslice_owner() {
        assert!(matches!(
            Scheduled::Replaced {
                previous: 7,
                next: 9,
                address_space: (),
            }
            .handoff(|owner| *owner == 9),
            Handoff::SelectedCurrent
        ));
    }

    #[test]
    fn changing_to_a_nonrunning_owner_preserves_both_guards_and_its_address_space() {
        let Handoff::Switch {
            previous,
            next,
            address_space,
        } = (Scheduled::Replaced {
            previous: 7,
            next: 9,
            address_space: 13,
        })
        .handoff(|owner| *owner == 7)
        else {
            panic!("new owner requires a switch");
        };
        assert_eq!(previous, Some(7));
        assert_eq!(next, 9);
        assert_eq!(address_space, Some(13));
    }
}
