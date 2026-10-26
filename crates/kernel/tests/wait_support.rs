//! Fail-fast context stubs for compiling the actual wait-list/ordered-lock code.
//!
//! Only empty-list destruction and uncontended locks run on the host. Scheduler,
//! wakeup and shootdown paths deliberately fail if reached; they are not emulated.

pub(crate) mod percpu {
    std::thread_local! { static ACCESSES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) }; }
    pub(crate) struct PercpuBlock;
    impl PercpuBlock {
        pub(crate) fn current() -> &'static Self {
            ACCESSES.with(|count| count.set(count.get() + 1));
            &Self
        }
        pub(crate) fn maybe_handle_tlb_shootdown(&self) {
            panic!("host wait-list tests must not enter a shootdown path");
        }
    }
    pub(crate) fn accesses() -> usize {
        ACCESSES.with(core::cell::Cell::get)
    }
}

pub(crate) mod context {
    use crate::sync::{CleanLockToken, LockToken, RwLock, L3, L4};
    use alloc::sync::Arc;
    pub(crate) type ContextLock = RwLock<L4, Context>;
    pub(crate) struct Context {
        pub(crate) cancelled: bool,
    }
    pub(crate) struct Control;
    impl Control {
        pub(crate) fn currently_pending_unblocked(&self, _: &()) -> usize {
            panic!("host wait-list tests must not inspect signals");
        }
    }
    impl Context {
        pub(crate) fn sigcontrol(&self) -> Option<(&Control, &(), ())> {
            None
        }
        /// Host stub for `Context::killed_before_wait`: host tests construct
        /// contexts directly and never mark one as being sigkilled, so the
        /// wait-condition path sees "not killed", matching the kernel default.
        pub(crate) fn killed_before_wait(&self) -> bool {
            false
        }
        pub(crate) fn block(&mut self, _: &'static str) {
            panic!("host tests must not block");
        }
    }
    pub(crate) struct ContextRef(Arc<ContextLock>);
    impl core::ops::Deref for ContextRef {
        type Target = Arc<ContextLock>;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    pub(crate) fn current() -> ContextRef {
        panic!("host tests must not access a current context");
    }
    pub(crate) fn unblock_context(_: &Arc<ContextLock>, _: &mut LockToken<'_, L3>) {
        panic!("host tests must not wake a context");
    }
    pub(crate) fn switch(_: &mut CleanLockToken) {
        panic!("host tests must not switch contexts");
    }
}
