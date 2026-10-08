//! A [`Session`] stamps each request with a unique `X-Request-Id` and counts what it sent.

use super::Client;
use crate::error::CourierError;
use crate::http::{Request, Response};
use crate::util::id::IdGenerator;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Session<'a> {
    client: &'a Client,
    ids: IdGenerator,
    sent: AtomicU64,
}

impl<'a> Session<'a> {
    pub fn new(client: &'a Client, prefix: &str) -> Self {
        Self {
            client,
            ids: IdGenerator::new(prefix),
            sent: AtomicU64::new(0),
        }
    }

    pub fn send(&self, request: Request) -> Result<Response, CourierError> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        self.client
            .send(request.with_header("X-Request-Id", &self.ids.next()))
    }

    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ClientBuilder;
    use crate::transport::mock::{ok, MockTransport};
    use std::sync::Arc;

    #[test]
    fn stamps_ids_and_counts() {
        let t = Arc::new(MockTransport::new(vec![ok(), ok()]));
        let c = ClientBuilder::from_text("", &[]).unwrap().transport(t.clone()).build().unwrap();
        let s = Session::new(&c, "s1");
        s.send(Request::get("http://h/a").unwrap()).unwrap();
        s.send(Request::get("http://h/b").unwrap()).unwrap();
        assert_eq!(s.sent(), 2);
        let ids: Vec<_> = t
            .requests()
            .iter()
            .map(|r| r.headers.get("x-request-id").unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["s1-00000001", "s1-00000002"]);
    }
}
