//! An append-only record of finished requests.
//!
//! One line per request: `time_ms, route, method, url, attempts, outcome, detail`. Lines that do
//! not parse are skipped when reading and counted, never silently fixed.

use super::codec::{decode_line, encode_line};
use crate::error::{CourierError, ErrorKind};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
    Rejected,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Rejected => "rejected",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "success" => Some(Outcome::Success),
            "failure" => Some(Outcome::Failure),
            "rejected" => Some(Outcome::Rejected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub time_ms: u64,
    pub route: String,
    pub method: String,
    pub url: String,
    pub attempts: u32,
    pub outcome: Outcome,
    pub detail: String,
}

impl Entry {
    pub fn to_line(&self) -> String {
        encode_line(&[
            &self.time_ms.to_string(),
            &self.route,
            &self.method,
            &self.url,
            &self.attempts.to_string(),
            self.outcome.as_str(),
            &self.detail,
        ])
    }

    pub fn from_line(line: &str) -> Result<Self, String> {
        let f = decode_line(line)?;
        if f.len() != 7 {
            return Err(format!("expected 7 fields, found {}", f.len()));
        }
        Ok(Entry {
            time_ms: f[0].parse().map_err(|_| "bad time".to_string())?,
            route: f[1].clone(),
            method: f[2].clone(),
            url: f[3].clone(),
            attempts: f[4].parse().map_err(|_| "bad attempts".to_string())?,
            outcome: Outcome::parse(&f[5]).ok_or_else(|| format!("bad outcome `{}`", f[5]))?,
            detail: f[6].clone(),
        })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Read {
    pub entries: Vec<Entry>,
    pub skipped: usize,
}

pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self, CourierError> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: &Entry) -> Result<(), CourierError> {
        let mut f = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| CourierError::new(ErrorKind::Journal, e.to_string()))?;
        writeln!(f, "{}", entry.to_line())
            .map_err(|e| CourierError::new(ErrorKind::Journal, e.to_string()))
    }

    pub fn read(&self) -> Result<Read, CourierError> {
        let file = File::open(&self.path)?;
        let mut out = Read::default();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            match Entry::from_line(&line) {
                Ok(e) => out.entries.push(e),
                Err(_) => out.skipped += 1,
            }
        }
        Ok(out)
    }

    /// Keeps only the newest `keep` entries. Returns how many were dropped.
    pub fn truncate_to(&self, keep: usize) -> Result<usize, CourierError> {
        let read = self.read()?;
        let drop = read.entries.len().saturating_sub(keep);
        if drop == 0 {
            return Ok(0);
        }
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            for e in &read.entries[drop..] {
                writeln!(f, "{}", e.to_line())?;
            }
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u32) -> Entry {
        Entry {
            time_ms: n as u64,
            route: "default".into(),
            method: "GET".into(),
            url: format!("http://h/{n}"),
            attempts: 1,
            outcome: Outcome::Success,
            detail: String::new(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("courier-journal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("journal.log")
    }

    #[test]
    fn entries_round_trip_through_lines() {
        let mut e = entry(1);
        e.detail = "tab\there\nnewline".into();
        assert_eq!(Entry::from_line(&e.to_line()).unwrap(), e);
    }

    #[test]
    fn append_then_read() {
        let j = Journal::open(&temp("append")).unwrap();
        j.append(&entry(1)).unwrap();
        j.append(&entry(2)).unwrap();
        let r = j.read().unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[1].url, "http://h/2");
        assert_eq!(r.skipped, 0);
    }

    #[test]
    fn corrupt_lines_are_skipped_and_counted() {
        let path = temp("corrupt");
        let j = Journal::open(&path).unwrap();
        j.append(&entry(1)).unwrap();
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"garbage\n").unwrap();
        j.append(&entry(2)).unwrap();
        let r = j.read().unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.skipped, 1);
    }

    #[test]
    fn truncate_keeps_the_newest() {
        let j = Journal::open(&temp("truncate")).unwrap();
        for n in 1..=5 {
            j.append(&entry(n)).unwrap();
        }
        assert_eq!(j.truncate_to(2).unwrap(), 3);
        let urls: Vec<_> = j.read().unwrap().entries.into_iter().map(|e| e.url).collect();
        assert_eq!(urls, vec!["http://h/4", "http://h/5"]);
        assert_eq!(j.truncate_to(10).unwrap(), 0);
    }

    #[test]
    fn open_creates_missing_directories() {
        let path = temp("nested").parent().unwrap().join("a/b/journal.log");
        assert!(Journal::open(&path).is_ok());
        assert!(path.exists());
    }

    #[test]
    fn outcome_names_round_trip() {
        for o in [Outcome::Success, Outcome::Failure, Outcome::Rejected] {
            assert_eq!(Outcome::parse(o.as_str()), Some(o));
        }
        assert_eq!(Outcome::parse("nope"), None);
    }
}
