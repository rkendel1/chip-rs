//! Caps the number of requests in flight at once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct InflightGate {
    count: Arc<AtomicUsize>,
}

/// Released on drop.
pub struct Permit {
    count: Arc<AtomicUsize>,
}

impl InflightGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_enter(&self, limit: usize) -> Option<Permit> {
        let mut current = self.count.load(Ordering::SeqCst);
        loop {
            if current >= limit {
                return None;
            }
            match self.count.compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    return Some(Permit {
                        count: self.count.clone(),
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }

    pub fn current(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_the_limit_and_releases_on_drop() {
        let g = InflightGate::new();
        let a = g.try_enter(2).unwrap();
        let _b = g.try_enter(2).unwrap();
        assert!(g.try_enter(2).is_none());
        drop(a);
        assert_eq!(g.current(), 1);
        assert!(g.try_enter(2).is_some());
    }

    #[test]
    fn zero_limit_admits_nothing() {
        assert!(InflightGate::new().try_enter(0).is_none());
    }
}
