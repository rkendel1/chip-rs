//! How many attempts a request gets.

/// Attempts allowed for every request.
pub const STANDARD_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    max_attempts: u32,
}

impl Retry {
    /// The behaviour of every release before retries became tunable: three attempts.
    pub const fn standard() -> Self {
        Self {
            max_attempts: STANDARD_ATTEMPTS,
        }
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Whether another attempt may be made after `made` attempts.
    pub fn permits(&self, made: u32) -> bool {
        made < self.max_attempts
    }
}

impl Default for Retry {
    fn default() -> Self {
        Self::standard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_is_three_attempts() {
        assert_eq!(Retry::standard().max_attempts(), 3);
        assert_eq!(Retry::default(), Retry::standard());
    }

    #[test]
    fn permits_up_to_the_limit() {
        let r = Retry::standard();
        assert!(r.permits(0));
        assert!(r.permits(2));
        assert!(!r.permits(3));
        assert!(!r.permits(4));
    }
}
