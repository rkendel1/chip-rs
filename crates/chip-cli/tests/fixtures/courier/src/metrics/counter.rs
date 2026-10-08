use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        let c = Counter::new();
        c.inc();
        c.add(4);
        assert_eq!(c.get(), 5);
    }
}
