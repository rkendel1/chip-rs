//! Time source. Everything that waits or measures goes through a [`Clock`] so that tests (and the
//! simulated command line transport) never really sleep.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync {
    /// Monotonic time since this clock started.
    fn now(&self) -> Duration;
    /// Waits for `d`. A manual clock advances instead.
    fn sleep(&self, d: Duration);
}

pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A clock that only moves when told to, or when something sleeps on it.
#[derive(Default)]
pub struct ManualClock {
    state: Mutex<ManualState>,
}

#[derive(Default)]
struct ManualState {
    now: Duration,
    slept: Vec<Duration>,
}

impl ManualClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, d: Duration) {
        self.state.lock().unwrap().now += d;
    }

    /// Every duration passed to `sleep`, in order.
    pub fn slept(&self) -> Vec<Duration> {
        self.state.lock().unwrap().slept.clone()
    }

    pub fn total_slept(&self) -> Duration {
        self.state.lock().unwrap().slept.iter().sum()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.state.lock().unwrap().now
    }

    fn sleep(&self, d: Duration) {
        let mut s = self.state.lock().unwrap();
        s.now += d;
        s.slept.push(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_sleep_advances_and_records() {
        let c = ManualClock::new();
        c.sleep(Duration::from_millis(10));
        c.sleep(Duration::from_millis(5));
        assert_eq!(c.now(), Duration::from_millis(15));
        assert_eq!(c.slept().len(), 2);
        assert_eq!(c.total_slept(), Duration::from_millis(15));
    }

    #[test]
    fn manual_clock_advance_is_not_a_sleep() {
        let c = ManualClock::new();
        c.advance(Duration::from_secs(1));
        assert_eq!(c.now(), Duration::from_secs(1));
        assert!(c.slept().is_empty());
    }

    #[test]
    fn system_clock_is_monotonic() {
        let c = SystemClock::new();
        let a = c.now();
        let b = c.now();
        assert!(b >= a);
    }
}
