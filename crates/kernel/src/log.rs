use alloc::collections::VecDeque;
use core::fmt;
use spin::{Mutex, MutexGuard};

use crate::devices::graphical_debug::{DebugDisplay, DEBUG_DISPLAY};

pub static LOG: Mutex<Option<Log>> = Mutex::new(None);

pub fn init() {
    let mut guard = LOG.lock();
    let log = Log::new(1024 * 1024);
    // Ferric Oxide: what was logged before the heap existed (`kernel.log_persist`).
    #[cfg(feature = "log_persist")]
    let log = {
        let mut log = log;
        crate::log_persist::take_early(&mut log);
        log
    };
    *guard = Some(log);
}

pub struct Log {
    data: VecDeque<u8>,
    size: usize,
}

impl Log {
    pub fn new(size: usize) -> Log {
        Log {
            data: VecDeque::with_capacity(size),
            size,
        }
    }

    pub fn read(&self) -> (&[u8], &[u8]) {
        self.data.as_slices()
    }

    pub fn write(&mut self, buf: &[u8]) {
        // Ferric Oxide: also into reserved memory that the next boot can read.
        #[cfg(feature = "log_persist")]
        crate::log_persist::append(buf);
        for &b in buf {
            while self.data.len() + 1 >= self.size {
                self.data.pop_front();
            }
            self.data.push_back(b);
        }
    }
}

pub struct Writer<'a> {
    log: MutexGuard<'a, Option<Log>>,
    display: MutexGuard<'a, Option<DebugDisplay>>,
    arch: crate::arch::debug::Writer<'a>,
}

impl<'a> Writer<'a> {
    pub fn new() -> Writer<'a> {
        Writer {
            log: LOG.lock(),
            display: DEBUG_DISPLAY.lock(),
            arch: crate::arch::debug::Writer::new(),
        }
    }

    pub fn write(&mut self, buf: &[u8], preserve: bool) {
        if preserve && let Some(ref mut log) = *self.log {
            log.write(buf);
        }
        // Ferric Oxide: keep what is logged before the heap exists (`kernel.log_persist`).
        #[cfg(feature = "log_persist")]
        if preserve && self.log.is_none() {
            crate::log_persist::early(buf);
        }

        if let Some(display) = &mut *self.display {
            display.write(buf);
        }

        self.arch.write(buf);
    }
}

impl fmt::Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> Result<(), fmt::Error> {
        self.write(s.as_bytes(), true);
        Ok(())
    }
}
