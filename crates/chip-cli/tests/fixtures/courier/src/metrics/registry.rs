use super::{Counter, Histogram};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Registry {
    counters: Mutex<BTreeMap<String, Arc<Counter>>>,
    pub latency: Histogram,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn counter(&self, name: &str) -> Arc<Counter> {
        self.counters
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    pub fn value(&self, name: &str) -> u64 {
        self.counters.lock().unwrap().get(name).map_or(0, |c| c.get())
    }

    /// `name value` lines in name order, then the latency summary.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (name, c) in self.counters.lock().unwrap().iter() {
            out.push_str(&format!("{name} {}\n", c.get()));
        }
        out.push_str(&format!("latency_count {}\n", self.latency.count()));
        if let Some(mean) = self.latency.mean_ms() {
            out.push_str(&format!("latency_mean_ms {mean:.1}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_shared_by_name() {
        let r = Registry::new();
        r.counter("a").inc();
        r.counter("a").inc();
        assert_eq!(r.value("a"), 2);
        assert_eq!(r.value("missing"), 0);
    }

    #[test]
    fn render_is_sorted() {
        let r = Registry::new();
        r.counter("b").inc();
        r.counter("a").add(3);
        assert!(r.render().starts_with("a 3\nb 1\n"));
    }
}
