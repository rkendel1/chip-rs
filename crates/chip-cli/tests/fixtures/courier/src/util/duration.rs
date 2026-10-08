//! Parsing and formatting of the short duration syntax used in configuration: `250ms`, `2s`,
//! `1m`, `1h`. A bare number is rejected: a unit is always required.

use std::time::Duration;

pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("empty duration".to_string());
    }
    let split = t
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("duration `{t}` has no unit (use ms, s, m or h)"))?;
    let (digits, unit) = t.split_at(split);
    if digits.is_empty() {
        return Err(format!("duration `{t}` has no number"));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("duration `{t}` is too large"))?;
    let ms_per = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        other => return Err(format!("unknown duration unit `{other}` in `{t}`")),
    };
    n.checked_mul(ms_per)
        .map(Duration::from_millis)
        .ok_or_else(|| format!("duration `{t}` is too large"))
}

pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms == 0 {
        "0ms".to_string()
    } else if ms % 3_600_000 == 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms % 60_000 == 0 {
        format!("{}m", ms / 60_000)
    } else if ms % 1_000 == 0 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_unit() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("3m").unwrap(), Duration::from_secs(180));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(parse_duration("  5s ").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn rejects_bare_numbers_and_unknown_units() {
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("10d").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("-1s").is_err());
    }

    #[test]
    fn rejects_overflow() {
        assert!(parse_duration("99999999999999999999s").is_err());
        assert!(parse_duration("18446744073709551615h").is_err());
    }

    #[test]
    fn formats_in_the_largest_exact_unit() {
        assert_eq!(format_duration(Duration::from_millis(0)), "0ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
        assert_eq!(format_duration(Duration::from_secs(2)), "2s");
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
    }

    #[test]
    fn format_then_parse_round_trips() {
        for ms in [1u64, 999, 1000, 61_000, 3_600_000, 90_000] {
            let d = Duration::from_millis(ms);
            assert_eq!(parse_duration(&format_duration(d)).unwrap(), d);
        }
    }
}
