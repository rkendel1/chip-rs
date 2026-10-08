//! An in-memory log of what the client did, newest last. Bounded: old lines are dropped.

use std::collections::VecDeque;
use std::sync::Mutex;

pub struct EventLog {
    capacity: usize,
    lines: Mutex<VecDeque<String>>,
}

impl EventLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            lines: Mutex::new(VecDeque::new()),
        }
    }

    pub fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() == self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order() {
        let l = EventLog::new(10);
        l.push("a".into());
        l.push("b".into());
        assert_eq!(l.lines(), vec!["a", "b"]);
    }

    #[test]
    fn drops_the_oldest_beyond_capacity() {
        let l = EventLog::new(2);
        for s in ["a", "b", "c"] {
            l.push(s.into());
        }
        assert_eq!(l.lines(), vec!["b", "c"]);
    }

    #[test]
    fn zero_capacity_still_holds_one_line() {
        let l = EventLog::new(0);
        l.push("a".into());
        l.push("b".into());
        assert_eq!(l.lines(), vec!["b"]);
    }
}
