use super::args::{Command, Parsed, USAGE};
use super::output;
use crate::client::ClientBuilder;
use crate::config::{self, render};
use crate::error::{CourierError, ErrorKind};
use crate::http::{Method, Request};
use crate::state::Journal;
use crate::transport::loopback::LoopbackTransport;
use crate::util::ManualClock;
use std::path::Path;
use std::sync::Arc;

fn read_config(parsed: &Parsed) -> Result<String, CourierError> {
    match &parsed.config_path {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| CourierError::io(format!("cannot read {path}: {e}"))),
        None => Ok(String::new()),
    }
}

pub fn execute(parsed: &Parsed, env: &[(String, String)]) -> Result<String, CourierError> {
    match &parsed.command {
        Command::Help => Ok(USAGE.to_string()),
        Command::ConfigShow => {
            let loaded = config::load(&read_config(parsed)?, env)?;
            Ok(render::render(&loaded.config))
        }
        Command::ConfigCheck => {
            let loaded = config::load(&read_config(parsed)?, env)?;
            let mut out = String::from("configuration is valid\n");
            for w in &loaded.warnings {
                out.push_str(&format!("warning: {w}\n"));
            }
            Ok(out)
        }
        Command::Send { method, url } => {
            let mut text = read_config(parsed)?;
            if let Some(j) = &parsed.journal_path {
                text.push_str(&format!("\n[journal]\nenabled = true\npath = {j}\n"));
            }
            let client = ClientBuilder::from_text(&text, env)?
                .transport(Arc::new(LoopbackTransport::new()))
                .clock(Arc::new(ManualClock::new()))
                .build()?;
            let request = Request::new(Method::parse(method)?, url)?;
            let response = client.send(request)?;
            Ok(output::response(&response))
        }
        Command::JournalTail { count } => {
            let read = open_journal(parsed)?.read()?;
            let from = read.entries.len().saturating_sub(*count);
            let mut out = String::new();
            for e in &read.entries[from..] {
                out.push_str(&output::entry_line(e));
                out.push('\n');
            }
            Ok(out)
        }
        Command::JournalStats => {
            let read = open_journal(parsed)?.read()?;
            Ok(output::stats(&read.entries, read.skipped))
        }
    }
}

fn open_journal(parsed: &Parsed) -> Result<Journal, CourierError> {
    let path = parsed
        .journal_path
        .as_deref()
        .ok_or_else(|| CourierError::new(ErrorKind::Usage, "this command needs --journal FILE"))?;
    Journal::open(Path::new(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::dispatch;

    fn run(words: &[&str]) -> Result<String, CourierError> {
        dispatch(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &[])
    }

    #[test]
    fn help_lists_commands() {
        assert!(run(&["help"]).unwrap().contains("send METHOD URL"));
    }

    #[test]
    fn config_show_prints_defaults() {
        assert!(run(&["config", "show"]).unwrap().contains("timeout = 10s"));
    }

    #[test]
    fn send_through_the_loopback() {
        let out = run(&["send", "GET", "http://h/hello"]).unwrap();
        assert!(out.starts_with("200 OK"));
        assert!(out.contains("GET /hello"));
    }

    #[test]
    fn send_retries_through_transient_failures() {
        let out = run(&["send", "GET", "http://h/x?fail=2"]).unwrap();
        assert!(out.starts_with("200 OK"));
    }

    #[test]
    fn send_reports_terminal_failures() {
        let e = run(&["send", "GET", "http://h/status/404"]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Status);
    }

    #[test]
    fn journal_commands_need_a_journal() {
        assert_eq!(run(&["journal", "stats"]).unwrap_err().kind, ErrorKind::Usage);
    }

    #[test]
    fn missing_config_file_is_an_io_error() {
        let e = run(&["--config", "/definitely/not/here.conf", "config", "show"]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Io);
    }
}
