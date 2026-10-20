use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicU32 as AtomicUint, Ordering},
};

use crate::{
    error::Errno,
    header::errno::EFAULT,
    platform::{Pal, Sys},
    sync::FutexAtomicTy,
};

/// An unsafe "one thread to one thread" synchronization primitive. Used for and modeled after
/// pthread_join only, at the moment.
#[derive(Debug)]
pub struct Waitval<T> {
    state: AtomicUint,
    value: UnsafeCell<MaybeUninit<T>>,
}

unsafe impl<T: Send + Sync> Send for Waitval<T> {}
unsafe impl<T: Send + Sync> Sync for Waitval<T> {}

impl<T> Waitval<T> {
    #[expect(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            state: AtomicUint::new(0),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    // SAFETY: Caller must ensure both (1) that the value has not yet been initialized, and (2)
    // that this is never run by more than one thread simultaneously.
    pub unsafe fn post(&self, value: T) {
        unsafe { self.value.get().write(MaybeUninit::new(value)) };
        // Ferric Oxide: once `state` is 1, the joiner can return from `wait` and free the
        // thread control block holding `self` (`pthread::dealloc_thread` unmaps it) before the
        // wake below runs. So take the address first, do not touch `self` after the store, and
        // treat EFAULT (the page is gone) as nobody left to wake instead of panicking.
        let state = self.state.ptr();
        self.state.store(1, Ordering::Release);
        // SAFETY: FUTEX_WAKE only names the word; nothing here dereferences `state`. Both
        // kernels look the address up before using it and fail with EFAULT if it is no longer
        // mapped (Redox: `validate_and_translate_virt` in `syscall/futex.rs`; Linux: the futex
        // key lookup), so a freed thread control block cannot fault this thread.
        match unsafe { Sys::futex_wake(state, i32::MAX as u32) } {
            Ok(_) | Err(Errno(EFAULT)) => {}
            Err(Errno(errno)) => panic!("futex wake failed with errno {errno}"),
        }
    }

    pub fn wait(&self) -> &T {
        while self.state.load(Ordering::Acquire) == 0 {
            crate::sync::futex_wait(&self.state, 0, None);
        }

        unsafe { (*self.value.get()).assume_init_ref() }
    }
}
