use super::Prepare;
use crate::http::{Headers, Request};

/// Adds `User-Agent` and `Accept` unless the caller set them.
pub struct DefaultHeaders {
    defaults: Headers,
}

impl DefaultHeaders {
    pub fn new(user_agent: &str) -> Self {
        let mut defaults = Headers::new();
        defaults.append("User-Agent", user_agent);
        defaults.append("Accept", "*/*");
        Self { defaults }
    }
}

impl Prepare for DefaultHeaders {
    fn prepare(&self, request: &mut Request) {
        request.headers.merge_missing(&self.defaults);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_missing_headers() {
        let mut r = Request::get("http://h/").unwrap();
        DefaultHeaders::new("ua/1").prepare(&mut r);
        assert_eq!(r.headers.get("user-agent"), Some("ua/1"));
        assert_eq!(r.headers.get("accept"), Some("*/*"));
    }

    #[test]
    fn keeps_what_the_caller_set() {
        let mut r = Request::get("http://h/").unwrap().with_header("Accept", "text/plain");
        DefaultHeaders::new("ua/1").prepare(&mut r);
        assert_eq!(r.headers.get("accept"), Some("text/plain"));
    }
}
