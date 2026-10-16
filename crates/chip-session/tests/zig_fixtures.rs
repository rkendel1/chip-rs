//! Compatibility oracle: sessions written by the real Zig `fx` binary
//! (`tests/fixtures/zig/*`, produced by `docs/rebuild/tools/capture_zig_sessions.py` in rkendel1/fx).

use std::fs;
use std::path::{Path, PathBuf};

use chip_session::{
    CheckpointStatus, Kind, ManifestStatus, SessionError, decode_frame, encode_frame, read_session,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/zig")
}

/// The fixture directory is named by scenario; the session id is the directory name Zig used.
/// Copy it under its real id so the directory name matches `authority.json`.
fn materialize(scenario: &str) -> (tempfile::TempDir, PathBuf) {
    let src = fixtures().join(scenario);
    let provenance: serde_json::Value =
        serde_json::from_slice(&fs::read(src.join("PROVENANCE.json")).unwrap()).unwrap();
    let id = provenance["session_id"].as_str().unwrap().to_string();
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(id);
    fs::create_dir(&dir).unwrap();
    for entry in fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "PROVENANCE.json" {
            fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        }
    }
    (temp, dir)
}

const TORN: &[u8] = b"{\"schema_version\":1,\"torn";

const SCENARIOS: [(&str, usize); 3] = [
    ("single_text", 7),
    ("tool_round_trip", 12),
    ("resumed_two_turns", 13),
];

#[test]
fn every_zig_written_session_reads_and_verifies() {
    for (scenario, events) in SCENARIOS {
        let (_keep, dir) = materialize(scenario);
        let log = read_session(&dir).unwrap_or_else(|e| panic!("{scenario}: {e}"));
        assert_eq!(log.frames.len(), events, "{scenario}");
        assert_eq!(log.frames[0].kind, Kind::SessionStarted);
        assert_eq!(log.frames[0].seq, 1);
        assert_eq!(log.position.through_seq as usize, events);
        assert_eq!(log.uncommitted_tail_bytes, 0);
        assert_eq!(log.manifest, ManifestStatus::Current, "{scenario}");
        assert_eq!(log.checkpoint, CheckpointStatus::Verified, "{scenario}");
        assert_eq!(log.frames.last().unwrap().kind, Kind::HistoryTurnCommitted);
    }
}

#[test]
fn the_second_turn_continues_the_same_log() {
    let (_keep, dir) = materialize("resumed_two_turns");
    let log = read_session(&dir).unwrap();
    let turns = log
        .frames
        .iter()
        .filter(|f| f.kind == Kind::HistoryTurnCommitted)
        .count();
    assert_eq!(turns, 2, "two committed turns in one generation");
    assert!(
        log.frames
            .iter()
            .all(|f| f.log_generation == log.position.log_generation)
    );
}

#[test]
fn frames_re_encode_to_exactly_the_bytes_zig_wrote() {
    for (scenario, _) in SCENARIOS {
        let (_keep, dir) = materialize(scenario);
        let bytes = fs::read(dir.join("events.jsonl")).unwrap();
        let log = read_session(&dir).unwrap();
        let mut rebuilt = Vec::new();
        for frame in &log.frames {
            rebuilt.extend(encode_frame(frame).unwrap());
        }
        assert_eq!(
            rebuilt, bytes,
            "{scenario}: Rust must write the same bytes Zig wrote"
        );
    }
}

#[test]
fn decode_then_encode_is_the_identity_for_each_line() {
    let (_keep, dir) = materialize("tool_round_trip");
    let bytes = fs::read(dir.join("events.jsonl")).unwrap();
    let mut offset = 0u64;
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        let frame = decode_frame(line, offset).unwrap();
        assert_eq!(encode_frame(&frame).unwrap(), line);
        offset += line.len() as u64;
    }
}

fn corrupt(
    scenario: &str,
    change: impl FnOnce(&Path),
) -> Result<chip_session::CommittedLog, SessionError> {
    let (_keep, dir) = materialize(scenario);
    change(&dir);
    read_session(&dir)
}

fn commit_file(dir: &Path) -> PathBuf {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("commit.")
        })
        .unwrap()
}

#[test]
fn an_uncommitted_tail_is_reported_and_never_returned_as_history() {
    let log = corrupt("single_text", |dir| {
        let mut bytes = fs::read(dir.join("events.jsonl")).unwrap();
        bytes.extend_from_slice(TORN);
        fs::write(dir.join("events.jsonl"), bytes).unwrap();
    })
    .unwrap();
    assert_eq!(log.frames.len(), 7);
    assert_eq!(log.uncommitted_tail_bytes, TORN.len() as u64);
}

#[test]
fn a_complete_frame_past_the_watermark_is_still_not_committed() {
    let (_keep, dir) = materialize("single_text");
    let committed = fs::read(dir.join("events.jsonl")).unwrap();
    let last = committed
        .split_inclusive(|&b| b == b'\n')
        .next_back()
        .unwrap()
        .to_vec();
    let mut bytes = committed.clone();
    bytes.extend_from_slice(&last);
    fs::write(dir.join("events.jsonl"), bytes).unwrap();
    let log = read_session(&dir).unwrap();
    assert_eq!(log.frames.len(), 7);
    assert_eq!(log.uncommitted_tail_bytes, last.len() as u64);
}

#[test]
fn truncating_inside_the_committed_range_is_refused() {
    let err = corrupt("single_text", |dir| {
        let bytes = fs::read(dir.join("events.jsonl")).unwrap();
        fs::write(dir.join("events.jsonl"), &bytes[..bytes.len() - 5]).unwrap();
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::WatermarkMismatch(_)), "{err}");
}

#[test]
fn flipping_a_committed_byte_is_refused() {
    let err = corrupt("tool_round_trip", |dir| {
        let mut bytes = fs::read(dir.join("events.jsonl")).unwrap();
        let at = bytes.iter().position(|&b| b == b':').unwrap();
        bytes[at] = b'!';
        fs::write(dir.join("events.jsonl"), bytes).unwrap();
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::InvalidFrame { .. }), "{err}");
}

#[test]
fn a_sequence_gap_is_refused() {
    let err = corrupt("single_text", |dir| {
        let text = fs::read_to_string(dir.join("events.jsonl")).unwrap();
        let edited = text.replacen("\"seq\":3,", "\"seq\":9,", 1);
        fs::write(dir.join("events.jsonl"), edited).unwrap();
    })
    .unwrap_err();
    assert!(
        matches!(
            err,
            SessionError::SequenceGap {
                expected: 3,
                found: 9,
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn an_unknown_key_or_kind_in_a_frame_is_refused() {
    for (needle, replacement) in [
        ("\"payload\":", "\"extra\":1,\"payload\":"),
        ("\"kind\":\"usage_checkpointed\"", "\"kind\":\"made_up\""),
    ] {
        let err = corrupt("single_text", |dir| {
            let text = fs::read_to_string(dir.join("events.jsonl")).unwrap();
            fs::write(
                dir.join("events.jsonl"),
                text.replacen(needle, replacement, 1),
            )
            .unwrap();
        })
        .unwrap_err();
        assert!(
            matches!(
                err,
                SessionError::InvalidFrame { .. } | SessionError::WatermarkMismatch(_)
            ),
            "{needle}: {err}"
        );
    }
}

#[test]
fn a_frame_from_another_generation_is_refused() {
    let err = corrupt("single_text", |dir| {
        let text = fs::read_to_string(dir.join("events.jsonl")).unwrap();
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        // Flip the first hex digit of the log_generation of frame 4 (same length, still valid hex).
        let marker = "\"log_generation\":\"";
        let at = lines[3].find(marker).unwrap() + marker.len();
        let digit = lines[3].as_bytes()[at];
        let replacement = if digit == b'a' { "b" } else { "a" };
        lines[3].replace_range(at..at + 1, replacement);
        fs::write(dir.join("events.jsonl"), lines.join("\n") + "\n").unwrap();
    })
    .unwrap_err();
    assert!(
        matches!(err, SessionError::GenerationMismatch { .. }),
        "{err}"
    );
}

#[test]
fn a_watermark_for_another_session_or_position_is_refused() {
    let err = corrupt("single_text", |dir| {
        let path = commit_file(dir);
        let text = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            text.replacen("\"through_seq\":7", "\"through_seq\":6", 1),
        )
        .unwrap();
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::WatermarkMismatch(_)), "{err}");

    let err = corrupt("single_text", |dir| {
        let path = commit_file(dir);
        let text = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            text.replacen("\"session_id\":\"", "\"session_id\":\"x", 1),
        )
        .unwrap();
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::WatermarkMismatch(_)), "{err}");
}

#[test]
fn a_missing_or_extra_key_in_authority_or_watermark_is_refused() {
    let err = corrupt("single_text", |dir| {
        fs::remove_file(dir.join("authority.json")).unwrap()
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::Missing(_)), "{err}");
    let err = corrupt("single_text", |dir| {
        let text = fs::read_to_string(dir.join("authority.json")).unwrap();
        fs::write(
            dir.join("authority.json"),
            text.replacen('}', ",\"extra\":1}", 1),
        )
        .unwrap();
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::InvalidFormat { .. }), "{err}");
    let err = corrupt("single_text", |dir| {
        fs::remove_file(commit_file(dir)).unwrap()
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::Missing(_)), "{err}");
}

#[test]
fn a_stale_or_missing_projection_is_reported_not_fatal() {
    let log = corrupt("single_text", |dir| {
        fs::remove_file(dir.join("session.json")).unwrap()
    })
    .unwrap();
    assert_eq!(log.manifest, ManifestStatus::Missing);
    let log = corrupt("single_text", |dir| {
        let text = fs::read_to_string(dir.join("session.json")).unwrap();
        fs::write(
            dir.join("session.json"),
            text.replacen("\"last_event_seq\":7", "\"last_event_seq\":6", 1),
        )
        .unwrap();
    })
    .unwrap();
    assert!(matches!(log.manifest, ManifestStatus::Stale { .. }));
    let log = corrupt("single_text", |dir| {
        fs::write(dir.join("checkpoint.json"), b"{}").unwrap()
    })
    .unwrap();
    assert_eq!(log.checkpoint, CheckpointStatus::DigestMismatch);
}

#[cfg(unix)]
#[test]
fn symlinks_and_hard_links_are_refused() {
    let err = corrupt("single_text", |dir| {
        fs::rename(dir.join("events.jsonl"), dir.join("real.jsonl")).unwrap();
        std::os::unix::fs::symlink(dir.join("real.jsonl"), dir.join("events.jsonl")).unwrap();
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::UnsafePath(_)), "{err}");
    let err = corrupt("single_text", |dir| {
        fs::hard_link(dir.join("events.jsonl"), dir.join("alias.jsonl")).unwrap()
    })
    .unwrap_err();
    assert!(matches!(err, SessionError::UnsafePath(_)), "{err}");
}

#[test]
fn unsafe_session_ids_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let spaced = temp.path().join("a b");
    fs::create_dir(&spaced).unwrap();
    assert!(matches!(
        read_session(&spaced).unwrap_err(),
        SessionError::InvalidSessionId
    ));
    assert!(matches!(
        read_session(&temp.path().join("..")).unwrap_err(),
        SessionError::InvalidSessionId
    ));
}
