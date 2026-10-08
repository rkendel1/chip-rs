//! Delay between attempts: exponential, capped, without jitter.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    base: Duration,
    factor: u32,
    cap: Duration,
}

impl Backoff {
    /// 100ms, doubling, never more than 5s.
    pub const fn standard() -> Self {
        Self {
            base: Duration::from_millis(100),
            factor: 2,
            cap: Duration::from_secs(5),
        }
    }

    pub const fn exponential(base: Duration, factor: u32, cap: Duration) -> Self {
        Self { base, factor, cap }
    }

    /// The pause after `made` attempts have been made (`made >= 1`).
    pub fn delay(&self, made: u32) -> Duration {
        let exponent = made.saturating_sub(1).min(32);
        let mut d = self.base;
        for _ in 0..exponent {
            d = d.saturating_mul(self.factor);
            if d >= self.cap {
                return self.cap;
            }
        }
        d.min(self.cap)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::standard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_sequence() {
        let b = Backoff::standard();
        let ms: Vec<_> = (1..=8).map(|n| b.delay(n).as_millis()).collect();
        assert_eq!(ms, vec![100, 200, 400, 800, 1600, 3200, 5000, 5000]);
    }

    #[test]
    fn zero_attempts_use_the_base() {
        assert_eq!(Backoff::standard().delay(0), Duration::from_millis(100));
    }

    #[test]
    fn huge_attempt_counts_do_not_overflow() {
        assert_eq!(Backoff::standard().delay(u32::MAX), Duration::from_secs(5));
    }

    #[test]
    fn custom_curves() {
        let b = Backoff::exponential(Duration::from_secs(1), 3, Duration::from_secs(20));
        assert_eq!(b.delay(1), Duration::from_secs(1));
        assert_eq!(b.delay(2), Duration::from_secs(3));
        assert_eq!(b.delay(3), Duration::from_secs(9));
        assert_eq!(b.delay(4), Duration::from_secs(20));
    }

    #[test]
    fn factor_one_is_constant() {
        let b = Backoff::exponential(Duration::from_millis(50), 1, Duration::from_secs(1));
        assert_eq!(b.delay(1), b.delay(9));
    }
}
