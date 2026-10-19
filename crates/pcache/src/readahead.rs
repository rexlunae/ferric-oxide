//! How far ahead of a reader the kernel fills.
//!
//! The kernel keeps one [`Readahead`] per open file description. Each read reports the pages
//! it touched ([`Readahead::on_read`]). A read that starts where the last one ended, or at the
//! start of the file, is *sequential*. The first sequential read gets the initial window of
//! pages filled past it, and each further one doubles the window, up to the maximum. Any other
//! read is *random*: it resets the window and gets no readahead, so random readers do not fill
//! pages nobody asked for. The window never reaches past the end of the file.
//!
//! The answer is the range to fill *beyond* what the read asked for. The kernel fills the
//! pages of it that are absent, together with the read's own missing pages, in one record
//! where they are contiguous. The file system may clamp a fill to its extents.

use core::fmt;

use crate::abi::{MAX_RECORD_PAGES, PageRange};

/// The readahead window bounds, in pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadaheadConfig {
    initial: u32,
    max: u32,
}

impl ReadaheadConfig {
    /// A window that starts at `initial` pages and grows to at most `max`.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Zero`] when `initial` is 0, [`ConfigError::Order`] when it is above
    /// `max`, and [`ConfigError::TooLarge`] when `max` is above [`MAX_RECORD_PAGES`], so that
    /// every window [`Readahead::on_read`] returns fits in one fill record.
    pub const fn new(initial: u32, max: u32) -> Result<Self, ConfigError> {
        if initial == 0 {
            return Err(ConfigError::Zero);
        }
        if max > MAX_RECORD_PAGES {
            return Err(ConfigError::TooLarge);
        }
        if initial > max {
            return Err(ConfigError::Order);
        }
        Ok(Self { initial, max })
    }

    /// The defaults: 4 pages (16 KiB) at first, growing to 32 (128 KiB).
    pub const DEFAULT: Self = Self {
        initial: 4,
        max: 32,
    };

    /// The first window.
    pub const fn initial(self) -> u32 {
        self.initial
    }

    /// The largest window.
    pub const fn max(self) -> u32 {
        self.max
    }
}

impl Default for ReadaheadConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a [`ReadaheadConfig`] was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The initial window is 0 pages.
    Zero,
    /// The initial window is larger than the maximum.
    Order,
    /// The maximum is above [`MAX_RECORD_PAGES`], more than one record can carry.
    TooLarge,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Zero => "the initial readahead window is 0 pages",
            Self::Order => "the initial readahead window is larger than the maximum",
            Self::TooLarge => "the readahead maximum is larger than one fill record",
        })
    }
}

impl core::error::Error for ConfigError {}

/// The readahead state of one open file description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Readahead {
    config: ReadaheadConfig,
    /// The page a sequential read starts at: where the last read ended.
    next: u64,
    /// The window the next sequential read gets; 0 before the first one.
    window: u32,
}

impl Readahead {
    /// The state of a description that has not been read yet.
    pub const fn new(config: ReadaheadConfig) -> Self {
        Self {
            config,
            next: 0,
            window: 0,
        }
    }

    /// Records a read of `pages` in a file of `file_pages` pages, and returns the pages to fill
    /// past it, if any: at most [`MAX_RECORD_PAGES`], as the config allows no more.
    pub fn on_read(&mut self, pages: PageRange, file_pages: u64) -> Option<PageRange> {
        let sequential = pages.first() == self.next || pages.first() == 0;
        self.next = pages.end();
        if !sequential {
            self.window = 0;
            return None;
        }
        self.window = if self.window == 0 {
            self.config.initial
        } else {
            self.window.saturating_mul(2).min(self.config.max)
        };
        let end = pages
            .end()
            .saturating_add(u64::from(self.window))
            .min(file_pages);
        PageRange::from_bounds(pages.end(), end)
    }

    /// The window the next sequential read gets (0 until a sequential read has happened).
    pub const fn window(&self) -> u32 {
        self.window
    }

    /// Changes the bounds, keeping the current window within the new maximum.
    pub fn set_config(&mut self, config: ReadaheadConfig) {
        self.config = config;
        self.window = self.window.min(config.max);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::ToString;
    use std::vec::Vec;

    fn range(first: u64, count: u32) -> PageRange {
        PageRange::new(first, count).expect("a valid range")
    }

    fn reader() -> Readahead {
        Readahead::new(ReadaheadConfig::new(4, 32).expect("a valid config"))
    }

    #[test]
    fn sequential_reads_double_the_window_up_to_the_maximum() {
        let mut ahead = reader();
        let mut windows = Vec::new();
        for page in 0..6 {
            let extra = ahead.on_read(range(page, 1), 1_000).expect("readahead");
            assert_eq!(extra.first(), page + 1);
            windows.push(extra.count());
        }
        assert_eq!(windows, [4, 8, 16, 32, 32, 32]);
    }

    #[test]
    fn a_random_read_resets_and_gets_nothing() {
        let mut ahead = reader();
        assert!(ahead.on_read(range(0, 2), 1_000).is_some());
        assert!(ahead.on_read(range(2, 2), 1_000).is_some());
        assert_eq!(ahead.window(), 8);
        assert_eq!(ahead.on_read(range(500, 1), 1_000), None);
        assert_eq!(ahead.window(), 0);
        // Reading on from where the random read ended is sequential again, from the start.
        assert_eq!(ahead.on_read(range(501, 1), 1_000), Some(range(502, 4)));
    }

    #[test]
    fn strided_reads_get_no_readahead() {
        let mut ahead = reader();
        let _ = ahead.on_read(range(0, 1), 1_000);
        for page in [10, 20, 30, 40] {
            assert_eq!(ahead.on_read(range(page, 1), 1_000), None);
        }
    }

    #[test]
    fn the_window_stops_at_the_end_of_the_file() {
        let mut ahead = reader();
        assert_eq!(ahead.on_read(range(0, 2), 4), Some(range(2, 2)));
        // At the end there is nothing left to read ahead.
        assert_eq!(ahead.on_read(range(2, 2), 4), None);
        assert_eq!(ahead.on_read(range(0, 1), 0), None);
    }

    #[test]
    fn rereading_from_the_start_is_sequential() {
        let mut ahead = reader();
        let _ = ahead.on_read(range(0, 8), 100);
        assert_eq!(ahead.on_read(range(0, 8), 100), Some(range(8, 8)));
    }

    #[test]
    fn the_largest_window_fits_one_record() {
        let config = ReadaheadConfig::new(MAX_RECORD_PAGES, MAX_RECORD_PAGES).expect("valid");
        let mut ahead = Readahead::new(config);
        for page in 0..4 {
            let extra = ahead.on_read(range(page, 1), u64::MAX).expect("readahead");
            assert!(crate::abi::RecordPages::new(extra).is_some());
        }
    }

    #[test]
    fn configs_are_checked_and_can_shrink() {
        assert_eq!(ReadaheadConfig::new(0, 4), Err(ConfigError::Zero));
        assert_eq!(ReadaheadConfig::new(8, 4), Err(ConfigError::Order));
        assert_eq!(
            ReadaheadConfig::new(4, MAX_RECORD_PAGES + 1),
            Err(ConfigError::TooLarge)
        );
        assert_ne!(
            ConfigError::TooLarge.to_string(),
            ConfigError::Order.to_string()
        );
        assert_eq!(ReadaheadConfig::default(), ReadaheadConfig::DEFAULT);
        assert!(!ConfigError::Order.to_string().is_empty());
        let mut ahead = reader();
        for page in 0..5 {
            let _ = ahead.on_read(range(page, 1), 1_000);
        }
        assert_eq!(ahead.window(), 32);
        ahead.set_config(ReadaheadConfig::new(2, 8).expect("a valid config"));
        assert_eq!(ahead.window(), 8);
        assert_eq!(ahead.on_read(range(5, 1), 1_000), Some(range(6, 8)));
    }
}
