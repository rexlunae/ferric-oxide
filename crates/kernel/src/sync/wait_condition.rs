//! Scheduler wait lists protected by the kernel's lock-order tokens.
//!
//! A normal notification removes registrations. Signal-style notification keeps
//! them, letting a resumed waiter distinguish interruption from normal delivery.

use core::mem::ManuallyDrop;

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    context::{self, unblock_context, ContextLock},
    sync::{CleanLockToken, LockToken, Mutex, L1, L2, L3},
};

/// A collection of blocked contexts waiting for a resource or interruption.
#[derive(Debug)]
pub struct WaitCondition {
    contexts: Mutex<L3, Vec<Weak<ContextLock>>>,
}

impl WaitCondition {
    /// Creates an empty wait list without allocating.
    pub const fn new() -> WaitCondition {
        WaitCondition {
            contexts: Mutex::new(Vec::new()),
        }
    }

    /// Wakes all registered contexts and removes their registrations.
    ///
    /// Returns the number of registrations, including expired weak references.
    pub fn notify(&self, token: &mut CleanLockToken) -> usize {
        self.notify_locked(token.token().downgrade())
    }

    /// Wakes and removes registrations while lower-level locks may be held.
    pub fn notify_locked(&self, token: LockToken<'_, L1>) -> usize {
        let mut contexts = self.contexts.lock(token);
        let (contexts, mut token) = contexts.token_split();
        let len = contexts.len();
        while let Some(context_weak) = contexts.pop() {
            if let Some(context_ref) = context_weak.upgrade() {
                unblock_context(&context_ref, &mut token.token());
            }
        }
        len
    }

    /// Wakes registered contexts without consuming their registrations.
    ///
    /// They will report an interrupted wait when they resume. Returns the number
    /// of registrations, including expired weak references.
    pub fn notify_signal(&self, token: LockToken<'_, L1>) -> usize {
        let mut contexts = self.contexts.lock(token);
        let (contexts, mut token) = contexts.token_split();
        let len = contexts.len();
        for context_weak in contexts.iter() {
            if let Some(context_ref) = context_weak.upgrade() {
                unblock_context(&context_ref, &mut token.token());
            }
        }
        len
    }

    /// Registers the current context, drops `guard`, and waits for notification.
    ///
    /// Returns false for pending signals, cancellation, or an interrupted wait.
    /// The token must come from `guard`, whose drop must release the caller's
    /// remaining locks before the context switch.
    pub fn wait<'a, T>(
        &self,
        guard: T,
        reason: &'static str,
        token: &'a mut LockToken<'a, L1>,
    ) -> bool {
        let mut token = token.downgrade();
        self.wait_inner(guard, reason, &mut token)
    }

    /// Waits with a level-two guard, using the same protocol as [`Self::wait`].
    pub fn wait_inner<'a, T>(
        &self,
        guard: T,
        reason: &'static str,
        token: &'a mut LockToken<'a, L2>,
    ) -> bool {
        let current_context_ptr = {
            let current_context_ref = context::current();
            // Avoid a context switch between blocking ourselves and adding
            // ourselves to the wait list as otherwise we might miss a wakeup.
            // We cannot add ourselves to the wait list first as that would lead
            // to deadlock if we were woken up immediately.

            let mut token = token.token();
            {
                let context = current_context_ref.upgradeable_read(token.token());
                if let Some((control, pctl, _)) = context.sigcontrol()
                    && control.currently_pending_unblocked(pctl) != 0
                {
                    return false;
                }
                // Ferric Oxide: a cancelled context (a ring worker whose ring closed) waits no
                // more, as if a signal were pending.
                if context.cancelled {
                    return false;
                }
                // Ferric Oxide: nor does a context being killed (#200).
                if context.killed_before_wait() {
                    return false;
                }
                context.upgrade().block(reason);
            }

            self.contexts
                .lock(token.token())
                .push(Arc::downgrade(&current_context_ref));

            drop(guard);

            // It's perfectly valid to use raw pointers in Safe Rust for comparing...
            Arc::as_ptr(&*current_context_ref)
        };

        {
            // SAFETY: guard and the context/wait-list guards have been dropped;
            // the caller's token is borrowed for this wait, and no locks remain.
            let token = unsafe { &mut CleanLockToken::new() };
            context::switch(token);
            // ... and in order for it to be possible to switch back, the context Arc and thus
            // current_context_ptr must continue to be alive (meaningful to check against).
        }

        let mut waited = true;

        {
            let mut contexts = self.contexts.lock(token.token());

            if let Some(index) = contexts
                .iter()
                .position(|c| Weak::as_ptr(c) == current_context_ptr)
            {
                contexts.swap_remove(index);
                waited = false;
            }
        }

        waited
    }

    /// Wakes interrupted waiters and destroys the list using the caller's token.
    pub fn into_drop(self, token: &mut CleanLockToken) {
        self.into_drop_locked(token.token().downgrade());
    }

    /// Destroys the list while the caller holds lower-level locks.
    pub fn into_drop_locked(self, token: LockToken<'_, L1>) {
        let mut this = ManuallyDrop::new(self);
        this.inner_drop(token);
        // SAFETY: ManuallyDrop suppresses Self::drop (which would create another
        // clean token). contexts is the only field and is dropped exactly once.
        unsafe {
            core::ptr::drop_in_place(&mut this.contexts);
        }
    }

    fn inner_drop(&mut self, token: LockToken<'_, L1>) {
        self.notify_signal(token);
    }
}

impl Drop for WaitCondition {
    fn drop(&mut self) {
        // SAFETY: Implicit destruction is only valid without held locks or a
        // live clean token. Callers holding a token use into_drop[_locked].
        let mut token = unsafe { CleanLockToken::new() };
        self.inner_drop(token.downgrade());
        #[cfg(feature = "drop_panic")]
        {
            panic!("WaitCondition dropped");
        }
    }
}
