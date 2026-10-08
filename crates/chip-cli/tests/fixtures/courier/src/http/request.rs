use super::{Headers, Method, Url};
use crate::error::CourierError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub url: Url,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: Method, url: &str) -> Result<Self, CourierError> {
        Ok(Self {
            method,
            url: Url::parse(url)?,
            headers: Headers::new(),
            body: Vec::new(),
        })
    }

    pub fn get(url: &str) -> Result<Self, CourierError> {
        Self::new(Method::Get, url)
    }

    pub fn post(url: &str, body: &[u8]) -> Result<Self, CourierError> {
        Ok(Self::new(Method::Post, url)?.with_body(body))
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.set(name, value);
        self
    }

    pub fn with_body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_compose() {
        let r = Request::post("http://h/x", b"hi").unwrap().with_header("A", "1");
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.body, b"hi");
        assert_eq!(r.headers.get("a"), Some("1"));
    }

    #[test]
    fn bad_url_is_an_error() {
        assert!(Request::get("nope").is_err());
    }
}
