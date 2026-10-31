//! Wake completion owners and return submission credits, including after cancellation.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

/// Collect the owner, if still present, and one submitter for a hardware completion.
///
/// Cancelling a future removes its completion owner, but the eventual hardware
/// completion still frees queue resources. A submitter must be woken in both cases.
pub(crate) fn collect_wakeups<T>(
    completed: Option<T>,
    submitters: Option<&mut VecDeque<T>>,
    ready: &mut Vec<T>,
) {
    if let Some(owner) = completed {
        ready.push(owner);
    }
    if let Some(submitter) = submitters.and_then(VecDeque::pop_front) {
        ready.push(submitter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_completion_wakes_a_submitter() {
        let mut submitters = VecDeque::from([11, 12]);
        let mut ready = vec![1];
        collect_wakeups(None, Some(&mut submitters), &mut ready);
        assert_eq!(ready, [1, 11]);
        assert_eq!(submitters, [12]);
    }

    #[test]
    fn live_completion_wakes_owner_and_one_fifo_submitter() {
        let mut submitters = VecDeque::from([11, 12]);
        let mut ready = Vec::new();
        collect_wakeups(Some(10), Some(&mut submitters), &mut ready);
        assert_eq!(ready, [10, 11]);
        assert_eq!(submitters, [12]);
    }

    #[test]
    fn completion_without_submission_waiters_still_wakes_its_owner() {
        let mut ready = Vec::new();
        collect_wakeups(Some(10), None, &mut ready);
        assert_eq!(ready, [10]);
    }

    #[test]
    fn cancelled_completion_without_waiters_schedules_nothing() {
        let mut ready = Vec::<usize>::new();
        let mut empty = VecDeque::new();
        collect_wakeups(None, Some(&mut empty), &mut ready);
        collect_wakeups(None, None, &mut ready);
        assert!(ready.is_empty());
    }
}
