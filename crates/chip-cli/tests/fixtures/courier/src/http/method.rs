use crate::error::{CourierError, ErrorKind};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Patch,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
        }
    }

    pub fn parse(text: &str) -> Result<Self, CourierError> {
        match text.to_ascii_uppercase().as_str() {
            "GET" => Ok(Method::Get),
            "HEAD" => Ok(Method::Head),
            "POST" => Ok(Method::Post),
            "PUT" => Ok(Method::Put),
            "DELETE" => Ok(Method::Delete),
            "PATCH" => Ok(Method::Patch),
            other => Err(CourierError::new(
                ErrorKind::Usage,
                format!("unknown method `{other}`"),
            )),
        }
    }

    /// Safe to repeat without changing server state (RFC 9110 section 9.2.2).
    pub fn is_idempotent(self) -> bool {
        !matches!(self, Method::Post | Method::Patch)
    }

    pub fn has_body(self) -> bool {
        matches!(self, Method::Post | Method::Put | Method::Patch)
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(Method::parse("get").unwrap(), Method::Get);
        assert_eq!(Method::parse("Patch").unwrap(), Method::Patch);
    }

    #[test]
    fn unknown_method_is_a_usage_error() {
        assert_eq!(Method::parse("FETCH").unwrap_err().kind, ErrorKind::Usage);
    }

    #[test]
    fn idempotence() {
        assert!(Method::Get.is_idempotent());
        assert!(Method::Put.is_idempotent());
        assert!(Method::Delete.is_idempotent());
        assert!(!Method::Post.is_idempotent());
        assert!(!Method::Patch.is_idempotent());
    }

    #[test]
    fn bodies() {
        assert!(Method::Post.has_body());
        assert!(!Method::Get.has_body());
    }

    #[test]
    fn display_matches_as_str() {
        assert_eq!(Method::Delete.to_string(), "DELETE");
    }
}
