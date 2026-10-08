//! A deliberately small URL type: `scheme://host[:port]/path[?query]`.

use crate::error::{CourierError, ErrorKind};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
}

impl Url {
    pub fn parse(text: &str) -> Result<Self, CourierError> {
        let bad = |why: &str| CourierError::new(ErrorKind::Url, format!("{why}: `{text}`"));
        let (scheme, rest) = text.split_once("://").ok_or_else(|| bad("missing scheme"))?;
        if scheme != "http" && scheme != "https" {
            return Err(bad("unsupported scheme"));
        }
        let (authority, tail) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return Err(bad("missing host"));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                let port: u16 = p.parse().map_err(|_| bad("invalid port"))?;
                (h, Some(port))
            }
            None => (authority, None),
        };
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return Err(bad("invalid host"));
        }
        let (path, query) = match tail.split_once('?') {
            Some((p, q)) => (p, Some(q.to_string())),
            None => (tail, None),
        };
        Ok(Url {
            scheme: scheme.to_string(),
            host: host.to_ascii_lowercase(),
            port,
            path: if path.is_empty() { "/".to_string() } else { path.to_string() },
            query,
        })
    }

    pub fn effective_port(&self) -> u16 {
        self.port
            .unwrap_or(if self.scheme == "https" { 443 } else { 80 })
    }

    /// Value of a query parameter, without percent-decoding.
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query.as_deref()?.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (k == name).then_some(v)
        })
    }

    pub fn origin(&self) -> String {
        match self.port {
            Some(p) => format!("{}://{}:{}", self.scheme, self.host, p),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.origin(), self.path)?;
        if let Some(q) = &self.query {
            write!(f, "?{q}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_parts() {
        let u = Url::parse("https://Example.com:8443/a/b?x=1&y=2").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, Some(8443));
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query.as_deref(), Some("x=1&y=2"));
    }

    #[test]
    fn path_defaults_to_root() {
        assert_eq!(Url::parse("http://h").unwrap().path, "/");
        assert_eq!(Url::parse("http://h?q=1").unwrap().path, "/");
    }

    #[test]
    fn effective_port_follows_scheme() {
        assert_eq!(Url::parse("http://h").unwrap().effective_port(), 80);
        assert_eq!(Url::parse("https://h").unwrap().effective_port(), 443);
        assert_eq!(Url::parse("http://h:81").unwrap().effective_port(), 81);
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["h/x", "ftp://h", "http://", "http://h:99999", "http://a b/"] {
            assert_eq!(Url::parse(bad).unwrap_err().kind, ErrorKind::Url, "{bad}");
        }
    }

    #[test]
    fn query_params() {
        let u = Url::parse("http://h/p?fail=2&flag").unwrap();
        assert_eq!(u.query_param("fail"), Some("2"));
        assert_eq!(u.query_param("flag"), Some(""));
        assert_eq!(u.query_param("nope"), None);
    }

    #[test]
    fn display_round_trips() {
        let text = "https://host:9/a?b=c";
        assert_eq!(Url::parse(text).unwrap().to_string(), text);
    }
}
