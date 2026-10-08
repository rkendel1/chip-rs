//! Environment overrides. `COURIER_<SECTION>_<KEY>` sets `<section>.<key>`; for example
//! `COURIER_CLIENT_TIMEOUT=2s` sets `client.timeout`. Only the fixed sections can be set this way
//! (route sections are file-only), and unrecognised names are ignored.

use super::parser::RawConfig;
use super::schema::SECTIONS;

pub const PREFIX: &str = "COURIER_";

pub fn from_pairs(pairs: &[(String, String)]) -> RawConfig {
    let mut raw = RawConfig::new();
    for (name, value) in pairs {
        let Some(rest) = name.strip_prefix(PREFIX) else {
            continue;
        };
        let rest = rest.to_ascii_lowercase();
        // Section names may themselves contain an underscore (`rate_limit`), so match against the
        // declared sections instead of splitting at the first underscore.
        let found = SECTIONS.iter().find_map(|spec| {
            let key = rest.strip_prefix(spec.name)?.strip_prefix('_')?;
            spec.keys.contains(&key).then_some((spec.name, key))
        });
        if let Some((section, key)) = found {
            raw.set(section, key, value);
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn maps_section_and_key() {
        let raw = from_pairs(&pairs(&[("COURIER_CLIENT_TIMEOUT", "2s")]));
        assert_eq!(raw.get("client", "timeout"), Some("2s"));
    }

    #[test]
    fn handles_underscores_in_section_and_key() {
        let raw = from_pairs(&pairs(&[
            ("COURIER_RATE_LIMIT_BURST", "7"),
            ("COURIER_CLIENT_MAX_INFLIGHT", "4"),
        ]));
        assert_eq!(raw.get("rate_limit", "burst"), Some("7"));
        assert_eq!(raw.get("client", "max_inflight"), Some("4"));
    }

    #[test]
    fn ignores_unrelated_and_unknown_names() {
        let raw = from_pairs(&pairs(&[
            ("PATH", "/bin"),
            ("COURIER_BIN", "x"),
            ("COURIER_CLIENT_NOPE", "x"),
            ("COURIER_NOSECTION_KEY", "x"),
        ]));
        assert!(raw.is_empty());
    }
}
