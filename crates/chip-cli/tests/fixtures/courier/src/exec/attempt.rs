//! Bookkeeping for the attempts made on behalf of one request.

use super::classify::Failure;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptState {
    made: u32,
    history: Vec<String>,
}

impl AttemptState {
    pub fn new() -> Self {
        Self {
            made: 0,
            history: Vec::new(),
        }
    }

    /// Called when an attempt is about to be sent.
    pub fn begin(&mut self) {
        self.made += 1;
    }

    pub fn made(&self) -> u32 {
        self.made
    }

    pub fn record(&mut self, failure: &Failure) {
        self.history.push(match failure {
            Failure::Status(s, _) => format!("status {}", s.code()),
            Failure::Transport(k, _) => format!("transport {k:?}"),
        });
    }

    /// Called when the client is about to wait because the server asked it to. The attempt that
    /// drew the instruction was answered by the server, so it is not charged against the request.
    pub fn paused_for_server(&mut self) {
        self.made = self.made.saturating_sub(1);
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }
}

impl Default for AttemptState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Status;

    #[test]
    fn counts_attempts() {
        let mut s = AttemptState::new();
        s.begin();
        s.begin();
        assert_eq!(s.made(), 2);
    }

    #[test]
    fn records_a_history_line_per_failure() {
        let mut s = AttemptState::new();
        s.record(&Failure::Status(Status(503), None));
        assert_eq!(s.history(), ["status 503"]);
    }

    #[test]
    fn pausing_never_goes_below_zero() {
        let mut s = AttemptState::new();
        s.paused_for_server();
        assert_eq!(s.made(), 0);
    }
}
