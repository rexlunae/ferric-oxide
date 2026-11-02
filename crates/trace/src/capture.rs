//! The files a capture directory adds to the rings' records.
//!
//! `ferric-trace capture` (on the target) and `ferric-trace follow` (on the host) write a
//! capture directory: one `cpu<n>` file of records per CPU, the clock line in a file named
//! as the scheme's [`CLOCK_PATH`](crate::abi::CLOCK_PATH), and the files named here. Tools
//! that only check whether a capture is whole, such as the QEMU harness's verdict, read
//! these names from here so they agree with the writer.

/// Present while a capture is being written and removed once it is complete; a capture
/// that failed or was killed keeps it.
pub const INCOMPLETE: &str = "INCOMPLETE";

/// The [`StatsLine`](crate::abi::StatsLine)s of every CPU when the capture began.
pub const STATS_START: &str = "stats.start";

/// The [`StatsLine`](crate::abi::StatsLine)s of every CPU when the capture ended, written
/// only by a capture that reached its end.
pub const STATS_END: &str = "stats.end";
