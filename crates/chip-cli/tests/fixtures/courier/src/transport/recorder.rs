//! Records the shape of every call that passes through to an inner transport.

use super::{Transport, TransportError};
use crate::http::{Request, Response};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    pub url: String,
    pub outcome: String,
}

pub struct Recorder<T: Transport> {
    inner: T,
    calls: Mutex<Vec<Call>>,
}

impl<T: Transport> Recorder<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl<T: Transport> Transport for Recorder<T> {
    fn send(&self, request: &Request, timeout: Duration) -> Result<Response, TransportError> {
        let result = self.inner.send(request, timeout);
        let outcome = match &result {
            Ok(r) => r.status.code().to_string(),
            Err(e) => format!("{:?}", e.kind),
        };
        self.calls.lock().unwrap().push(Call {
            method: request.method.to_string(),
            url: request.url.to_string(),
            outcome,
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::{connect_error, ok, MockTransport};

    #[test]
    fn records_successes_and_errors() {
        let r = Recorder::new(MockTransport::new(vec![ok(), connect_error()]));
        let req = Request::get("http://h/a").unwrap();
        let d = Duration::from_secs(1);
        let _ = r.send(&req, d);
        let _ = r.send(&req, d);
        let calls = r.calls();
        assert_eq!(calls[0].outcome, "200");
        assert_eq!(calls[1].outcome, "Connect");
        assert_eq!(calls[0].url, "http://h/a");
    }
}
