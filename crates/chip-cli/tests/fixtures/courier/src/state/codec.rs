//! Line encoding shared by the journal and the store: tab-separated fields, with tab, newline,
//! carriage return and backslash escaped inside a field.

pub fn escape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for c in field.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

pub fn unescape(field: &str) -> Result<String, String> {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => return Err(format!("unknown escape `\\{other}`")),
            None => return Err("dangling backslash".to_string()),
        }
    }
    Ok(out)
}

pub fn encode_line(fields: &[&str]) -> String {
    fields.iter().map(|f| escape(f)).collect::<Vec<_>>().join("\t")
}

pub fn decode_line(line: &str) -> Result<Vec<String>, String> {
    line.split('\t').map(unescape).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_the_separators() {
        assert_eq!(escape("a\tb\nc\\d"), "a\\tb\\nc\\\\d");
    }

    #[test]
    fn round_trips_awkward_text() {
        for s in ["", "plain", "tab\there", "line\nbreak", "back\\slash", "\\t literal", "\r\n"] {
            assert_eq!(unescape(&escape(s)).unwrap(), s, "{s:?}");
        }
    }

    #[test]
    fn rejects_bad_escapes() {
        assert!(unescape("\\x").is_err());
        assert!(unescape("trailing\\").is_err());
    }

    #[test]
    fn lines_round_trip() {
        let line = encode_line(&["a", "b\tc", ""]);
        assert_eq!(decode_line(&line).unwrap(), vec!["a", "b\tc", ""]);
    }

    #[test]
    fn an_encoded_line_is_a_single_line() {
        assert!(!encode_line(&["x\ny", "z"]).contains('\n'));
    }
}
