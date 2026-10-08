//! The one error type every public operation returns.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Config,
    Url,
    Transport,
    Status,
    RetriesExhausted,
    DeadlineExceeded,
    WaitTooLong,
    CircuitOpen,
    RateLimited,
    Journal,
    Io,
    Usage,
}

impl ErrorKind {
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Config => "config",
            ErrorKind::Url => "url",
            ErrorKind::Transport => "transport",
            ErrorKind::Status => "status",
            ErrorKind::RetriesExhausted => "retries_exhausted",
            ErrorKind::DeadlineExceeded => "deadline_exceeded",
            ErrorKind::WaitTooLong => "wait_too_long",
            ErrorKind::CircuitOpen => "circuit_open",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::Journal => "journal",
            ErrorKind::Io => "io",
            ErrorKind::Usage => "usage",
        }
    }

    /// Exit code used by the command line interface.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::Usage => 2,
            ErrorKind::Config | ErrorKind::Url => 3,
            ErrorKind::Io | ErrorKind::Journal => 4,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourierError {
    pub kind: ErrorKind,
    pub message: String,
    /// How many attempts were made before the error was returned (0 when none were).
    pub attempts: u32,
}

impl CourierError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            attempts: 0,
        }
    }

    pub fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }
}

impl fmt::Display for CourierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.attempts > 0 {
            write!(
                f,
                "{}: {} (after {} attempt{})",
                self.kind.name(),
                self.message,
                self.attempts,
                if self.attempts == 1 { "" } else { "s" }
            )
        } else {
            write!(f, "{}: {}", self.kind.name(), self.message)
        }
    }
}

impl std::error::Error for CourierError {}

impl From<std::io::Error> for CourierError {
    fn from(e: std::io::Error) -> Self {
        CourierError::io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_mentions_attempts_only_when_present() {
        let e = CourierError::new(ErrorKind::Status, "boom");
        assert_eq!(e.to_string(), "status: boom");
        assert_eq!(
            e.with_attempts(1).to_string(),
            "status: boom (after 1 attempt)"
        );
    }

    #[test]
    fn plural_attempts() {
        let e = CourierError::new(ErrorKind::RetriesExhausted, "gave up").with_attempts(3);
        assert!(e.to_string().ends_with("(after 3 attempts)"));
    }

    #[test]
    fn exit_codes_group_by_cause() {
        assert_eq!(ErrorKind::Usage.exit_code(), 2);
        assert_eq!(ErrorKind::Config.exit_code(), 3);
        assert_eq!(ErrorKind::Url.exit_code(), 3);
        assert_eq!(ErrorKind::Io.exit_code(), 4);
        assert_eq!(ErrorKind::Status.exit_code(), 1);
    }

    #[test]
    fn io_errors_convert() {
        let e: CourierError = std::io::Error::new(std::io::ErrorKind::NotFound, "gone").into();
        assert_eq!(e.kind, ErrorKind::Io);
        assert_eq!(e.message, "gone");
    }

    #[test]
    fn names_are_unique() {
        let kinds = [
            ErrorKind::Config,
            ErrorKind::Url,
            ErrorKind::Transport,
            ErrorKind::Status,
            ErrorKind::RetriesExhausted,
            ErrorKind::DeadlineExceeded,
            ErrorKind::WaitTooLong,
            ErrorKind::CircuitOpen,
            ErrorKind::RateLimited,
            ErrorKind::Journal,
            ErrorKind::Io,
            ErrorKind::Usage,
        ];
        let mut names: Vec<_> = kinds.iter().map(|k| k.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), kinds.len());
    }
}
