//! A request-wide time limit, measured on a [`Clock`](crate::util::Clock).

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    at: Option<Duration>,
}

impl Deadline {
    /// A deadline `limit` after `started` (none when `limit` is `None`).
    pub fn new(started: Duration, limit: Option<Duration>) -> Self {
        Self {
            at: limit.map(|l| started + l),
        }
    }

    pub fn none() -> Self {
        Self { at: None }
    }

    pub fn remaining(&self, now: Duration) -> Option<Duration> {
        self.at.map(|at| at.saturating_sub(now))
    }

    /// Sleeping for `wait` from `now` would run past the deadline.
    pub fn forbids(&self, now: Duration, wait: Duration) -> bool {
        self.at.is_some_and(|at| now + wait > at)
    }

    pub fn passed(&self, now: Duration) -> bool {
        self.at.is_some_and(|at| now >= at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_deadline_never_forbids() {
        let d = Deadline::none();
        assert!(!d.forbids(Duration::from_secs(1), Duration::from_secs(1000)));
        assert!(!d.passed(Duration::from_secs(1000)));
        assert_eq!(d.remaining(Duration::ZERO), None);
    }

    #[test]
    fn forbids_waits_that_overrun() {
        let d = Deadline::new(Duration::from_secs(10), Some(Duration::from_secs(5)));
        assert!(!d.forbids(Duration::from_secs(10), Duration::from_secs(5)));
        assert!(d.forbids(Duration::from_secs(10), Duration::from_millis(5001)));
        assert!(d.forbids(Duration::from_secs(14), Duration::from_secs(2)));
    }

    #[test]
    fn remaining_saturates() {
        let d = Deadline::new(Duration::ZERO, Some(Duration::from_secs(2)));
        assert_eq!(d.remaining(Duration::from_secs(1)), Some(Duration::from_secs(1)));
        assert_eq!(d.remaining(Duration::from_secs(9)), Some(Duration::ZERO));
        assert!(d.passed(Duration::from_secs(2)));
    }
}
