//! The route table: finds the most specific route for a URL and resolves its settings at the
//! moment a request is dispatched, so overrides added later apply to later requests.

use super::matcher;
use super::settings::RouteOverrides;
use crate::config::Config;
use crate::exec::ExecSettings;
use crate::http::Url;

pub const DEFAULT_ROUTE: &str = "default";

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    name: String,
    prefix: String,
    overrides: RouteOverrides,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub route: String,
    pub settings: ExecSettings,
}

#[derive(Debug, Clone)]
pub struct RouteTable {
    base: ExecSettings,
    entries: Vec<Entry>,
}

impl RouteTable {
    pub fn new(base: ExecSettings) -> Self {
        Self {
            base,
            entries: Vec::new(),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        let mut table = Self::new(ExecSettings::from(&config.client));
        for r in &config.routes {
            table.add(&r.name, &r.prefix, RouteOverrides::from(r));
        }
        table
    }

    pub fn add(&mut self, name: &str, prefix: &str, overrides: RouteOverrides) {
        self.entries.push(Entry {
            name: name.to_string(),
            prefix: prefix.to_string(),
            overrides,
        });
    }

    /// Replaces the overrides of an existing route. Returns false when there is no such route.
    pub fn set_overrides(&mut self, name: &str, overrides: RouteOverrides) -> bool {
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(e) => {
                e.overrides = overrides;
                true
            }
            None => false,
        }
    }

    pub fn base(&self) -> &ExecSettings {
        &self.base
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    pub fn resolve(&self, url: &Url) -> Resolved {
        let best = self
            .entries
            .iter()
            .filter(|e| matcher::matches(&e.prefix, &url.path))
            .max_by_key(|e| matcher::specificity(&e.prefix));
        match best {
            Some(e) => Resolved {
                route: e.name.clone(),
                settings: e.overrides.apply(&self.base),
            },
            None => Resolved {
                route: DEFAULT_ROUTE.to_string(),
                settings: self.base.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn url(path: &str) -> Url {
        Url::parse(&format!("http://h{path}")).unwrap()
    }

    fn timeout(secs: u64) -> RouteOverrides {
        RouteOverrides {
            timeout: Some(Duration::from_secs(secs)),
            ..Default::default()
        }
    }

    #[test]
    fn unmatched_paths_use_the_default_route() {
        let t = RouteTable::new(ExecSettings::default());
        let r = t.resolve(&url("/x"));
        assert_eq!(r.route, "default");
        assert_eq!(r.settings, ExecSettings::default());
    }

    #[test]
    fn most_specific_prefix_wins() {
        let mut t = RouteTable::new(ExecSettings::default());
        t.add("a", "/a", timeout(1));
        t.add("ab", "/a/b", timeout(2));
        assert_eq!(t.resolve(&url("/a/b/c")).route, "ab");
        assert_eq!(t.resolve(&url("/a/x")).route, "a");
    }

    #[test]
    fn overrides_can_be_replaced_after_construction() {
        let mut t = RouteTable::new(ExecSettings::default());
        t.add("a", "/a", timeout(1));
        assert!(t.set_overrides("a", timeout(9)));
        assert_eq!(t.resolve(&url("/a")).settings.timeout, Duration::from_secs(9));
        assert!(!t.set_overrides("missing", timeout(1)));
    }

    #[test]
    fn built_from_configuration() {
        let c = crate::config::load("[route.r]\nprefix = /r\ntimeout = 2s\n", &[]).unwrap().config;
        let t = RouteTable::from_config(&c);
        assert_eq!(t.names(), vec!["r"]);
        assert_eq!(t.resolve(&url("/r/1")).settings.timeout, Duration::from_secs(2));
        assert_eq!(t.resolve(&url("/z")).settings.timeout, Duration::from_secs(10));
    }
}
