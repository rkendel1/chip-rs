//! The built-in defaults, expressed in the same raw form as a file so that layering treats them
//! like any other layer.

use super::parser::RawConfig;

pub const DEFAULT_TIMEOUT: &str = "10s";
pub const DEFAULT_MAX_INFLIGHT: &str = "32";
pub const DEFAULT_USER_AGENT: &str = "courier/0.7";

pub fn default_raw() -> RawConfig {
    let mut raw = RawConfig::new();
    raw.set("client", "timeout", DEFAULT_TIMEOUT);
    raw.set("client", "max_inflight", DEFAULT_MAX_INFLIGHT);
    raw.set("client", "user_agent", DEFAULT_USER_AGENT);
    raw.set("auth", "scheme", "bearer");
    raw.set("journal", "enabled", "false");
    raw.set("circuit", "enabled", "true");
    raw.set("circuit", "threshold", "5");
    raw.set("circuit", "cooldown", "30s");
    raw.set("rate_limit", "enabled", "false");
    raw.set("rate_limit", "rate", "100");
    raw.set("rate_limit", "burst", "20");
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_section_that_has_defaults() {
        let raw = default_raw();
        for s in ["client", "auth", "journal", "circuit", "rate_limit"] {
            assert!(raw.sections.contains_key(s), "{s}");
        }
    }

    #[test]
    fn defaults_name_only_declared_keys() {
        let raw = default_raw();
        for (section, keys) in &raw.sections {
            let spec = super::super::schema::SECTIONS
                .iter()
                .find(|s| s.name == section)
                .expect("declared section");
            for k in keys.keys() {
                assert!(spec.keys.contains(&k.as_str()), "{section}.{k}");
            }
        }
    }
}
