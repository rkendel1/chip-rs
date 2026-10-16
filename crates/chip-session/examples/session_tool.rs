//! Small driver used by the cross-implementation check (`docs/rebuild/tools/verify_rust_session_resume.py`
//! in rkendel1/fx): it lets the real Zig binary and this crate take turns on one session directory.
//!
//!   session_tool create <sessions_dir> <id>      # session_started payload JSON on stdin
//!   session_tool append <session_dir> <effort>   # appends preferences_changed {"effort": <effort>}
//!   session_tool read   <session_dir>            # prints a JSON summary of the committed log
//!   session_tool append-raw <session_dir> <kind> # payload JSON on stdin, written as given (negative controls)

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

use chip_session::{Kind, NewEvent, OsIds, SessionWriter, read_session};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("create") if args.len() == 4 => {
            let mut payload = String::new();
            std::io::stdin().read_to_string(&mut payload).unwrap();
            NewEvent::new(Kind::SessionStarted, now_ms(), payload.trim())
                .and_then(|first| {
                    SessionWriter::create(
                        Path::new(&args[2]),
                        &args[3],
                        first,
                        Box::new(OsIds),
                        None,
                    )
                })
                .map(|w| {
                    format!(
                        "{{\"created\":\"{}\",\"through_seq\":{}}}",
                        w.session_id(),
                        w.position().through_seq
                    )
                })
        }
        Some("append") if args.len() == 4 => NewEvent::new(
            Kind::PreferencesChanged,
            now_ms(),
            &format!("{{\"effort\":\"{}\"}}", args[3]),
        )
        .and_then(|event| {
            SessionWriter::open(Path::new(&args[2]), Box::new(OsIds), None)
                .and_then(|mut w| w.append(vec![event]))
        })
        .map(|p| {
            format!(
                "{{\"through_seq\":{},\"through_event_log_bytes\":{}}}",
                p.through_seq, p.through_event_log_bytes
            )
        }),
        Some("append-raw") if args.len() == 4 => {
            let kind = match args[3].as_str() {
                "history_turn_committed" => Kind::HistoryTurnCommitted,
                "preferences_changed" => Kind::PreferencesChanged,
                "state_replacement_started" => Kind::StateReplacementStarted,
                "recovery_checkpoint_set" => Kind::RecoveryCheckpointSet,
                other => {
                    eprintln!("unsupported kind for append-raw: {other}");
                    return ExitCode::from(2);
                }
            };
            let mut payload = String::new();
            std::io::stdin().read_to_string(&mut payload).unwrap();
            NewEvent::new(kind, now_ms(), payload.trim())
                .and_then(|event| {
                    SessionWriter::open(Path::new(&args[2]), Box::new(OsIds), None)
                        .and_then(|mut w| w.append(vec![event]))
                })
                .map(|p| format!("{{\"through_seq\":{}}}", p.through_seq))
        }
        Some("read") if args.len() == 3 => read_session(Path::new(&args[2])).map(|log| {
            let kinds: Vec<String> = log
                .frames
                .iter()
                .map(|f| format!("\"{}:{:?}\"", f.seq, f.kind))
                .collect();
            format!(
                "{{\"through_seq\":{},\"frames\":[{}],\"uncommitted_tail_bytes\":{}}}",
                log.position.through_seq,
                kinds.join(","),
                log.uncommitted_tail_bytes
            )
        }),
        _ => {
            eprintln!(
                "usage: session_tool create <sessions_dir> <id> | append <session_dir> <effort> | read <session_dir>"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
