//! Parser for the configuration file format:
//!
//! ```text
//! # comment
//! [client]
//! timeout = 5s
//! user_agent = "my app"
//!
//! [route.slow]
//! prefix = /reports
//! ```

use crate::error::CourierError;
use crate::util::text::{strip_comment, unquote};
use std::collections::BTreeMap;

/// Section name to key/value pairs, both as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawConfig {
    pub sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl RawConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, section: &str, key: &str, value: &str) {
        self.sections
            .entry(section.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(section)?.get(key).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

pub fn parse(text: &str) -> Result<RawConfig, CourierError> {
    let mut raw = RawConfig::new();
    let mut current: Option<String> = None;
    for (index, line) in text.lines().enumerate() {
        let n = index + 1;
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest
                .strip_suffix(']')
                .ok_or_else(|| CourierError::config(format!("line {n}: unterminated section header")))?
                .trim();
            if name.is_empty() {
                return Err(CourierError::config(format!("line {n}: empty section name")));
            }
            raw.sections.entry(name.to_string()).or_default();
            current = Some(name.to_string());
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| CourierError::config(format!("line {n}: expected `key = value`")))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(CourierError::config(format!("line {n}: empty key")));
        }
        let section = current
            .as_deref()
            .ok_or_else(|| CourierError::config(format!("line {n}: `{key}` appears before any section")))?;
        let table = raw.sections.get_mut(section).expect("section was created");
        if table.contains_key(key) {
            return Err(CourierError::config(format!(
                "line {n}: `{key}` is set twice in [{section}]"
            )));
        }
        table.insert(key.to_string(), unquote(value).to_string());
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_and_keys() {
        let raw = parse("[client]\ntimeout = 5s\nuser_agent = \"my app\"\n[journal]\npath=/tmp/j").unwrap();
        assert_eq!(raw.get("client", "timeout"), Some("5s"));
        assert_eq!(raw.get("client", "user_agent"), Some("my app"));
        assert_eq!(raw.get("journal", "path"), Some("/tmp/j"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let raw = parse("# top\n\n[a]\n# inside\nk = v # trailing\n").unwrap();
        assert_eq!(raw.get("a", "k"), Some("v"));
    }

    #[test]
    fn empty_sections_exist() {
        let raw = parse("[circuit]\n").unwrap();
        assert!(raw.sections.contains_key("circuit"));
    }

    #[test]
    fn key_before_section_is_an_error() {
        let e = parse("k = v").unwrap_err();
        assert!(e.message.contains("line 1"));
    }

    #[test]
    fn duplicate_keys_are_errors() {
        let e = parse("[a]\nk = 1\nk = 2").unwrap_err();
        assert!(e.message.contains("set twice"));
    }

    #[test]
    fn malformed_lines_report_their_number() {
        assert!(parse("[a]\n\nnonsense").unwrap_err().message.contains("line 3"));
        assert!(parse("[a").unwrap_err().message.contains("unterminated"));
        assert!(parse("[]").unwrap_err().message.contains("empty section"));
        assert!(parse("[a]\n= 3").unwrap_err().message.contains("empty key"));
    }

    #[test]
    fn values_may_contain_equals_signs() {
        let raw = parse("[a]\nurl = http://h/?x=1").unwrap();
        assert_eq!(raw.get("a", "url"), Some("http://h/?x=1"));
    }
}
