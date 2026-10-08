use super::{Headers, Status};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: Status,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Self {
            status: Status(status),
            headers: Headers::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.set(name, value);
        self
    }

    pub fn with_body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }

    /// The server's `Retry-After` hint when it is a whole number of seconds. HTTP dates are not
    /// supported: they are ignored rather than guessed at.
    pub fn retry_after(&self) -> Option<Duration> {
        let raw = self.headers.get("retry-after")?.trim();
        raw.parse::<u64>().ok().map(Duration::from_secs)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds() {
        let r = Response::new(429).with_header("Retry-After", "7");
        assert_eq!(r.retry_after(), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_ignores_dates_and_garbage() {
        let d = Response::new(503).with_header("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT");
        assert_eq!(d.retry_after(), None);
        let g = Response::new(503).with_header("Retry-After", "-4");
        assert_eq!(g.retry_after(), None);
        assert_eq!(Response::new(503).retry_after(), None);
    }

    #[test]
    fn body_text() {
        assert_eq!(Response::new(200).with_body(b"ok").text(), "ok");
    }
}
