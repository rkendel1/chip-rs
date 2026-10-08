//! Which failures are worth another attempt.

use crate::error::{CourierError, ErrorKind};
use crate::http::Status;
use crate::transport::TransportErrorKind;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The server answered, but not with success. Carries its `Retry-After` hint when it gave one.
    Status(Status, Option<Duration>),
    Transport(TransportErrorKind, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Retryable,
    Terminal,
}

pub fn classify(failure: &Failure) -> Disposition {
    match failure {
        Failure::Status(s, _) => match s.code() {
            408 | 429 | 500 | 502 | 503 | 504 => Disposition::Retryable,
            _ => Disposition::Terminal,
        },
        Failure::Transport(kind, _) => match kind {
            TransportErrorKind::Connect
            | TransportErrorKind::Timeout
            | TransportErrorKind::Reset => Disposition::Retryable,
            TransportErrorKind::Protocol | TransportErrorKind::ScriptExhausted => {
                Disposition::Terminal
            }
        },
    }
}

impl Failure {
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Failure::Status(_, hint) => *hint,
            Failure::Transport(..) => None,
        }
    }

    /// The error returned when this failure ends the request.
    pub fn into_error(self, kind: ErrorKind, attempts: u32) -> CourierError {
        let message = match &self {
            Failure::Status(s, _) => format!("server answered {s}"),
            Failure::Transport(k, m) => format!("{k:?}: {m}"),
        };
        CourierError::new(kind, message).with_attempts(attempts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16) -> Failure {
        Failure::Status(Status(code), None)
    }

    #[test]
    fn server_errors_and_throttling_are_retryable() {
        for code in [408, 429, 500, 502, 503, 504] {
            assert_eq!(classify(&status(code)), Disposition::Retryable, "{code}");
        }
    }

    #[test]
    fn other_statuses_are_terminal() {
        for code in [400, 401, 403, 404, 409, 410, 422, 501, 505] {
            assert_eq!(classify(&status(code)), Disposition::Terminal, "{code}");
        }
    }

    #[test]
    fn transport_failures() {
        let t = |k| Failure::Transport(k, String::new());
        assert_eq!(classify(&t(TransportErrorKind::Connect)), Disposition::Retryable);
        assert_eq!(classify(&t(TransportErrorKind::Timeout)), Disposition::Retryable);
        assert_eq!(classify(&t(TransportErrorKind::Reset)), Disposition::Retryable);
        assert_eq!(classify(&t(TransportErrorKind::Protocol)), Disposition::Terminal);
        assert_eq!(classify(&t(TransportErrorKind::ScriptExhausted)), Disposition::Terminal);
    }

    #[test]
    fn hints_only_come_from_statuses() {
        let f = Failure::Status(Status(429), Some(Duration::from_secs(3)));
        assert_eq!(f.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(Failure::Transport(TransportErrorKind::Reset, String::new()).retry_after(), None);
    }

    #[test]
    fn into_error_describes_the_failure() {
        let e = status(503).into_error(ErrorKind::RetriesExhausted, 3);
        assert_eq!(e.kind, ErrorKind::RetriesExhausted);
        assert_eq!(e.attempts, 3);
        assert!(e.message.contains("503"));
    }
}
