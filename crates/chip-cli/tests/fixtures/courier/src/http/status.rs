use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Status(pub u16);

impl Status {
    pub const OK: Status = Status(200);
    pub const NOT_FOUND: Status = Status(404);
    pub const TOO_MANY_REQUESTS: Status = Status(429);
    pub const INTERNAL_SERVER_ERROR: Status = Status(500);
    pub const BAD_GATEWAY: Status = Status(502);
    pub const SERVICE_UNAVAILABLE: Status = Status(503);
    pub const GATEWAY_TIMEOUT: Status = Status(504);

    pub fn code(self) -> u16 {
        self.0
    }

    pub fn is_informational(self) -> bool {
        (100..200).contains(&self.0)
    }

    pub fn is_success(self) -> bool {
        (200..300).contains(&self.0)
    }

    pub fn is_redirection(self) -> bool {
        (300..400).contains(&self.0)
    }

    pub fn is_client_error(self) -> bool {
        (400..500).contains(&self.0)
    }

    pub fn is_server_error(self) -> bool {
        (500..600).contains(&self.0)
    }

    pub fn reason(self) -> &'static str {
        match self.0 {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            301 => "Moved Permanently",
            302 => "Found",
            304 => "Not Modified",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            408 => "Request Timeout",
            409 => "Conflict",
            410 => "Gone",
            422 => "Unprocessable Entity",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Unknown",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.0, self.reason())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_partition_the_range() {
        for code in 100u16..600 {
            let s = Status(code);
            let n = [
                s.is_informational(),
                s.is_success(),
                s.is_redirection(),
                s.is_client_error(),
                s.is_server_error(),
            ]
            .iter()
            .filter(|b| **b)
            .count();
            assert_eq!(n, 1, "status {code}");
        }
    }

    #[test]
    fn display_includes_reason() {
        assert_eq!(Status::TOO_MANY_REQUESTS.to_string(), "429 Too Many Requests");
        assert_eq!(Status(299).to_string(), "299 Unknown");
    }

    #[test]
    fn constants() {
        assert!(Status::OK.is_success());
        assert!(Status::BAD_GATEWAY.is_server_error());
        assert!(Status::NOT_FOUND.is_client_error());
    }
}
