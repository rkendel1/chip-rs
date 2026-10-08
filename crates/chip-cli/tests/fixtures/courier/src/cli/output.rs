//! Text rendering for the command line.

use crate::http::Response;
use crate::state::journal::{Entry, Outcome};

pub fn response(r: &Response) -> String {
    let mut out = format!("{}\n", r.status);
    for (n, v) in r.headers.iter() {
        out.push_str(&format!("{n}: {v}\n"));
    }
    if !r.body.is_empty() {
        out.push('\n');
        out.push_str(&r.text());
        out.push('\n');
    }
    out
}

pub fn entry_line(e: &Entry) -> String {
    format!(
        "{:>8}ms {:<8} {:<6} {} attempts={} {}{}",
        e.time_ms,
        e.route,
        e.method,
        e.url,
        e.attempts,
        e.outcome.as_str(),
        if e.detail.is_empty() { String::new() } else { format!(" ({})", e.detail) }
    )
}

pub fn stats(entries: &[Entry], skipped: usize) -> String {
    let count = |o: Outcome| entries.iter().filter(|e| e.outcome == o).count();
    let attempts: u32 = entries.iter().map(|e| e.attempts).sum();
    format!(
        "entries {}\nsuccess {}\nfailure {}\nrejected {}\nattempts {}\nskipped {}\n",
        entries.len(),
        count(Outcome::Success),
        count(Outcome::Failure),
        count(Outcome::Rejected),
        attempts,
        skipped
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(outcome: Outcome, attempts: u32) -> Entry {
        Entry {
            time_ms: 5,
            route: "default".into(),
            method: "GET".into(),
            url: "http://h/".into(),
            attempts,
            outcome,
            detail: String::new(),
        }
    }

    #[test]
    fn response_has_status_headers_and_body() {
        let r = Response::new(200).with_header("A", "b").with_body(b"hi");
        assert_eq!(response(&r), "200 OK\nA: b\n\nhi\n");
    }

    #[test]
    fn stats_sum_up() {
        let s = stats(&[entry(Outcome::Success, 1), entry(Outcome::Failure, 3)], 2);
        assert!(s.contains("entries 2"));
        assert!(s.contains("attempts 4"));
        assert!(s.contains("skipped 2"));
    }

    #[test]
    fn entry_lines_show_detail_only_when_present() {
        let mut e = entry(Outcome::Failure, 3);
        assert!(!entry_line(&e).contains('('));
        e.detail = "boom".into();
        assert!(entry_line(&e).ends_with("(boom)"));
    }
}
