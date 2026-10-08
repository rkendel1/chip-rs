//! Request identifiers: a process-local counter rendered as fixed-width hex.

use std::sync::atomic::{AtomicU64, Ordering};

pub struct IdGenerator {
    prefix: String,
    next: AtomicU64,
}

impl IdGenerator {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            next: AtomicU64::new(1),
        }
    }

    pub fn next(&self) -> String {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        format!("{}-{:08x}", self.prefix, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_increase_and_are_fixed_width() {
        let g = IdGenerator::new("req");
        assert_eq!(g.next(), "req-00000001");
        assert_eq!(g.next(), "req-00000002");
    }

    #[test]
    fn generators_are_independent() {
        let a = IdGenerator::new("a");
        let b = IdGenerator::new("b");
        a.next();
        assert_eq!(b.next(), "b-00000001");
    }
}
