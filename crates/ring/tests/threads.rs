//! Both sides of a ring on separate threads: entries arrive complete and in order, and the
//! need-wakeup protocol loses no wakeup.

use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ferric_ring::notify::Sleep;
use ferric_ring::queue::PushError;
use ferric_ring::{Consumer, Layout, Producer, Sqe};

const COUNT: u64 = 200_000;

fn memory(layout: &Layout) -> Vec<AtomicU64> {
    (0..layout.words()).map(|_| AtomicU64::new(0)).collect()
}

/// An SQE whose every field follows from `i`, so a torn or stale entry shows.
fn sqe(i: u64) -> Sqe {
    Sqe::write(
        i as u32,
        i.rotate_left(17),
        !i,
        (i as u32).wrapping_mul(3),
        i,
    )
}

#[test]
fn entries_cross_threads_whole_and_in_order() {
    let layout = Layout::new(16, 0).expect("valid");
    let mem = memory(&layout);
    let mem = &mem[..];
    thread::scope(|s| {
        s.spawn(|| {
            let mut tx = Producer::<Sqe>::new(layout.sq());
            let mut i = 0;
            while i < COUNT {
                match tx.push(mem, &sqe(i)) {
                    Ok(()) => i += 1,
                    Err(PushError::Full) => thread::yield_now(),
                    Err(err) => panic!("{err}"),
                }
            }
        });
        let mut rx = Consumer::<Sqe>::new(layout.sq());
        let mut i = 0;
        while i < COUNT {
            match rx.pop(mem).expect("valid") {
                Some(got) => {
                    assert_eq!(got, sqe(i));
                    i += 1;
                }
                None => thread::yield_now(),
            }
        }
    });
}

#[test]
fn no_wakeup_is_lost() {
    let layout = Layout::new(8, 0).expect("valid");
    let mem = memory(&layout);
    let mem = &mem[..];
    // The channel stands in for the kernel's event: a wakeup is one message.
    let (wake, woken) = mpsc::channel::<()>();
    thread::scope(|s| {
        s.spawn(move || {
            let mut tx = Producer::<Sqe>::new(layout.sq());
            for i in 0..COUNT / 10 {
                loop {
                    match tx.push(mem, &Sqe::nop(i)) {
                        Ok(()) => break,
                        Err(PushError::Full) => thread::yield_now(),
                        Err(err) => panic!("{err}"),
                    }
                }
                if tx.needs_wakeup(mem).expect("in bounds") {
                    wake.send(()).expect("consumer alive");
                }
            }
        });
        let mut rx = Consumer::<Sqe>::new(layout.sq());
        let mut next = 0;
        let mut sleeps = 0u64;
        while next < COUNT / 10 {
            if let Some(got) = rx.pop(mem).expect("valid") {
                assert_eq!(got.user_data, next);
                next += 1;
                continue;
            }
            if rx.prepare_sleep(mem).expect("valid") == Sleep::Sleep {
                // A lost wakeup leaves the consumer asleep with entries pending.
                woken
                    .recv_timeout(Duration::from_secs(10))
                    .expect("a wakeup for every sleep with entries to come");
                rx.woken(mem).expect("in bounds");
                sleeps += 1;
            }
        }
        // Stale wakeups from before a Retry may be left over; they are harmless.
        while woken.try_recv().is_ok() {}
        assert!(sleeps <= COUNT / 10);
    });
}

/// The kernel's `Enter` coordination: a waiter sleeps under a lock while SQEs are in flight
/// and too few completions are pending ([`wait_done`]), and a submitter that counts a chain
/// and then fails to consume it takes it out again. The [`Drained`] that `end` returns is
/// what the submitter wakes the waiter on; without it the waiter would sleep for ever.
#[test]
fn a_failed_submission_wakes_a_waiter() {
    use std::sync::{Arc, Condvar, Mutex};

    use ferric_ring::notify::{InFlight, wait_done};

    for _ in 0..1000 {
        let state = Arc::new((Mutex::new(InFlight::new()), Condvar::new()));
        state.0.lock().unwrap().start(3);
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = {
            let state = Arc::clone(&state);
            thread::spawn(move || {
                let (lock, completed) = &*state;
                let mut work = lock.lock().unwrap();
                // One completion wanted, none pending.
                while !wait_done(0, 1, work.get()) {
                    work = completed.wait(work).unwrap();
                }
                done_tx.send(()).unwrap();
            })
        };
        thread::yield_now();
        // The consumption failed: the chain is taken out of the count.
        let drained = state.0.lock().unwrap().end(3);
        if drained.is_some() {
            state.1.notify_all();
        }
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the waiter was woken");
        waiter.join().unwrap();
    }
}
