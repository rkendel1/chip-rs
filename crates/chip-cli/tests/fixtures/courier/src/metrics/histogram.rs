use std::sync::Mutex;
use std::time::Duration;

/// Fixed millisecond buckets; the last bucket is "everything slower".
pub const BOUNDS_MS: [u64; 8] = [1, 5, 10, 50, 100, 500, 1000, 5000];

#[derive(Debug, Default)]
pub struct Histogram {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    buckets: [u64; BOUNDS_MS.len() + 1],
    count: u64,
    sum_ms: u128,
}

impl Histogram {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&self, d: Duration) {
        let ms = d.as_millis() as u64;
        let slot = BOUNDS_MS.iter().position(|b| ms <= *b).unwrap_or(BOUNDS_MS.len());
        let mut i = self.inner.lock().unwrap();
        i.buckets[slot] += 1;
        i.count += 1;
        i.sum_ms += ms as u128;
    }

    pub fn count(&self) -> u64 {
        self.inner.lock().unwrap().count
    }

    pub fn mean_ms(&self) -> Option<f64> {
        let i = self.inner.lock().unwrap();
        (i.count > 0).then(|| i.sum_ms as f64 / i.count as f64)
    }

    pub fn buckets(&self) -> Vec<u64> {
        self.inner.lock().unwrap().buckets.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_by_upper_bound() {
        let h = Histogram::new();
        h.observe(Duration::from_millis(1));
        h.observe(Duration::from_millis(7));
        h.observe(Duration::from_secs(60));
        let b = h.buckets();
        assert_eq!(b[0], 1);
        assert_eq!(b[2], 1);
        assert_eq!(b[BOUNDS_MS.len()], 1);
        assert_eq!(h.count(), 3);
    }

    #[test]
    fn mean() {
        let h = Histogram::new();
        assert_eq!(h.mean_ms(), None);
        h.observe(Duration::from_millis(10));
        h.observe(Duration::from_millis(30));
        assert_eq!(h.mean_ms(), Some(20.0));
    }
}
