//! A circuit breaker per route. A route that fails `threshold` requests in a row is opened for
//! `cooldown`; the first request after that is a probe, and its outcome closes or re-opens it.
//!
//! The breaker counts requests, not attempts: a request that succeeded on its third attempt is
//! one success.

use crate::config::CircuitSection;
use crate::util::Clock;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteState {
    pub consecutive_failures: u32,
    pub open_until: Option<Duration>,
    pub probing: bool,
}

pub struct CircuitBreaker {
    section: CircuitSection,
    clock: Arc<dyn Clock>,
    routes: Mutex<BTreeMap<String, RouteState>>,
}

impl CircuitBreaker {
    pub fn new(section: CircuitSection, clock: Arc<dyn Clock>) -> Self {
        Self {
            section,
            clock,
            routes: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn state(&self, route: &str) -> State {
        let now = self.clock.now();
        let routes = self.routes.lock().unwrap();
        match routes.get(route) {
            Some(RouteState { open_until: Some(until), .. }) => {
                if now < *until {
                    State::Open
                } else {
                    State::HalfOpen
                }
            }
            _ => State::Closed,
        }
    }

    /// Whether a request on `route` may be sent now. A half-open route admits one probe.
    pub fn admit(&self, route: &str) -> bool {
        if !self.section.enabled {
            return true;
        }
        let now = self.clock.now();
        let mut routes = self.routes.lock().unwrap();
        let s = routes.entry(route.to_string()).or_default();
        match s.open_until {
            None => true,
            Some(until) if now < until => false,
            Some(_) => {
                if s.probing {
                    false
                } else {
                    s.probing = true;
                    true
                }
            }
        }
    }

    pub fn record_success(&self, route: &str) {
        let mut routes = self.routes.lock().unwrap();
        routes.insert(route.to_string(), RouteState::default());
    }

    pub fn record_failure(&self, route: &str) {
        if !self.section.enabled {
            return;
        }
        let now = self.clock.now();
        let mut routes = self.routes.lock().unwrap();
        let s = routes.entry(route.to_string()).or_default();
        s.consecutive_failures += 1;
        let probe_failed = s.probing;
        s.probing = false;
        if probe_failed || s.consecutive_failures >= self.section.threshold {
            s.open_until = Some(now + self.section.cooldown);
        }
    }

    pub fn snapshot(&self) -> BTreeMap<String, RouteState> {
        self.routes.lock().unwrap().clone()
    }

    pub fn restore(&self, states: BTreeMap<String, RouteState>) {
        *self.routes.lock().unwrap() = states;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ManualClock;

    fn breaker(threshold: u32) -> (CircuitBreaker, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        let b = CircuitBreaker::new(
            CircuitSection {
                enabled: true,
                threshold,
                cooldown: Duration::from_secs(10),
            },
            clock.clone(),
        );
        (b, clock)
    }

    #[test]
    fn opens_after_threshold_consecutive_failures() {
        let (b, _) = breaker(3);
        for _ in 0..2 {
            assert!(b.admit("r"));
            b.record_failure("r");
        }
        assert_eq!(b.state("r"), State::Closed);
        assert!(b.admit("r"));
        b.record_failure("r");
        assert_eq!(b.state("r"), State::Open);
        assert!(!b.admit("r"));
    }

    #[test]
    fn a_success_resets_the_streak() {
        let (b, _) = breaker(2);
        b.record_failure("r");
        b.record_success("r");
        b.record_failure("r");
        assert_eq!(b.state("r"), State::Closed);
    }

    #[test]
    fn routes_are_independent() {
        let (b, _) = breaker(1);
        b.record_failure("a");
        assert!(!b.admit("a"));
        assert!(b.admit("b"));
    }

    #[test]
    fn admits_one_probe_after_the_cooldown() {
        let (b, clock) = breaker(1);
        b.record_failure("r");
        clock.advance(Duration::from_secs(10));
        assert_eq!(b.state("r"), State::HalfOpen);
        assert!(b.admit("r"));
        assert!(!b.admit("r"), "only one probe at a time");
    }

    #[test]
    fn a_failed_probe_reopens_and_a_good_one_closes() {
        let (b, clock) = breaker(5);
        for _ in 0..5 {
            b.record_failure("r");
        }
        clock.advance(Duration::from_secs(10));
        assert!(b.admit("r"));
        b.record_failure("r");
        assert_eq!(b.state("r"), State::Open);
        clock.advance(Duration::from_secs(10));
        assert!(b.admit("r"));
        b.record_success("r");
        assert_eq!(b.state("r"), State::Closed);
        assert!(b.admit("r"));
    }

    #[test]
    fn disabled_never_blocks() {
        let clock = Arc::new(ManualClock::new());
        let b = CircuitBreaker::new(
            CircuitSection { enabled: false, threshold: 1, cooldown: Duration::from_secs(1) },
            clock,
        );
        b.record_failure("r");
        assert!(b.admit("r"));
    }

    #[test]
    fn snapshot_and_restore() {
        let (b, clock) = breaker(1);
        b.record_failure("r");
        let snap = b.snapshot();
        let (b2, _) = breaker(1);
        b2.restore(snap);
        assert!(!b2.admit("r"));
        let _ = clock;
    }
}
