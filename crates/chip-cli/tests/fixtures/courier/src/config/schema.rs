//! The typed configuration and the table of sections and keys it understands.

use super::parser::RawConfig;
use super::Loaded;
use crate::error::CourierError;
use crate::util::duration::parse_duration;
use crate::util::text::{is_identifier, parse_bool};
use std::collections::BTreeMap;
use std::time::Duration;

pub struct SectionSpec {
    pub name: &'static str,
    pub keys: &'static [&'static str],
}

/// Every fixed section and the keys it accepts. `route.<name>` sections are handled separately.
pub const SECTIONS: &[SectionSpec] = &[
    SectionSpec {
        name: "client",
        keys: &["base_url", "timeout", "deadline", "max_wait", "max_inflight", "user_agent"],
    },
    SectionSpec {
        name: "auth",
        keys: &["token", "scheme"],
    },
    SectionSpec {
        name: "journal",
        keys: &["enabled", "path"],
    },
    SectionSpec {
        name: "circuit",
        keys: &["enabled", "threshold", "cooldown"],
    },
    SectionSpec {
        name: "rate_limit",
        keys: &["enabled", "rate", "burst"],
    },
];

pub const ROUTE_KEYS: &[&str] = &["prefix", "timeout", "deadline", "max_wait", "max_inflight"];

#[derive(Debug, Clone, PartialEq)]
pub struct ClientSection {
    pub base_url: Option<String>,
    /// Bound on each attempt.
    pub timeout: Duration,
    /// Bound on the whole request, retries and waits included.
    pub deadline: Option<Duration>,
    /// Longest the client will sleep at once; a longer wait fails the request instead.
    pub max_wait: Option<Duration>,
    pub max_inflight: usize,
    pub user_agent: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthSection {
    pub token: Option<String>,
    pub scheme: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JournalSection {
    pub enabled: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CircuitSection {
    pub enabled: bool,
    pub threshold: u32,
    pub cooldown: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitSection {
    pub enabled: bool,
    pub rate: u32,
    pub burst: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteSection {
    pub name: String,
    pub prefix: String,
    pub timeout: Option<Duration>,
    pub deadline: Option<Duration>,
    pub max_wait: Option<Duration>,
    pub max_inflight: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub client: ClientSection,
    pub auth: AuthSection,
    pub journal: JournalSection,
    pub circuit: CircuitSection,
    pub rate_limit: RateLimitSection,
    pub routes: Vec<RouteSection>,
}

/// Typed access to one section's keys. Whatever is not taken is reported as unknown.
struct Fields<'a> {
    section: String,
    values: BTreeMap<&'a str, &'a str>,
}

impl<'a> Fields<'a> {
    fn new(section: &str, map: &'a BTreeMap<String, String>) -> Self {
        Self {
            section: section.to_string(),
            values: map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect(),
        }
    }

    fn text(&mut self, key: &str) -> Option<String> {
        self.values.remove(key).map(str::to_string)
    }

    fn duration(&mut self, key: &str) -> Result<Option<Duration>, CourierError> {
        match self.values.remove(key) {
            None => Ok(None),
            Some(v) => parse_duration(v)
                .map(Some)
                .map_err(|e| CourierError::config(format!("[{}] {key}: {e}", self.section))),
        }
    }

    fn uint(&mut self, key: &str) -> Result<Option<u64>, CourierError> {
        match self.values.remove(key) {
            None => Ok(None),
            Some(v) => v.trim().parse::<u64>().map(Some).map_err(|_| {
                CourierError::config(format!(
                    "[{}] {key}: `{v}` is not a non-negative integer",
                    self.section
                ))
            }),
        }
    }

    fn boolean(&mut self, key: &str) -> Result<Option<bool>, CourierError> {
        match self.values.remove(key) {
            None => Ok(None),
            Some(v) => parse_bool(v).map(Some).ok_or_else(|| {
                CourierError::config(format!("[{}] {key}: `{v}` is not a boolean", self.section))
            }),
        }
    }

    fn leftover(self, warnings: &mut Vec<String>) {
        for key in self.values.keys() {
            warnings.push(format!("[{}] unknown key `{key}` ignored", self.section));
        }
    }
}

fn empty() -> BTreeMap<String, String> {
    BTreeMap::new()
}

impl Config {
    pub fn from_raw(raw: &RawConfig) -> Result<Loaded, CourierError> {
        let mut warnings = Vec::new();
        let none = empty();
        let section = |name: &str| raw.sections.get(name).unwrap_or(&none);

        let mut f = Fields::new("client", section("client"));
        let client = ClientSection {
            base_url: f.text("base_url"),
            timeout: f.duration("timeout")?.unwrap_or(Duration::from_secs(10)),
            deadline: f.duration("deadline")?,
            max_wait: f.duration("max_wait")?,
            max_inflight: f.uint("max_inflight")?.unwrap_or(32) as usize,
            user_agent: f.text("user_agent").unwrap_or_else(|| "courier".to_string()),
        };
        f.leftover(&mut warnings);

        let mut f = Fields::new("auth", section("auth"));
        let auth = AuthSection {
            token: f.text("token"),
            scheme: f.text("scheme").unwrap_or_else(|| "bearer".to_string()),
        };
        f.leftover(&mut warnings);

        let mut f = Fields::new("journal", section("journal"));
        let journal = JournalSection {
            enabled: f.boolean("enabled")?.unwrap_or(false),
            path: f.text("path"),
        };
        f.leftover(&mut warnings);

        let mut f = Fields::new("circuit", section("circuit"));
        let circuit = CircuitSection {
            enabled: f.boolean("enabled")?.unwrap_or(true),
            threshold: f.uint("threshold")?.unwrap_or(5) as u32,
            cooldown: f.duration("cooldown")?.unwrap_or(Duration::from_secs(30)),
        };
        f.leftover(&mut warnings);

        let mut f = Fields::new("rate_limit", section("rate_limit"));
        let rate_limit = RateLimitSection {
            enabled: f.boolean("enabled")?.unwrap_or(false),
            rate: f.uint("rate")?.unwrap_or(100) as u32,
            burst: f.uint("burst")?.unwrap_or(20) as u32,
        };
        f.leftover(&mut warnings);

        let mut routes = Vec::new();
        for (name, map) in &raw.sections {
            if let Some(route) = name.strip_prefix("route.") {
                if !is_identifier(route) {
                    return Err(CourierError::config(format!(
                        "[{name}]: route names may use letters, digits, `-` and `_` only"
                    )));
                }
                let mut f = Fields::new(name, map);
                let prefix = f.text("prefix").ok_or_else(|| {
                    CourierError::config(format!("[{name}] needs a `prefix`"))
                })?;
                routes.push(RouteSection {
                    name: route.to_string(),
                    prefix,
                    timeout: f.duration("timeout")?,
                    deadline: f.duration("deadline")?,
                    max_wait: f.duration("max_wait")?,
                    max_inflight: f.uint("max_inflight")?.map(|n| n as usize),
                });
                f.leftover(&mut warnings);
            } else if !SECTIONS.iter().any(|s| s.name == name) {
                warnings.push(format!("unknown section [{name}] ignored"));
            }
        }

        Ok(Loaded {
            config: Config {
                client,
                auth,
                journal,
                circuit,
                rate_limit,
                routes,
            },
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parser::parse;

    fn load(text: &str) -> Result<Loaded, CourierError> {
        Config::from_raw(&parse(text).unwrap())
    }

    #[test]
    fn empty_input_gives_the_defaults() {
        let l = load("").unwrap();
        assert_eq!(l.config.client.timeout, Duration::from_secs(10));
        assert_eq!(l.config.client.max_inflight, 32);
        assert!(l.config.circuit.enabled);
        assert!(!l.config.rate_limit.enabled);
        assert!(l.warnings.is_empty());
    }

    #[test]
    fn reads_typed_values() {
        let l = load("[client]\ntimeout = 250ms\nmax_inflight = 4\nmax_wait = 5s\n[circuit]\nthreshold = 2\n").unwrap();
        assert_eq!(l.config.client.timeout, Duration::from_millis(250));
        assert_eq!(l.config.client.max_inflight, 4);
        assert_eq!(l.config.client.max_wait, Some(Duration::from_secs(5)));
        assert_eq!(l.config.circuit.threshold, 2);
    }

    #[test]
    fn unknown_keys_and_sections_are_warnings() {
        let l = load("[client]\nbogus = 1\n[nothing]\nx = 1\n").unwrap();
        assert_eq!(l.warnings.len(), 2);
        assert!(l.warnings[0].contains("unknown key `bogus`"));
        assert!(l.warnings[1].contains("unknown section [nothing]"));
    }

    #[test]
    fn bad_values_are_errors_naming_the_key() {
        let e = load("[client]\ntimeout = soon\n").unwrap_err();
        assert!(e.message.contains("[client] timeout"));
        assert!(load("[circuit]\nthreshold = many\n").is_err());
        assert!(load("[journal]\nenabled = maybe\n").is_err());
    }

    #[test]
    fn routes_need_a_prefix_and_a_sane_name() {
        assert!(load("[route.a]\ntimeout = 1s\n").is_err());
        assert!(load("[route.a b]\nprefix = /x\n").is_err());
        let l = load("[route.reports]\nprefix = /reports\ntimeout = 30s\n").unwrap();
        assert_eq!(l.config.routes[0].name, "reports");
        assert_eq!(l.config.routes[0].timeout, Some(Duration::from_secs(30)));
        assert_eq!(l.config.routes[0].max_inflight, None);
    }

    #[test]
    fn section_table_has_no_duplicates() {
        let mut names: Vec<_> = SECTIONS.iter().map(|s| s.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), SECTIONS.len());
        for s in SECTIONS {
            let mut keys = s.keys.to_vec();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), s.keys.len(), "{}", s.name);
        }
    }
}
