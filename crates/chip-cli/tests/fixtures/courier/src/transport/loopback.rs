//! An in-process "server" used by the command line: `GET /status/<code>` answers with that
//! status; `?fail=N` makes the first N attempts at that URL answer 503; `?retry_after=S` adds a
//! `Retry-After` header to those failures. Everything else answers 200 with an echo body.

use super::{Transport, TransportError};
use crate::http::{Request, Response};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Default)]
pub struct LoopbackTransport {
    hits: Mutex<HashMap<String, u32>>,
}

impl LoopbackTransport {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Transport for LoopbackTransport {
    fn send(&self, request: &Request, _timeout: Duration) -> Result<Response, TransportError> {
        let key = request.url.to_string();
        let hit = {
            let mut hits = self.hits.lock().unwrap();
            let n = hits.entry(key).or_insert(0);
            *n += 1;
            *n
        };
        if let Some(code) = request
            .url
            .path
            .strip_prefix("/status/")
            .and_then(|c| c.parse::<u16>().ok())
        {
            return Ok(Response::new(code));
        }
        let fail: u32 = request
            .url
            .query_param("fail")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if hit <= fail {
            let mut r = Response::new(503);
            if let Some(s) = request.url.query_param("retry_after") {
                r = r.with_header("Retry-After", s);
            }
            return Ok(r);
        }
        Ok(Response::new(200).with_body(format!("{} {}", request.method, request.url.path).as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(t: &LoopbackTransport, url: &str) -> Response {
        t.send(&Request::get(url).unwrap(), Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn echoes_by_default() {
        let t = LoopbackTransport::new();
        assert_eq!(send(&t, "http://h/a").text(), "GET /a");
    }

    #[test]
    fn status_paths() {
        let t = LoopbackTransport::new();
        assert_eq!(send(&t, "http://h/status/404").status.code(), 404);
    }

    #[test]
    fn fail_counts_attempts_per_url() {
        let t = LoopbackTransport::new();
        assert_eq!(send(&t, "http://h/x?fail=2").status.code(), 503);
        assert_eq!(send(&t, "http://h/x?fail=2").status.code(), 503);
        assert_eq!(send(&t, "http://h/x?fail=2").status.code(), 200);
        assert_eq!(send(&t, "http://h/y?fail=1").status.code(), 503);
    }

    #[test]
    fn failures_can_carry_retry_after() {
        let t = LoopbackTransport::new();
        let r = send(&t, "http://h/z?fail=1&retry_after=4");
        assert_eq!(r.retry_after(), Some(Duration::from_secs(4)));
    }
}
