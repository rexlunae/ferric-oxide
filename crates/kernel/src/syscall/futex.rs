//! # Futex
//! Futex or Fast Userspace Mutex is "a method for waiting until a certain condition becomes true."
//!
//! For more information about futexes, please read [this](https://eli.thegreenplace.net/2018/basics-of-futexes/) blog post, and the [futex(2)](http://man7.org/linux/man-pages/man2/futex.2.html) man page
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, Ordering};
use hashbrown::{hash_map::DefaultHashBuilder, HashMap};
use rmm::Arch;
use syscall::EINTR;

use crate::{
    context::{
        self,
        memory::{AccessMode, AddrSpace, Provider},
        unblock_context, ContextLock,
    },
    memory::{Page, PhysicalAddress, VirtualAddress},
    sync::{CleanLockToken, Mutex, L1},
};

use crate::syscall::{
    data::TimeSpec,
    error::{Error, Result, EAGAIN, EFAULT, EINVAL, ENOMEM, ETIMEDOUT},
    flag::{FUTEX_WAIT, FUTEX_WAIT64, FUTEX_WAKE},
};

use super::usercopy::UserSlice;

// Physical address used as key, required if synchronizing across address spaces
// (necessitates MAP_SHARED since CoW would invalidate this address).
type FutexList = HashMap<PhysicalAddress, Vec<FutexEntry>>;

pub struct FutexEntry {
    // Context to wake up, and compare address spaces.
    context_lock: Weak<ContextLock>,
}

// TODO: Process-private futexes? In that case, put the futex table in each AddrSpace, or just
// implement that fully in userspace. Although futex is probably the best API for process-shared
// POSIX synchronization primitives, a local hash table and wait-for-thread kernel APIs (e.g.
// lwp_park/lwp_unpark from NetBSD) could be a simpler replacement.
static FUTEXES: Mutex<L1, FutexList> =
    Mutex::new(FutexList::with_hasher(DefaultHashBuilder::new()));

pub fn get_futex_stat(token: &mut CleanLockToken) -> (usize, usize) {
    let mut regc = 0;
    let mut regl = 0;
    let registry = FUTEXES.lock(token.token());
    for (_, v) in registry.iter() {
        regl += v.len();
        regc += 1;
    }
    (regc, regl)
}

fn validate_and_translate_virt(space: &AddrSpace, addr: VirtualAddress) -> Option<PhysicalAddress> {
    // TODO: Move this elsewhere!
    if addr.data().saturating_add(size_of::<usize>()) >= crate::USER_END_OFFSET {
        return None;
    }

    let page = Page::containing_address(addr);
    let off = addr.data() - page.start_address().data();

    let (frame, _) = space.table.utable.translate(page.start_address())?;

    Some(frame.add(off))
}

pub fn futex(
    addr: usize,
    op: usize,
    val: usize,
    val2: usize,
    _addr2: usize,
    token: &mut CleanLockToken,
) -> Result<usize> {
    let target_virtaddr = VirtualAddress::new(addr);
    let page = Page::containing_address(target_virtaddr);

    let current_addrsp = AddrSpace::current()?;

    // Keep the address space locked so we can safely read from the physical address. Unlock it
    // before context switching.
    let addr_space_guard = current_addrsp.acquire_read(token.downgrade());
    let needs_correction = match addr_space_guard.grants.contains(page) {
        Some((_, info)) => matches!(info.provider, Provider::Allocated { .. }),
        None => false,
    };

    let addr_space_guard = if needs_correction {
        drop(addr_space_guard);
        crate::context::memory::try_correcting_page_tables(page, AccessMode::Write, token)
            .map_err(|err| match err {
                crate::context::memory::PfError::Oom => Error::new(ENOMEM),
                crate::context::memory::PfError::Segv => Error::new(EFAULT),
                _ => Error::new(EFAULT),
            })?;
        current_addrsp.acquire_read(token.downgrade())
    } else {
        addr_space_guard
    };

    let target_physaddr = validate_and_translate_virt(&addr_space_guard, target_virtaddr)
        .ok_or(Error::new(EFAULT))?;

    match op {
        // TODO: FUTEX_WAIT_MULTIPLE?
        FUTEX_WAIT | FUTEX_WAIT64 => {
            // Reading the timeout can fault; no address-space lock may be held then.
            drop(addr_space_guard);
            let timeout_opt = UserSlice::ro(val2, size_of::<TimeSpec>())?
                .none_if_null()
                .map(|buf| unsafe { buf.read_exact::<TimeSpec>() })
                .transpose()?;
            // Ferric Oxide: a negative timeout used to become a deadline that
            // never arrives (found by ferric-syscall-fuzz).
            if let Some(timeout) = &timeout_opt {
                crate::time::check_timespec(timeout)?;
            }

            let target_physaddr = {
                // FUTEX_WAKE holds this same lock. The comparison and waiter insertion
                // must be one critical section, or a wake between them is lost.
                let mut futexes_lock = FUTEXES.lock(token.token());
                let (futexes, mut token) = futexes_lock.token_split();
                let context_lock = context::current();
                let mut lock_token = token.token();
                let addr_space_guard = current_addrsp.acquire_read(lock_token.downgrade());
                let target_physaddr =
                    validate_and_translate_virt(&addr_space_guard, target_virtaddr)
                        .ok_or(Error::new(EFAULT))?;

                let (fetched, expected) = if op == FUTEX_WAIT {
                    // Must be aligned, otherwise it could cross a page boundary and mess up the
                    // (simpler) validation we did in the first place.
                    if !addr.is_multiple_of(4) {
                        return Err(Error::new(EINVAL));
                    }

                    // TODO: On systems where virtual memory is not abundant, we might instead
                    // add an atomic usercopy function.
                    let accessible_addr = crate::memory::RmmA::phys_to_virt(target_physaddr).data();

                    (
                        u64::from(unsafe {
                            (*(accessible_addr as *const AtomicU32)).load(Ordering::SeqCst)
                        }),
                        u64::from(val as u32),
                    )
                } else {
                    #[cfg(target_has_atomic = "64")]
                    {
                        use core::sync::atomic::AtomicU64;

                        // op == FUTEX_WAIT64
                        if !addr.is_multiple_of(8) {
                            return Err(Error::new(EINVAL));
                        }

                        // TODO: On systems where virtual memory is not abundant, we might instead
                        // add an atomic usercopy function.
                        let accessible_addr =
                            crate::memory::RmmA::phys_to_virt(target_physaddr).data();

                        (
                            unsafe {
                                (*(accessible_addr as *const AtomicU64)).load(Ordering::SeqCst)
                            },
                            val as u64,
                        )
                    }
                    #[cfg(not(target_has_atomic = "64"))]
                    {
                        return Err(Error::new(crate::syscall::error::EOPNOTSUPP));
                    }
                };
                if fetched != expected {
                    return Err(Error::new(EAGAIN));
                }

                drop(addr_space_guard);

                let mut context = context_lock.write(token.token());
                if let Some((tctl, pctl, _)) = context.sigcontrol()
                    && tctl.currently_pending_unblocked(pctl) != 0
                {
                    return Err(Error::new(EINTR));
                }
                // Ferric Oxide: a thread killed on its way here does not wait (#200).
                if context.killed_before_wait() {
                    return Err(Error::new(EINTR));
                }
                context.wake = timeout_opt.map(|time| time.to_nanos());
                context.block("futex");

                futexes
                    .entry(target_physaddr)
                    .or_insert_with(Vec::new)
                    .push(FutexEntry {
                        context_lock: Arc::downgrade(&context_lock),
                    });
                crate::trace::trace_event!(
                    ferric_trace::kernel::SCHED_FUTEX_WAIT,
                    target_physaddr.data(),
                    addr,
                    context.debug_id,
                    if op == FUTEX_WAIT { 32 } else { 64 },
                    expected,
                    context.wake.is_some(),
                );
                target_physaddr
            };

            context::switch(token);

            let result = finish_wait(target_physaddr, timeout_opt.is_some(), token);
            crate::trace::trace_event!(
                ferric_trace::kernel::SCHED_FUTEX_RESUME,
                target_physaddr.data(),
                addr,
                context::current().read(token.token()).debug_id,
                Error::mux(result),
            );
            result
        }
        FUTEX_WAKE => {
            let mut woken = 0;

            {
                drop(addr_space_guard);
                let mut futexes_map = FUTEXES.lock(token.token());
                let (futexes_map, mut token) = futexes_map.token_split();

                let is_empty = if let Some(futexes) = futexes_map.get_mut(&target_physaddr) {
                    while woken < val && !futexes.is_empty() {
                        let futex = futexes.swap_remove(0);
                        if let Some(ctx) = futex.context_lock.upgrade() {
                            unblock_context(&ctx, &mut token.token().downgrade());
                            woken += 1;
                        }
                    }

                    futexes.is_empty()
                } else {
                    false
                };
                if is_empty {
                    futexes_map.remove(&target_physaddr);
                }

                // A resumed waiter must take this lock in finish_wait before recording
                // completion, so publish the wake before it can publish its resume.
                crate::trace::trace_event!(
                    ferric_trace::kernel::SCHED_FUTEX_WAKE,
                    target_physaddr.data(),
                    addr,
                    val,
                    woken,
                );
            }
            Ok(woken)
        }
        _ => Err(Error::new(EINVAL)),
    }
}

/// Removes a resumed caller's queued entry and determines its syscall result before tracing it.
fn finish_wait(
    target_physaddr: PhysicalAddress,
    timed: bool,
    token: &mut CleanLockToken,
) -> Result<usize> {
    // The scheduler clears `wake` on timeout. Hence if a timeout was
    // set and `wake` is now `None`, we timed out.
    let wake = context::current().write(token.token()).wake.take();
    let resume_result = if timed && wake.is_none() {
        Err(Error::new(ETIMEDOUT))
    } else {
        Ok(0)
    };

    // TODO: this doing a second search is not efficient. It probably best to have another
    // hashtable mapping context and whether it was woken up AND where to remove index from FUTEXES
    let mut futexes_map = FUTEXES.lock(token.token());
    let (futexes_map, _) = futexes_map.token_split();

    let Some(futexes) = futexes_map.get_mut(&target_physaddr) else {
        return resume_result;
    };
    let current_lock = context::current();
    let mut found = false;

    let mut i = 0;
    while i < futexes.len() {
        if let Some(ctx) = futexes[i].context_lock.upgrade() {
            if Arc::ptr_eq(&ctx, &current_lock) {
                // Not woken up by FUTEX_WAKE, remove our entry
                futexes.swap_remove(i);
                found = true;
                break;
            }
            i += 1;
        } else {
            futexes.swap_remove(i);
        }
    }

    if futexes.is_empty() {
        futexes_map.remove(&target_physaddr);
    }

    // Timeouts must remove their waiter too, otherwise a later wake can be
    // consumed by a context that is no longer waiting on this word.
    if !found || resume_result.is_err() {
        return resume_result;
    }

    // Not woken up by FUTEX_WAKE, likely by signal
    Err(Error::new(EINTR))
}
