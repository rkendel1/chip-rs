//! The knobs one request is executed with. A route may override some of them.

use crate::config::ClientSection;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct ExecSettings {
    /// Bound on each attempt.
    pub timeout: Duration,
    /// Bound on the whole request, retries and waits included.
    pub deadline: Option<Duration>,
    /// Longest single wait. A longer wait fails the request instead of sleeping.
    pub max_wait: Option<Duration>,
    pub max_inflight: usize,
}

impl Default for ExecSettings {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            deadline: None,
            max_wait: None,
            max_inflight: 32,
        }
    }
}

impl From<&ClientSection> for ExecSettings {
    fn from(c: &ClientSection) -> Self {
        Self {
            timeout: c.timeout,
            deadline: c.deadline,
            max_wait: c.max_wait,
            max_inflight: c.max_inflight,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_defaults;

    #[test]
    fn defaults_match_the_default_configuration() {
        let c = load_defaults().config;
        assert_eq!(ExecSettings::from(&c.client), ExecSettings::default());
    }

    #[test]
    fn conversion_copies_every_field() {
        let mut c = load_defaults().config.client;
        c.timeout = Duration::from_secs(3);
        c.deadline = Some(Duration::from_secs(9));
        c.max_wait = Some(Duration::from_secs(4));
        c.max_inflight = 7;
        let s = ExecSettings::from(&c);
        assert_eq!(s.timeout, Duration::from_secs(3));
        assert_eq!(s.deadline, Some(Duration::from_secs(9)));
        assert_eq!(s.max_wait, Some(Duration::from_secs(4)));
        assert_eq!(s.max_inflight, 7);
    }
}
