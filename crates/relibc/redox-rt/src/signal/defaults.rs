//! The default action (`SIG_DFL`) of each signal, as POSIX defines it in `<signal.h>`.
//!
//! redox-rt stores each disposition's flags in the process control structure that procmgr
//! reads when it sends a signal, so the default action decides there whether procmgr marks
//! the signal pending and interrupts a thread's blocked system call. A signal whose default
//! action is to ignore it must therefore carry `IGNORED` while it has its default
//! disposition, or a blocked call fails with `EINTR` although no handler runs.

use super::{SIGCHLD, SIGCONT, SIGTSTP, SIGTTIN, SIGTTOU, SIGURG, SIGWINCH};

/// What a signal does to the process while its disposition is `SIG_DFL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefaultAction {
    /// The process terminates, possibly with a core dump.
    Terminate,
    /// The signal is discarded.
    Ignore,
    /// The process stops, unless the stop is suppressed for an orphaned process group.
    Stop,
}

/// Returns the action `sig` takes while its disposition is `SIG_DFL`.
///
/// `SIGSTOP` is not listed: it cannot be caught or ignored, and procmgr stops the process
/// itself. `SIGCONT` is ignored: procmgr continues a stopped process when the signal is
/// sent, whatever its disposition, so the delivered signal has nothing left to do.
pub(crate) const fn default_action(sig: usize) -> DefaultAction {
    match sig {
        SIGCHLD | SIGCONT | SIGURG | SIGWINCH => DefaultAction::Ignore,
        SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        _ => DefaultAction::Terminate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_the_signals_posix_ignores_by_default() {
        for sig in [SIGCHLD, SIGCONT, SIGURG, SIGWINCH] {
            assert_eq!(default_action(sig), DefaultAction::Ignore, "signal {sig}");
        }
    }

    #[test]
    fn stops_on_the_job_control_signals() {
        for sig in [SIGTSTP, SIGTTIN, SIGTTOU] {
            assert_eq!(default_action(sig), DefaultAction::Stop, "signal {sig}");
        }
    }

    #[test]
    fn terminates_on_every_other_signal() {
        let special = [
            SIGCHLD, SIGCONT, SIGURG, SIGWINCH, SIGTSTP, SIGTTIN, SIGTTOU,
        ];
        for sig in (1..=64).filter(|sig| !special.contains(sig)) {
            assert_eq!(
                default_action(sig),
                DefaultAction::Terminate,
                "signal {sig}"
            );
        }
    }
}
