//! Text helpers shared by the parsers.

/// Strips a trailing `#` comment that is not inside double quotes.
pub fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Removes one pair of surrounding double quotes, if present.
pub fn unquote(value: &str) -> &str {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

pub fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_stripped_outside_quotes_only() {
        assert_eq!(strip_comment("a = 1 # note"), "a = 1 ");
        assert_eq!(strip_comment("a = \"x # y\""), "a = \"x # y\"");
        assert_eq!(strip_comment("# whole line"), "");
    }

    #[test]
    fn unquote_removes_one_pair() {
        assert_eq!(unquote("\"hi\""), "hi");
        assert_eq!(unquote("  plain "), "plain");
        assert_eq!(unquote("\"\""), "");
        assert_eq!(unquote("\""), "\"");
    }

    #[test]
    fn identifiers() {
        assert!(is_identifier("slow-route_1"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("a b"));
        assert!(!is_identifier("a.b"));
    }

    #[test]
    fn booleans() {
        assert_eq!(parse_bool("Yes"), Some(true));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }
}
