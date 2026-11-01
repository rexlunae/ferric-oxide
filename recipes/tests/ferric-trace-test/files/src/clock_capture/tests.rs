//! Real shared-ring regressions for unconsumed overflow and bounded live capture.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use ferric_trace::abi::CPU_RING_WORDS;
use ferric_trace::kernel::SYSCALL_EXIT;
use ferric_trace::record::WORD_BYTES;
use ferric_trace::ring::{PushError, Ring, SHARED_HEADER_WORDS, Shared};

use super::*;

const WRITE: u64 = 4;
const FD: u64 = 0x7e57;
const FIRST_LEN: u64 = 1931;
type TestRing<'a> = Ring<Shared<&'a [AtomicU64]>>;

fn words() -> Vec<AtomicU64> {
    (0..SHARED_HEADER_WORDS + CPU_RING_WORDS)
        .map(|_| AtomicU64::new(0))
        .collect()
}

fn markers() -> Markers {
    Markers::new(WRITE, FD, FIRST_LEN)
}

fn write(ring: &TestRing<'_>, timestamp: u64, len: u64) {
    ring.push(SYSCALL_ENTER.id(), timestamp, &[WRITE, FD, 0, len, 0])
        .unwrap();
    ring.push(SYSCALL_EXIT.id(), timestamp + 1, &[WRITE, u64::MAX - 8])
        .unwrap();
}

#[test]
fn unconsumed_interval_loses_second_marker_and_counts_the_drop() {
    let words = words();
    let ring = Ring::shared(words.as_slice()).unwrap();
    let view = View::new(words.as_slice()).unwrap();
    let before = Counters::sample(&view);
    write(&ring, 10, FIRST_LEN);
    while ring.push(SYSCALL_EXIT.id(), 20, &[WRITE, 0]).is_ok() {}
    assert_eq!(
        ring.push(SYSCALL_ENTER.id(), 1000, &[WRITE, FD, 0, FIRST_LEN + 1, 0]),
        Err(PushError::Full)
    );
    let after = Counters::sample(&view);
    assert_eq!(after.tail, before.tail);
    assert!(after.head - before.head > 0);
    assert!(after.lost - before.lost >= 2);
    let mut buffer = vec![0; CPU_RING_WORDS * WORD_BYTES];
    let mut marks = markers();
    let peek = marks.collect(&view, &mut buffer).unwrap();
    ring.commit_to(peek.end()).unwrap();
    let error = marks.timestamps(0).unwrap_err();
    assert_eq!(error.actual, [1, 0, 0]);
    println!(
        "unconsumed: first={} second={} lost_during={} pending={}",
        error.actual[0],
        error.actual[1],
        after.lost - before.lost,
        after.head - after.tail
    );
}

#[test]
fn concurrent_bursts_exceed_ring_capacity_without_losing_any_probe_writes() {
    const ROUNDS: u64 = 64;
    const BURST: u64 = 64;
    let words = words();
    let ring = Ring::shared(words.as_slice()).unwrap();
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let (done_tx, done_rx) = mpsc::sync_channel(0);
    let (produced, consumed) = std::thread::scope(|scope| {
        let ring = &ring;
        let words = &words;
        let producer = scope.spawn(move || {
            write(ring, 10, FIRST_LEN);
            for round in 0..ROUNDS {
                for index in 0..BURST / 2 {
                    write(ring, 20 + round * BURST + index, FIRST_LEN + 2);
                }
                ready_tx.send(()).unwrap();
                // This half can publish while the consumer is copying the snapshot.
                // A later publication belongs to the next pass; neither side waits
                // for the ring to become empty.
                for index in BURST / 2..BURST {
                    write(ring, 20 + round * BURST + index, FIRST_LEN + 2);
                }
                done_rx.recv().unwrap();
                // Committing on the target generates more trace events. They belong to
                // the next pass, rather than extending the one that just completed.
                ring.push(SYSCALL_EXIT.id(), 9000, &[99, 0]).unwrap();
            }
            write(ring, 10_000, FIRST_LEN + 1);
            ready_tx.send(()).unwrap();
            done_rx.recv().unwrap();
        });
        let consumer = scope.spawn(move || {
            let view = View::new(words.as_slice()).unwrap();
            let mut buffer = vec![0; CPU_RING_WORDS * WORD_BYTES];
            let mut marks = markers();
            let mut copied = 0;
            for _ in 0..=ROUNDS {
                ready_rx.recv().unwrap();
                let peek = marks.collect(&view, &mut buffer).unwrap();
                copied += peek.len();
                ring.commit_to(peek.end()).unwrap();
                done_tx.send(()).unwrap();
            }
            (marks, copied)
        });
        (producer.join(), consumer.join())
    });
    assert!(produced.is_ok());
    let (marks, copied) = consumed.unwrap();
    assert!(copied > CPU_RING_WORDS * WORD_BYTES * 4);
    assert_eq!(ring.lost(), 0);
    assert_eq!(marks.timestamps(ROUNDS * BURST).unwrap(), (10, 10_000));
}

#[test]
fn commit_refills_do_not_extend_a_pass() {
    let words = words();
    let ring = Ring::shared(words.as_slice()).unwrap();
    let view = View::new(words.as_slice()).unwrap();
    let mut marks = markers();
    let mut buffer = vec![0; CPU_RING_WORDS * WORD_BYTES];
    write(&ring, 10, FIRST_LEN);
    let peek = marks.collect(&view, &mut buffer).unwrap();
    ring.commit_to(peek.end()).unwrap();
    write(&ring, 20, FIRST_LEN + 1);
    assert!(view.head() > peek.end());
    assert_eq!(marks.timestamps(0).unwrap_err().actual, [1, 0, 0]);
    let peek = marks.collect(&view, &mut buffer).unwrap();
    ring.commit_to(peek.end()).unwrap();
    assert_eq!(marks.timestamps(0).unwrap(), (10, 20));
}

#[test]
fn duplicate_missing_and_unrelated_writes_are_distinguished() {
    let words = words();
    let ring = Ring::shared(words.as_slice()).unwrap();
    let view = View::new(words.as_slice()).unwrap();
    let mut buffer = vec![0; CPU_RING_WORDS * WORD_BYTES];
    ring.push(SYSCALL_ENTER.id(), 1, &[WRITE, FD + 1, 0, FIRST_LEN, 0])
        .unwrap();
    ring.push(SYSCALL_ENTER.id(), 2, &[WRITE + 1, FD, 0, FIRST_LEN, 0])
        .unwrap();
    write(&ring, 10, FIRST_LEN);
    write(&ring, 20, FIRST_LEN);
    write(&ring, 30, FIRST_LEN + 1);
    let mut marks = markers();
    let peek = marks.collect(&view, &mut buffer).unwrap();
    ring.commit_to(peek.end()).unwrap();
    assert_eq!(marks.timestamps(1).unwrap_err().actual, [2, 1, 0]);
}

#[test]
fn a_small_buffer_does_not_consume_the_record() {
    let words = words();
    let ring = Ring::shared(words.as_slice()).unwrap();
    let view = View::new(words.as_slice()).unwrap();
    write(&ring, 10, FIRST_LEN);
    let error = markers().collect(&view, &mut [0; WORD_BYTES]).unwrap_err();
    assert!(matches!(
        error,
        CaptureError::Read(ReadError::BufferTooSmall { .. })
    ));
    assert_eq!(view.tail(), 0);
}

#[test]
fn unknown_and_catalog_incompatible_events_fail_without_consumption() {
    for (event, args) in [
        (EventId::new(u16::MAX), &[][..]),
        (SYSCALL_ENTER.id(), &[][..]),
    ] {
        let words = words();
        let ring = Ring::shared(words.as_slice()).unwrap();
        let view = View::new(words.as_slice()).unwrap();
        ring.push(event, 1, args).unwrap();
        let mut buffer = vec![0; CPU_RING_WORDS * WORD_BYTES];
        let error = markers().collect(&view, &mut buffer).unwrap_err();
        match error {
            CaptureError::Unknown { event: found }
            | CaptureError::Arguments { event: found, .. } => assert_eq!(found, event),
            other => panic!("unexpected capture error: {other}"),
        }
        assert_eq!(view.tail(), 0);
    }
}

#[test]
fn corrupt_records_do_not_advance_the_tail() {
    let words = words();
    let ring = Ring::shared(words.as_slice()).unwrap();
    let view = View::new(words.as_slice()).unwrap();
    write(&ring, 10, FIRST_LEN);
    words[SHARED_HEADER_WORDS].store(u64::MAX, Ordering::Relaxed);
    let mut buffer = vec![0; CPU_RING_WORDS * WORD_BYTES];
    assert!(matches!(
        markers().collect(&view, &mut buffer),
        Err(CaptureError::Corrupt { position: 0 })
    ));
    assert_eq!(view.tail(), 0);
}
