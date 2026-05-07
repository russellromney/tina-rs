//! Runtime-owned clock abstraction. Production uses MonotonicClock;
//! tests inject ManualClock so timer behavior is deterministic.

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::rc::Rc;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

pub(crate) trait Clock: std::fmt::Debug {
    fn now(&self) -> Instant;
}

#[derive(Debug)]
pub(crate) struct MonotonicClock;

impl Clock for MonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ManualClock {
    base: Instant,
    offset: Cell<Duration>,
}

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Cell::new(Duration::ZERO),
        }
    }

    pub(crate) fn advance(&self, by: Duration) {
        self.offset.set(self.offset.get() + by);
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + self.offset.get()
    }
}

#[cfg(test)]
impl Clock for Rc<ManualClock> {
    fn now(&self) -> Instant {
        self.base + self.offset.get()
    }
}
