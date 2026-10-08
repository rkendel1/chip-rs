use super::Prepare;
use crate::http::Request;

/// Sets the `Authorization` header from the configured scheme and token.
pub struct Authorization {
    value: String,
}

impl Authorization {
    pub fn new(scheme: &str, token: &str) -> Self {
        let label = if scheme == "basic" { "Basic" } else { "Bearer" };
        Self {
            value: format!("{label} {token}"),
        }
    }
}

impl Prepare for Authorization {
    fn prepare(&self, request: &mut Request) {
        request.headers.set("Authorization", &self.value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_and_basic() {
        let mut r = Request::get("http://h/").unwrap();
        Authorization::new("bearer", "t").prepare(&mut r);
        assert_eq!(r.headers.get("authorization"), Some("Bearer t"));
        Authorization::new("basic", "u:p").prepare(&mut r);
        assert_eq!(r.headers.get("authorization"), Some("Basic u:p"));
    }

    #[test]
    fn replaces_a_caller_supplied_header() {
        let mut r = Request::get("http://h/").unwrap().with_header("authorization", "old");
        Authorization::new("bearer", "new").prepare(&mut r);
        assert_eq!(r.headers.get_all("authorization"), vec!["Bearer new"]);
    }
}
