//! Host checks for the default action of each signal.

// The signal numbers of `redox_protocols::protocol`, which the module imports from its parent.
const SIGCHLD: usize = 17;
const SIGCONT: usize = 18;
const SIGTSTP: usize = 20;
const SIGTTIN: usize = 21;
const SIGTTOU: usize = 22;
const SIGURG: usize = 23;
const SIGWINCH: usize = 28;

#[path = "../src/signal/defaults.rs"]
mod defaults;
