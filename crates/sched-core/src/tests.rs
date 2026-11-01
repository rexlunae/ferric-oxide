//! Deterministic accounting and competing-task models; no timing assumptions.

use super::*;

extern crate std;

fn running_clock(total_weight: u64) -> FairClock {
    let mut clock = FairClock::new();
    clock.set_weight(0, total_weight);
    clock.set_activity(0, CpuActivity::Running);
    clock
}

fn normal() -> Weight {
    Weight::for_priority(20).unwrap()
}

fn active(weight: Weight) -> FairState {
    let mut task = FairState::new();
    task.activate(VirtualTime::ZERO, weight);
    task
}

#[test]
fn priorities_are_nonzero_ordered_and_out_of_range_is_rejected() {
    assert_eq!(normal().get(), 1024);
    let mut previous = u64::MAX;
    for priority in 0..40 {
        let weight = Weight::for_priority(priority).unwrap().get();
        assert!(weight < previous && weight > 0);
        previous = weight;
    }
    assert!(Weight::for_priority(40).is_none());
    assert!(Weight::for_priority(usize::MAX).is_none());
}

#[test]
fn real_elapsed_time_is_independent_of_interrupt_cadence() {
    let mut task = active(normal());
    // The PIT is roughly 4.06 ms, not the old 2.25 ms conversion constant.
    let interval = Duration::from_nanos(4_060_000);
    task.account(interval, normal(), ScheduleReason::Preempt);
    assert_eq!(task.runtime().get(), interval.as_nanos());
    assert_eq!(task.remaining(), BASE_SLICE - interval);
}

#[test]
fn short_events_preserve_unconsumed_service_and_deadline() {
    for reason in [
        ScheduleReason::Preempt,
        ScheduleReason::Wake,
        ScheduleReason::Block,
        ScheduleReason::Reconsider,
    ] {
        let mut task = active(normal());
        let deadline = task.deadline();
        task.account(Duration::from_nanos(1), normal(), reason);
        assert_eq!(task.runtime(), VirtualTime::new(1));
        assert_eq!(task.remaining(), BASE_SLICE - Duration::from_nanos(1));
        assert_eq!(task.deadline(), deadline);
    }
}

#[test]
fn explicit_yield_forfeits_remaining_service_even_after_a_long_interval() {
    for elapsed in [
        Duration::ZERO,
        Duration::from_millis(1),
        Duration::from_millis(19),
    ] {
        let mut task = active(normal());
        task.account(elapsed, normal(), ScheduleReason::Yield);
        assert_eq!(task.runtime().get(), BASE_SLICE.as_nanos());
        assert_eq!(task.remaining(), BASE_SLICE);
        assert_eq!(task.deadline().get(), 2 * BASE_SLICE.as_nanos());
    }
}

#[test]
fn slice_boundary_and_overrun_charge_all_execution_without_wrapping() {
    for elapsed in [BASE_SLICE, BASE_SLICE * 4, Duration::MAX] {
        let mut task = active(normal());
        task.account(elapsed, normal(), ScheduleReason::Preempt);
        assert_eq!(task.runtime().get(), elapsed.as_nanos());
        assert_eq!(task.remaining(), BASE_SLICE);
        assert_eq!(
            task.deadline().get(),
            elapsed.as_nanos() + BASE_SLICE.as_nanos()
        );
    }
}

#[test]
fn blocking_charges_execution_before_deactivation_and_wakeup_clamps() {
    let mut task = active(normal());
    task.account(Duration::from_millis(2), normal(), ScheduleReason::Block);
    task.deactivate();
    assert_eq!(task.runtime().get(), 2_000_000);
    assert!(task.remaining().is_zero());
    task.activate(VirtualTime::new(5_000_000), normal());
    assert_eq!(task.runtime().get(), 5_000_000);
    assert_eq!(task.remaining(), BASE_SLICE);
    task.activate(VirtualTime::ZERO, normal());
    assert_eq!(task.runtime().get(), 5_000_000);
}

#[test]
fn fractional_runtime_survives_many_short_observations() {
    for priority in 0..40 {
        let weight = Weight::for_priority(priority).unwrap();
        let mut whole = active(weight);
        let mut split = whole;
        whole.account(Duration::from_nanos(12_345), weight, ScheduleReason::Wake);
        for _ in 0..12_345 {
            split.account(Duration::from_nanos(1), weight, ScheduleReason::Wake);
        }
        assert_eq!(split.runtime(), whole.runtime());
        assert_eq!(split.remaining(), whole.remaining());
        assert_eq!(split.deadline(), whole.deadline());
    }
}

#[test]
fn reweighting_discards_only_the_old_fraction() {
    let high = Weight::for_priority(0).unwrap();
    let mut task = active(high);
    task.account(Duration::from_nanos(1), high, ScheduleReason::Preempt);
    assert_eq!(task.runtime(), VirtualTime::ZERO);
    task.account(Duration::from_nanos(1), normal(), ScheduleReason::Preempt);
    assert_eq!(task.runtime(), VirtualTime::new(1));
}

#[test]
fn cpu_clock_uses_preceding_population_and_never_charges_idle() {
    let mut clock = running_clock(2 * normal().get());
    clock.settle(2_000_000);
    assert_eq!(clock.time(), VirtualTime::new(1_000_000));
    clock.set_activity(2_000_000, CpuActivity::Idle);
    clock.set_weight(2_000_000, normal().get());
    clock.settle(100_000_000_000);
    assert_eq!(clock.time(), VirtualTime::new(1_000_000));
    clock.advance_to(100_000_000_000, VirtualTime::new(4_000_000));
    clock.advance_to(100_000_000_000, VirtualTime::ZERO);
    assert_eq!(clock.time(), VirtualTime::new(4_000_000));
}

#[test]
fn cpu_clock_partitioning_preserves_fractional_progress() {
    let mut whole = running_clock(88_761);
    let mut split = whole;
    whole.settle(100_000);
    for now in 1..=100_000 {
        split.settle(now);
    }
    assert_eq!(split.time(), whole.time());
}

#[test]
fn migration_preserves_positive_and_negative_lag_and_real_slice() {
    for (source, destination, expected) in [(10, 20, 25), (20, 30, 25), (30, 10, 0)] {
        let mut task = active(normal());
        task.account(Duration::from_nanos(15), normal(), ScheduleReason::Preempt);
        let remaining = task.remaining();
        task.migrate(
            VirtualTime::new(source),
            VirtualTime::new(destination),
            normal(),
        );
        assert_eq!(task.runtime(), VirtualTime::new(expected));
        assert_eq!(task.remaining(), remaining);
        assert_eq!(task.deadline().get(), expected + remaining.as_nanos());
    }
}

#[test]
fn wide_virtual_time_does_not_truncate_during_migration_or_overflow() {
    let mut task = active(normal());
    let high = VirtualTime::new(u128::from(u64::MAX) * 4);
    task.migrate(VirtualTime::ZERO, high, normal());
    assert_eq!(task.runtime(), high);
    task.migrate(VirtualTime::ZERO, VirtualTime::MAX, normal());
    task.account(Duration::MAX, normal(), ScheduleReason::Yield);
    assert_eq!(task.runtime(), VirtualTime::MAX);
    assert_eq!(task.deadline(), VirtualTime::MAX);
    let mut clock = running_clock(1);
    clock.advance_to(0, VirtualTime::MAX);
    clock.settle(u128::MAX);
    assert_eq!(clock.time(), VirtualTime::MAX);
}

#[test]
fn remote_steal_settles_the_old_population_without_resetting_owner_execution() {
    use std::sync::{Arc, Mutex};

    for (before, after, expected) in [(2048, 1024, 9_000_000), (3072, 2048, 5_000_000)] {
        let clock = Arc::new(Mutex::new(running_clock(before)));
        let remote = Arc::clone(&clock);
        std::thread::spawn(move || remote.lock().unwrap().set_weight(6_000_000, after))
            .join()
            .unwrap();

        let mut clock = clock.lock().unwrap();
        // A local scheduler may have sampled its owner's time before waiting
        // for the lock. That older sample must not rewind the remote boundary.
        clock.settle(5_000_000);
        clock.settle(12_000_000);
        assert_eq!(clock.time(), VirtualTime::new(expected));
        assert_eq!(clock.total_weight(), after);

        let mut owner = active(normal());
        owner.account(Duration::from_millis(12), normal(), ScheduleReason::Preempt);
        assert_eq!(owner.runtime(), VirtualTime::new(12_000_000));
        assert_eq!(owner.remaining(), BASE_SLICE - Duration::from_millis(12));
    }
}

#[test]
fn migration_uses_both_clocks_at_the_population_change_timestamp() {
    let mut source = running_clock(3 * normal().get());
    let mut destination = running_clock(normal().get());
    let mut task = active(normal());
    task.account(Duration::from_millis(3), normal(), ScheduleReason::Preempt);
    let remaining = task.remaining();

    source.set_weight(6_000_000, 2 * normal().get());
    destination.set_weight(6_000_000, 2 * normal().get());
    task.migrate(source.time(), destination.time(), normal());
    assert_eq!(source.time(), VirtualTime::new(2_000_000));
    assert_eq!(destination.time(), VirtualTime::new(6_000_000));
    assert_eq!(task.runtime(), VirtualTime::new(7_000_000));
    assert_eq!(task.remaining(), remaining);

    source.settle(12_000_000);
    destination.settle(12_000_000);
    assert_eq!(source.time(), VirtualTime::new(5_000_000));
    assert_eq!(destination.time(), VirtualTime::new(9_000_000));
}

#[test]
fn idle_population_changes_and_zero_weight_never_charge_idle_service() {
    let mut clock = FairClock::new();
    clock.set_weight(0, normal().get());
    clock.set_weight(10_000_000, 0);
    clock.set_activity(20_000_000, CpuActivity::Running);
    clock.settle(30_000_000);
    assert_eq!(clock.time(), VirtualTime::ZERO);
    clock.set_weight(30_000_000, normal().get());
    clock.settle(35_000_000);
    assert_eq!(clock.time(), VirtualTime::new(5_000_000));
    clock.set_activity(35_000_000, CpuActivity::Idle);
    clock.settle(100_000_000);
    assert_eq!(clock.time(), VirtualTime::new(5_000_000));
}

#[test]
fn full_width_clock_intervals_divide_before_saturating() {
    let mut normal_clock = running_clock(normal().get());
    normal_clock.settle(u128::MAX);
    assert_eq!(normal_clock.time(), VirtualTime::MAX);
    normal_clock.settle(0);
    assert_eq!(normal_clock.time(), VirtualTime::MAX);

    let mut high_weight = running_clock(2048);
    high_weight.settle(u128::MAX);
    assert_eq!(high_weight.time(), VirtualTime::new(u128::MAX / 2));
    let mut low_weight = running_clock(1);
    low_weight.settle(u128::MAX);
    assert_eq!(low_weight.time(), VirtualTime::MAX);
}

#[test]
fn borrowed_scheme_service_is_charged_to_the_same_owner() {
    let mut direct = active(normal());
    let mut borrowed = direct;
    direct.account(Duration::from_millis(5), normal(), ScheduleReason::Preempt);
    // No accounting reset occurs on physical direct switches. The scheduler
    // charges the owner's whole interval when it next reconsiders the owner.
    borrowed.account(Duration::from_millis(5), normal(), ScheduleReason::Block);
    assert_eq!(direct.runtime(), borrowed.runtime());
    assert_eq!(direct.remaining(), borrowed.remaining());
}

#[test]
fn competing_tasks_receive_weighted_service_with_irregular_events() {
    // This independent selection model checks the accounting primitive. It
    // does not exercise kernel run queues, physical handoffs or migration.
    let weights = [
        normal(),
        Weight::for_priority(25).unwrap(),
        Weight::for_priority(15).unwrap(),
    ];
    let mut tasks = weights.map(active);
    let mut service = [0u128; 3];
    let total_weight = weights.iter().map(|weight| weight.get()).sum();
    let mut clock = running_clock(total_weight);
    let mut now = 0;
    for event in 0..20_000u64 {
        let eligible = tasks.iter().any(|task| task.runtime() <= clock.time());
        if !eligible {
            clock.advance_to(now, tasks.iter().map(FairState::runtime).min().unwrap());
        }
        let next = (0..tasks.len())
            .filter(|&i| tasks[i].runtime() <= clock.time())
            .min_by_key(|&i| {
                (
                    tasks[i].deadline(),
                    core::cmp::Reverse(tasks[i].remaining()),
                    i,
                )
            })
            .unwrap();
        let elapsed = Duration::from_nanos(100_000 + event * 7919 % 1_000_000);
        service[next] += elapsed.as_nanos();
        now += elapsed.as_nanos();
        clock.settle(now);
        tasks[next].account(elapsed, weights[next], ScheduleReason::Preempt);
    }
    let total: u128 = service.iter().sum();
    for i in 0..tasks.len() {
        let expected = total * u128::from(weights[i].get()) / u128::from(total_weight);
        // At most a few outstanding slices at this finite sample boundary.
        assert!(
            service[i].abs_diff(expected) < BASE_SLICE.as_nanos() * 4,
            "service {:?} at {:?}",
            service,
            weights
        );
    }
}
