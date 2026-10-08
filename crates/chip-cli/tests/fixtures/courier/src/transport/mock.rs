//! A scripted transport: each call returns the next scripted outcome and records the request.
//! When the script runs out it fails with a non-retryable `ScriptExhausted` error, so a client
//! that makes more calls than a test expected terminates instead of looping.

use super::{Transport, TransportError, TransportErrorKind};
use crate::http::{Request, Response};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

pub type Outcome = Result<Response, TransportError>;

pub struct MockTransport {
    script: Mutex<VecDeque<Outcome>>,
    seen: Mutex<Vec<Request>>,
    timeouts: Mutex<Vec<Duration>>,
}

impl MockTransport {
    pub fn new(script: Vec<Outcome>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            seen: Mutex::new(Vec::new()),
            timeouts: Mutex::new(Vec::new()),
        }
    }

    /// `n` copies of the same response.
    pub fn repeating(response: Response, n: usize) -> Self {
        Self::new((0..n).map(|_| Ok(response.clone())).collect())
    }

    pub fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    /// The per-attempt timeout each call was made with.
    pub fn timeouts(&self) -> Vec<Duration> {
        self.timeouts.lock().unwrap().clone()
    }

    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

impl Transport for MockTransport {
    fn send(&self, request: &Request, timeout: Duration) -> Outcome {
        self.seen.lock().unwrap().push(request.clone());
        self.timeouts.lock().unwrap().push(timeout);
        self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
            Err(TransportError::new(
                TransportErrorKind::ScriptExhausted,
                "the scripted transport has no more outcomes",
            ))
        })
    }
}

pub fn ok() -> Outcome {
    Ok(Response::new(200))
}

pub fn status(code: u16) -> Outcome {
    Ok(Response::new(code))
}

pub fn retry_after(code: u16, seconds: u64) -> Outcome {
    Ok(Response::new(code).with_header("Retry-After", &seconds.to_string()))
}

pub fn connect_error() -> Outcome {
    Err(TransportError::new(
        TransportErrorKind::Connect,
        "connection refused",
    ))
}

pub fn timeout_error() -> Outcome {
    Err(TransportError::new(TransportErrorKind::Timeout, "timed out"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Request {
        Request::get("http://h/").unwrap()
    }

    #[test]
    fn plays_the_script_in_order_then_exhausts() {
        let t = MockTransport::new(vec![status(500), ok()]);
        let d = Duration::from_secs(1);
        assert_eq!(t.send(&req(), d).unwrap().status.code(), 500);
        assert_eq!(t.send(&req(), d).unwrap().status.code(), 200);
        let e = t.send(&req(), d).unwrap_err();
        assert_eq!(e.kind, TransportErrorKind::ScriptExhausted);
        assert_eq!(t.calls(), 3);
    }

    #[test]
    fn records_requests() {
        let t = MockTransport::new(vec![ok()]);
        t.send(&req(), Duration::from_secs(1)).unwrap();
        assert_eq!(t.requests()[0].url.host, "h");
    }

    #[test]
    fn repeating_builds_n_outcomes() {
        let t = MockTransport::repeating(Response::new(503), 4);
        assert_eq!(t.remaining(), 4);
    }

    #[test]
    fn helpers_build_expected_outcomes() {
        assert_eq!(
            retry_after(429, 9).unwrap().retry_after(),
            Some(Duration::from_secs(9))
        );
        assert_eq!(connect_error().unwrap_err().kind, TransportErrorKind::Connect);
        assert_eq!(timeout_error().unwrap_err().kind, TransportErrorKind::Timeout);
    }
}
