//! Merging of configuration layers. Later layers win, key by key: a layer that sets only
//! `client.timeout` leaves the lower layers' other `client` keys alone.

use super::parser::RawConfig;

#[derive(Debug, Default)]
pub struct Layers {
    layers: Vec<(String, RawConfig)>,
}

impl Layers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, name: &str, raw: RawConfig) {
        self.layers.push((name.to_string(), raw));
    }

    pub fn names(&self) -> Vec<&str> {
        self.layers.iter().map(|(n, _)| n.as_str()).collect()
    }

    pub fn merge(&self) -> RawConfig {
        let mut out = RawConfig::new();
        for (_, layer) in &self.layers {
            for (section, keys) in &layer.sections {
                let target = out.sections.entry(section.clone()).or_default();
                for (k, v) in keys {
                    target.insert(k.clone(), v.clone());
                }
            }
        }
        out
    }

    /// Which layer last set `section.key`, if any.
    pub fn origin(&self, section: &str, key: &str) -> Option<&str> {
        self.layers
            .iter()
            .rev()
            .find(|(_, raw)| raw.get(section, key).is_some())
            .map(|(n, _)| n.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pairs: &[(&str, &str, &str)]) -> RawConfig {
        let mut r = RawConfig::new();
        for (s, k, v) in pairs {
            r.set(s, k, v);
        }
        r
    }

    #[test]
    fn later_layers_win_per_key() {
        let mut l = Layers::new();
        l.push("a", raw(&[("client", "timeout", "1s"), ("client", "user_agent", "x")]));
        l.push("b", raw(&[("client", "timeout", "2s")]));
        let m = l.merge();
        assert_eq!(m.get("client", "timeout"), Some("2s"));
        assert_eq!(m.get("client", "user_agent"), Some("x"));
    }

    #[test]
    fn sections_are_unioned() {
        let mut l = Layers::new();
        l.push("a", raw(&[("client", "timeout", "1s")]));
        l.push("b", raw(&[("journal", "enabled", "true")]));
        let m = l.merge();
        assert_eq!(m.sections.len(), 2);
    }

    #[test]
    fn origin_reports_the_last_setter() {
        let mut l = Layers::new();
        l.push("defaults", raw(&[("client", "timeout", "1s")]));
        l.push("file", raw(&[("client", "timeout", "2s")]));
        l.push("env", raw(&[]));
        assert_eq!(l.origin("client", "timeout"), Some("file"));
        assert_eq!(l.origin("client", "nope"), None);
        assert_eq!(l.names(), vec!["defaults", "file", "env"]);
    }

    #[test]
    fn empty_merge_is_empty() {
        assert!(Layers::new().merge().is_empty());
    }
}
