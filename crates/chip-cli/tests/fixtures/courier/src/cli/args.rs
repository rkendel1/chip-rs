use crate::error::{CourierError, ErrorKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Send { method: String, url: String },
    ConfigShow,
    ConfigCheck,
    JournalTail { count: usize },
    JournalStats,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub command: Command,
    pub config_path: Option<String>,
    pub journal_path: Option<String>,
}

pub const USAGE: &str = "usage: courier [--config FILE] [--journal FILE] <command>\n\ncommands:\n  send METHOD URL        send one request through the loopback server\n  config show           print the effective configuration\n  config check          validate the configuration and list warnings\n  journal tail [N]      print the last N journal entries (default 10)\n  journal stats         summarise the journal\n  help                  print this text\n";

fn usage(message: &str) -> CourierError {
    CourierError::new(ErrorKind::Usage, message)
}

pub fn parse(argv: &[String]) -> Result<Parsed, CourierError> {
    let mut config_path = None;
    let mut journal_path = None;
    let mut rest: Vec<&str> = Vec::new();
    let mut it = argv.iter().map(String::as_str);
    while let Some(a) = it.next() {
        match a {
            "--config" => config_path = Some(it.next().ok_or_else(|| usage("--config needs a file"))?.to_string()),
            "--journal" => journal_path = Some(it.next().ok_or_else(|| usage("--journal needs a file"))?.to_string()),
            flag if flag.starts_with("--") => return Err(usage(&format!("unknown option `{flag}`"))),
            word => rest.push(word),
        }
    }
    let command = match rest.as_slice() {
        [] | ["help"] => Command::Help,
        ["send", method, url] => Command::Send {
            method: method.to_string(),
            url: url.to_string(),
        },
        ["config", "show"] => Command::ConfigShow,
        ["config", "check"] => Command::ConfigCheck,
        ["journal", "tail"] => Command::JournalTail { count: 10 },
        ["journal", "tail", n] => Command::JournalTail {
            count: n.parse().map_err(|_| usage("tail takes a number"))?,
        },
        ["journal", "stats"] => Command::JournalStats,
        other => return Err(usage(&format!("unrecognised command: {}", other.join(" ")))),
    };
    Ok(Parsed {
        command,
        config_path,
        journal_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(words: &[&str]) -> Result<Parsed, CourierError> {
        parse(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_arguments_is_help() {
        assert_eq!(p(&[]).unwrap().command, Command::Help);
    }

    #[test]
    fn send() {
        assert_eq!(
            p(&["send", "GET", "http://h/"]).unwrap().command,
            Command::Send { method: "GET".into(), url: "http://h/".into() }
        );
    }

    #[test]
    fn options_can_appear_anywhere() {
        let x = p(&["config", "--config", "c.conf", "show"]).unwrap();
        assert_eq!(x.command, Command::ConfigShow);
        assert_eq!(x.config_path.as_deref(), Some("c.conf"));
    }

    #[test]
    fn tail_count() {
        assert_eq!(p(&["journal", "tail", "3"]).unwrap().command, Command::JournalTail { count: 3 });
        assert!(p(&["journal", "tail", "x"]).is_err());
    }

    #[test]
    fn errors_are_usage_errors() {
        assert_eq!(p(&["frobnicate"]).unwrap_err().kind, ErrorKind::Usage);
        assert_eq!(p(&["--nope"]).unwrap_err().kind, ErrorKind::Usage);
        assert_eq!(p(&["--config"]).unwrap_err().kind, ErrorKind::Usage);
    }
}
