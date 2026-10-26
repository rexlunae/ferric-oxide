//! Request-scoped phase measurements and resettable reporting windows.
//!
//! Callers supply monotonic ticks. A sample pairs one whole request with the
//! summed time of its commands, including partial requests that end in error.
//!
//! ```
//! use ferric_blkqueue::timing::{Request, Window};
//! let mut request = Request::new(10);
//! request.command(20, 40);
//! let mut window = Window::new();
//! window.record(false, request.finish(50));
//! let samples = window.take();
//! assert_eq!(samples[0].sum, 40);
//! assert_eq!(samples[1].sum, 20);
//! assert_eq!(window.take()[0].count, 0);
//! ```

/// Timing state owned by one request, independent of its execution thread.
#[derive(Debug)]
pub struct Request {
    start: u64,
    device: u64,
}

impl Request {
    /// Starts a request at `now`.
    pub const fn new(now: u64) -> Self {
        Self {
            start: now,
            device: 0,
        }
    }

    /// Adds one command's elapsed ticks, saturating on overflow.
    pub fn command(&mut self, start: u64, end: u64) {
        self.device = self.device.saturating_add(end.saturating_sub(start));
    }

    /// Finishes this request; service and device counts advance together.
    pub fn finish(self, now: u64) -> Sample {
        Sample {
            service: now.saturating_sub(self.start),
            device: self.device,
        }
    }
}

/// One request's service time and accumulated command time, in caller ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Request entry through completion, including errors.
    pub service: u64,
    /// Sum of command durations within this request.
    pub device: u64,
}

/// Aggregate of one phase over a reporting interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Number of completed samples.
    pub count: u64,
    /// Sum of elapsed ticks (saturating).
    pub sum: u64,
    /// Smallest sample; zero when empty.
    pub min: u64,
    /// Largest sample; zero when empty.
    pub max: u64,
}

impl Counts {
    /// An empty accumulator, usable in static initializers.
    pub const EMPTY: Self = Self {
        count: 0,
        sum: 0,
        min: 0,
        max: 0,
    };

    fn record(&mut self, ticks: u64) {
        self.min = if self.count == 0 {
            ticks
        } else {
            self.min.min(ticks)
        };
        self.max = self.max.max(ticks);
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(ticks);
    }
}

/// Paired service/device counters for reads and writes, in that order.
#[derive(Debug)]
pub struct Window([Counts; 4]);

impl Window {
    /// Creates an empty window.
    pub const fn new() -> Self {
        Self([Counts::EMPTY; 4])
    }

    /// Publishes both phases of one read (`write=false`) or write request.
    pub fn record(&mut self, write: bool, sample: Sample) {
        let index = usize::from(write) * 2;
        self.0[index].record(sample.service);
        self.0[index + 1].record(sample.device);
    }

    /// Removes this window's samples, leaving the next interval empty.
    ///
    /// Order is read service, read device, write service, write device.
    pub fn take(&mut self) -> [Counts; 4] {
        core::mem::replace(&mut self.0, [Counts::EMPTY; 4])
    }
}

impl Default for Window {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_time_is_summed_once_per_request_including_a_failed_later_command() {
        let mut request = Request::new(10);
        request.command(20, 30);
        request.command(40, 60);
        let mut window = Window::new();
        window.record(false, request.finish(70));
        let [service, device, _, _] = window.take();
        assert_eq!(
            service,
            Counts {
                count: 1,
                sum: 60,
                min: 60,
                max: 60
            }
        );
        assert_eq!(
            device,
            Counts {
                count: 1,
                sum: 30,
                min: 30,
                max: 30
            }
        );
    }

    #[test]
    fn interleaved_requests_keep_their_own_service_and_command_times() {
        let mut first = Request::new(1);
        let mut second = Request::new(2);
        second.command(3, 7);
        first.command(4, 10);
        assert_eq!(
            second.finish(8),
            Sample {
                service: 6,
                device: 4
            }
        );
        assert_eq!(
            first.finish(12),
            Sample {
                service: 11,
                device: 6
            }
        );
    }

    #[test]
    fn reports_reset_and_do_not_repeat_old_requests() {
        let mut window = Window::new();
        for duration in [30, 10, 20] {
            window.record(
                false,
                Sample {
                    service: duration,
                    device: duration / 2,
                },
            );
        }
        window.record(
            true,
            Sample {
                service: 80,
                device: 40,
            },
        );
        let snapshot = window.take();
        assert_eq!(
            snapshot[0],
            Counts {
                count: 3,
                sum: 60,
                min: 10,
                max: 30
            }
        );
        assert_eq!(snapshot[1].count, 3);
        assert_eq!(snapshot[2].count, 1);
        assert_eq!(window.take(), [Counts::EMPTY; 4]);
        window.record(
            false,
            Sample {
                service: 9,
                device: 0,
            },
        );
        assert_eq!(
            window.take()[0],
            Counts {
                count: 1,
                sum: 9,
                min: 9,
                max: 9
            }
        );
    }

    #[test]
    fn zero_samples_and_overflow_remain_well_defined() {
        let mut window = Window::new();
        window.record(
            false,
            Sample {
                service: 0,
                device: 0,
            },
        );
        window.record(
            false,
            Sample {
                service: u64::MAX,
                device: 0,
            },
        );
        window.record(
            false,
            Sample {
                service: 1,
                device: 0,
            },
        );
        assert_eq!(
            window.take()[0],
            Counts {
                count: 3,
                sum: u64::MAX,
                min: 0,
                max: u64::MAX
            }
        );
    }
}
