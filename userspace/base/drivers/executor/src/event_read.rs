//! Read one driver event, retrying interruptions without losing the event payload.

#![forbid(unsafe_code)]

/// Retry interrupted reads; return all other failures to the executor.
pub(crate) fn next_event<T, E>(
    mut read: impl FnMut() -> Result<T, E>,
    is_interrupt: impl Fn(&E) -> bool,
) -> Result<T, E> {
    loop {
        match read() {
            Err(error) if is_interrupt(&error) => continue,
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind};

    use super::*;

    fn interrupted(error: &Error) -> bool {
        error.kind() == ErrorKind::Interrupted
    }

    #[test]
    fn immediate_event_is_read_once() {
        let mut calls = 0;
        let event = next_event(
            || {
                calls += 1;
                Ok((0, 0, 0))
            },
            interrupted,
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(event.0, 0);
    }

    #[test]
    fn repeated_interruptions_preserve_the_event() {
        let expected = (17, 42, 3);
        let mut results = [
            Err(Error::from(ErrorKind::Interrupted)),
            Err(Error::from(ErrorKind::Interrupted)),
            Ok(expected),
        ]
        .into_iter();
        let event = next_event(
            || results.next().expect("read beyond the event"),
            interrupted,
        )
        .unwrap();
        assert_eq!(event, expected);
        assert!(results.next().is_none());
    }

    #[test]
    fn terminal_errors_are_returned_without_retry() {
        for kind in [ErrorKind::PermissionDenied, ErrorKind::BrokenPipe] {
            let mut calls = 0;
            let error = next_event::<(), _>(
                || {
                    calls += 1;
                    Err(Error::from(kind))
                },
                interrupted,
            )
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn interruption_does_not_hide_a_later_failure() {
        let mut results = [
            Err(Error::from(ErrorKind::Interrupted)),
            Err(Error::from(ErrorKind::BrokenPipe)),
        ]
        .into_iter();
        let error = next_event::<(), _>(
            || results.next().expect("read beyond the terminal error"),
            interrupted,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
        assert!(results.next().is_none());
    }
}
