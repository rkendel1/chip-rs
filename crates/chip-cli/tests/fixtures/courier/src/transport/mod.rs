//! The boundary to the network. The library never opens a socket itself: every request goes
//! through a [`Transport`], which keeps the client testable and the command line deterministic.

pub mod fault;
pub mod loopback;
pub mod mock;
pub mod recorder;

use crate::http::{Request, Response};
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportErrorKind {
    Connect,
    Timeout,
    Reset,
    Protocol,
    /// A scripted transport ran out of scripted outcomes. Never retryable.
    ScriptExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    pub kind: TransportErrorKind,
    pub message: String,
}

impl TransportError {
    pub fn new(kind: TransportErrorKind, message: &str) -> Self {
        Self {
            kind,
            message: message.to_string(),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

pub trait Transport: Send + Sync {
    /// Sends one attempt. `timeout` bounds this attempt only.
    fn send(&self, request: &Request, timeout: Duration) -> Result<Response, TransportError>;
}
