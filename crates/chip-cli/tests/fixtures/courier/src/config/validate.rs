//! Cross-field checks that a single key's parser cannot make.

use super::schema::Config;
use crate::error::CourierError;
use std::collections::BTreeSet;
use std::time::Duration;

pub fn check(config: &Config) -> Result<(), CourierError> {
    let c = &config.client;
    if c.timeout.is_zero() {
        return Err(CourierError::config("[client] timeout must be greater than zero"));
    }
    if c.max_inflight == 0 || c.max_inflight > 1024 {
        return Err(CourierError::config("[client] max_inflight must be between 1 and 1024"));
    }
    if let Some(d) = c.deadline {
        if d < c.timeout {
            return Err(CourierError::config("[client] deadline must not be shorter than timeout"));
        }
    }
    if let Some(w) = c.max_wait {
        if w.is_zero() {
            return Err(CourierError::config("[client] max_wait must be greater than zero"));
        }
    }
    if !matches!(config.auth.scheme.as_str(), "bearer" | "basic") {
        return Err(CourierError::config(format!(
            "[auth] scheme `{}` is not supported (use bearer or basic)",
            config.auth.scheme
        )));
    }
    if config.journal.enabled && config.journal.path.is_none() {
        return Err(CourierError::config("[journal] enabled requires a path"));
    }
    if config.circuit.threshold == 0 {
        return Err(CourierError::config("[circuit] threshold must be at least 1"));
    }
    if config.circuit.cooldown < Duration::from_millis(1) {
        return Err(CourierError::config("[circuit] cooldown must be at least 1ms"));
    }
    if config.rate_limit.enabled && (config.rate_limit.rate == 0 || config.rate_limit.burst == 0) {
        return Err(CourierError::config("[rate_limit] rate and burst must be at least 1"));
    }
    let mut prefixes = BTreeSet::new();
    for r in &config.routes {
        if !r.prefix.starts_with('/') {
            return Err(CourierError::config(format!(
                "[route.{}] prefix must start with `/`",
                r.name
            )));
        }
        if !prefixes.insert(r.prefix.clone()) {
            return Err(CourierError::config(format!(
                "[route.{}] prefix `{}` is already used by another route",
                r.name, r.prefix
            )));
        }
        if r.max_inflight == Some(0) {
            return Err(CourierError::config(format!(
                "[route.{}] max_inflight must be at least 1",
                r.name
            )));
        }
        if r.timeout.is_some_and(|t| t.is_zero()) {
            return Err(CourierError::config(format!(
                "[route.{}] timeout must be greater than zero",
                r.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::load;

    fn err(text: &str) -> String {
        load(text, &[]).unwrap_err().message
    }

    #[test]
    fn defaults_validate() {
        assert!(load("", &[]).is_ok());
    }

    #[test]
    fn zero_timeout_rejected() {
        assert!(err("[client]\ntimeout = 0ms\n").contains("timeout"));
    }

    #[test]
    fn inflight_bounds() {
        assert!(err("[client]\nmax_inflight = 0\n").contains("max_inflight"));
        assert!(err("[client]\nmax_inflight = 5000\n").contains("max_inflight"));
    }

    #[test]
    fn deadline_not_shorter_than_timeout() {
        assert!(err("[client]\ntimeout = 5s\ndeadline = 1s\n").contains("deadline"));
        assert!(load("[client]\ntimeout = 5s\ndeadline = 5s\n", &[]).is_ok());
    }

    #[test]
    fn auth_scheme() {
        assert!(err("[auth]\nscheme = digest\n").contains("scheme"));
    }

    #[test]
    fn journal_needs_path_when_enabled() {
        assert!(err("[journal]\nenabled = true\n").contains("path"));
        assert!(load("[journal]\nenabled = true\npath = /tmp/j\n", &[]).is_ok());
    }

    #[test]
    fn circuit_and_rate_limit() {
        assert!(err("[circuit]\nthreshold = 0\n").contains("threshold"));
        assert!(err("[rate_limit]\nenabled = true\nrate = 0\n").contains("rate"));
        assert!(load("[rate_limit]\nrate = 0\n", &[]).is_ok(), "disabled sections are not checked");
    }

    #[test]
    fn route_rules() {
        assert!(err("[route.a]\nprefix = x\n").contains("start with"));
        assert!(err("[route.a]\nprefix = /x\n[route.b]\nprefix = /x\n").contains("already used"));
        assert!(err("[route.a]\nprefix = /x\nmax_inflight = 0\n").contains("max_inflight"));
    }
}
