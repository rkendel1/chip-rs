//! A token bucket on the client's clock.

use crate::config::RateLimitSection;
use crate::util::Clock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Bucket {
    tokens: f64,
    last: Duration,
}

pub struct RateLimiter {
    enabled: bool,
    rate: f64,
    burst: f64,
    clock: Arc<dyn Clock>,
    bucket: Mutex<Bucket>,
}

impl RateLimiter {
    pub fn new(section: &RateLimitSection, clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
        Self {
            enabled: section.enabled,
            rate: section.rate as f64,
            burst: section.burst as f64,
            clock,
            bucket: Mutex::new(Bucket {
                tokens: section.burst as f64,
                last: now,
            }),
        }
    }

    /// Takes one token if there is one.
    pub fn try_acquire(&self) -> bool {
        if !self.enabled {
            return true;
        }
        let now = self.clock.now();
        let mut b = self.bucket.lock().unwrap();
        let elapsed = now.saturating_sub(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.rate).min(self.burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    pub fn available(&self) -> u32 {
        self.bucket.lock().unwrap().tokens as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ManualClock;

    fn limiter(rate: u32, burst: u32, enabled: bool) -> (RateLimiter, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new());
        let l = RateLimiter::new(&RateLimitSection { enabled, rate, burst }, clock.clone());
        (l, clock)
    }

    #[test]
    fn disabled_always_admits() {
        let (l, _) = limiter(1, 1, false);
        for _ in 0..50 {
            assert!(l.try_acquire());
        }
    }

    #[test]
    fn burst_then_refuse() {
        let (l, _) = limiter(1, 3, true);
        assert!(l.try_acquire());
        assert!(l.try_acquire());
        assert!(l.try_acquire());
        assert!(!l.try_acquire());
    }

    #[test]
    fn refills_with_time() {
        let (l, clock) = limiter(2, 2, true);
        assert!(l.try_acquire());
        assert!(l.try_acquire());
        assert!(!l.try_acquire());
        clock.advance(Duration::from_millis(500));
        assert!(l.try_acquire());
        assert!(!l.try_acquire());
    }

    #[test]
    fn never_exceeds_burst() {
        let (l, clock) = limiter(100, 2, true);
        clock.advance(Duration::from_secs(60));
        assert!(l.try_acquire());
        assert_eq!(l.available(), 1);
    }
}
