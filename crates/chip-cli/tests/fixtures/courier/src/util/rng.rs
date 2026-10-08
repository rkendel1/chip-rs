//! A tiny deterministic generator (xorshift64*), used for jitter and fault schedules. Not for
//! anything that needs unpredictability.

#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..bound` (`0` when `bound` is 0).
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }

    /// A value in `[0.0, 1.0)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::seeded(7);
        let mut b = Rng::seeded(7);
        for _ in 0..16 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_differ() {
        let mut a = Rng::seeded(1);
        let mut b = Rng::seeded(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn zero_seed_is_usable() {
        let mut r = Rng::seeded(0);
        assert_ne!(r.next_u64(), 0);
    }

    #[test]
    fn below_respects_bound() {
        let mut r = Rng::seeded(3);
        for _ in 0..200 {
            assert!(r.below(10) < 10);
        }
        assert_eq!(r.below(0), 0);
    }

    #[test]
    fn unit_is_in_range() {
        let mut r = Rng::seeded(9);
        for _ in 0..200 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
        }
    }
}
