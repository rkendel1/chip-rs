use std::fmt;

/// Why a session could not be read. Every variant names a rule the format enforces; none of them
/// is retried or repaired here.
#[derive(Debug)]
pub enum SessionError {
    InvalidSessionId,
    /// A required file is absent.
    Missing(String),
    /// A path is a symlink, a directory, or has more than one hard link.
    UnsafePath(String),
    /// A managed file is not valid for its schema (bad JSON, wrong or extra keys, over size).
    InvalidFormat {
        file: String,
        reason: String,
    },
    EventFrameTooLarge {
        offset: u64,
    },
    /// The log ends inside a frame at or before the committed watermark.
    TruncatedFrame {
        offset: u64,
    },
    InvalidFrame {
        offset: u64,
        reason: String,
    },
    UnsupportedSchema {
        offset: u64,
        found: u64,
    },
    GenerationMismatch {
        offset: u64,
    },
    SequenceGap {
        offset: u64,
        expected: u64,
        found: u64,
    },
    /// The watermark does not describe a real boundary of the log.
    WatermarkMismatch(String),
    /// A write could not be made durable and was rolled back; nothing was published.
    PersistenceDegraded(String),
    /// A write may or may not have been published; the next open resolves it from the intent.
    CommitIndeterminate(String),
    /// The session directory or a managed file is not private (0700 / 0600).
    PrivateStatePermissions(String),
    /// Another writer holds the session lock.
    Locked,
    /// Creating a session whose directory already exists.
    AlreadyExists,
    /// An interrupted publication cannot be resolved to the prior or the proposed position.
    RecoveryFailed(String),
    /// A well-formed event or turn of a kind this implementation does not interpret (for example
    /// a compacted summary or a state replacement). Never guessed at, never skipped.
    Unsupported(String),
    /// Test-only: a configured fault point stopped the writer as if the process had died there.
    SimulatedCrash(&'static str),
    Io(std::io::Error),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId => write!(f, "invalid session id"),
            Self::Missing(name) => write!(f, "{name} is missing"),
            Self::UnsafePath(path) => write!(f, "unsafe session path: {path}"),
            Self::InvalidFormat { file, reason } => {
                write!(f, "{file} is not a valid session file: {reason}")
            }
            Self::EventFrameTooLarge { offset } => {
                write!(f, "event frame at byte {offset} is too large")
            }
            Self::TruncatedFrame { offset } => {
                write!(f, "event frame at byte {offset} is truncated")
            }
            Self::InvalidFrame { offset, reason } => {
                write!(f, "invalid event frame at byte {offset}: {reason}")
            }
            Self::UnsupportedSchema { offset, found } => {
                write!(
                    f,
                    "event frame at byte {offset} has unsupported schema_version {found}"
                )
            }
            Self::GenerationMismatch { offset } => write!(
                f,
                "event frame at byte {offset} is from another log generation"
            ),
            Self::SequenceGap {
                offset,
                expected,
                found,
            } => {
                write!(
                    f,
                    "event frame at byte {offset}: expected seq {expected}, found {found}"
                )
            }
            Self::WatermarkMismatch(reason) => {
                write!(f, "commit watermark does not match the log: {reason}")
            }
            Self::PersistenceDegraded(why) => {
                write!(f, "persistence degraded, nothing published: {why}")
            }
            Self::CommitIndeterminate(why) => write!(f, "commit indeterminate: {why}"),
            Self::PrivateStatePermissions(what) => write!(f, "not private: {what}"),
            Self::Locked => write!(f, "the session is locked by another writer"),
            Self::AlreadyExists => write!(f, "the session already exists"),
            Self::RecoveryFailed(why) => {
                write!(f, "interrupted publication cannot be resolved: {why}")
            }
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::SimulatedCrash(at) => write!(f, "simulated crash at {at}"),
            Self::Io(error) => write!(f, "session i/o: {error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
