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
