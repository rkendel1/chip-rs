//! The turn-event contract: Rust decodes the history Zig wrote, re-encodes it to the same bytes, and
//! refuses what it does not interpret.

use std::fs;
use std::path::{Path, PathBuf};

use chip_session::{
    ExecutionMemory, Frame, HistoryTurn, HistoryTurnCommitted, Kind, SessionError, TerminalReason,
    ToolStatus, UserTurn, decode_frame, read_session, reduce_history,
};

fn fixture(scenario: &str) -> (tempfile::TempDir, PathBuf) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/zig")
        .join(scenario);
    let id = serde_json::from_slice::<serde_json::Value>(
        &fs::read(src.join("PROVENANCE.json")).unwrap(),
    )
    .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(id);
    fs::create_dir(&dir).unwrap();
    for e in fs::read_dir(&src).unwrap() {
        let e = e.unwrap();
        if e.file_name() != "PROVENANCE.json" {
            fs::copy(e.path(), dir.join(e.file_name())).unwrap();
        }
    }
    (temp, dir)
}

fn frames_of(scenario: &str) -> Vec<Frame> {
    let (_t, dir) = fixture(scenario);
    read_session(&dir).unwrap().frames
}

#[test]
fn zig_history_decodes_into_meaningful_turns() {
    let history = reduce_history(&frames_of("tool_round_trip")).unwrap();
    assert_eq!(history.turns.len(), 1);
    assert!(!history.pending_recovery, "a commit closes the active turn");
    assert_eq!(history.preferences.model, "zai/glm-5.2");
    let commit = &history.turns[0].commit;
    assert_eq!(
        (commit.total_input_tokens, commit.total_output_tokens),
        (12, 3)
    );
    match history.turns[0].turn() {
        HistoryTurn::Assistant {
            user,
            assistant,
            execution,
        } => {
            assert_eq!(user.text, "list the files");
            assert_eq!(assistant, "done after the tool");
            assert_eq!(execution.tool_steps.len(), 1);
            let step = &execution.tool_steps[0];
            assert_eq!(step.tool_calls[0].name, "list_files");
            assert_eq!(step.tool_calls[0].arguments_json, r#"{"path":"."}"#);
            assert_eq!(step.tool_results[0].status, ToolStatus::Success);
            assert_eq!(step.tool_results[0].output, ".:\n- note.txt\n");
            assert_eq!(execution.files[0].tool_call_id, "call_1");
        }
        other => panic!("expected an assistant turn, got {other:?}"),
    }
}

#[test]
fn a_resumed_session_reduces_to_two_ordered_turns() {
    let history = reduce_history(&frames_of("resumed_two_turns")).unwrap();
    assert_eq!(
        history
            .turns
            .iter()
            .map(|t| t.turn().user().text.as_str())
            .collect::<Vec<_>>(),
        ["first question", "second question"]
    );
    assert!(history.turns[0].seq < history.turns[1].seq);
    assert!(
        history.total_input_tokens >= history.turns[0].commit.total_input_tokens,
        "totals are cumulative"
    );
}

#[test]
fn every_committed_turn_zig_wrote_re_encodes_to_the_same_bytes() {
    for scenario in ["single_text", "tool_round_trip", "resumed_two_turns"] {
        let mut seen = 0;
        for frame in frames_of(scenario) {
            if frame.kind != Kind::HistoryTurnCommitted {
                continue;
            }
            let commit = HistoryTurnCommitted::from_payload_json(frame.payload.get()).unwrap();
            assert_eq!(
                commit.to_payload_json().unwrap(),
                frame.payload.get(),
                "{scenario} seq {}",
                frame.seq
            );
            seen += 1;
        }
        assert!(seen > 0, "{scenario}: no committed turns to compare");
    }
}

fn payload(turn: &str) -> String {
    format!(
        r#"{{"conversation_language":"und-Latn","total_input_tokens":1,"total_output_tokens":2,"turn":{turn}}}"#
    )
}

const EXEC: &str = r#"{"schema_version":3,"tool_steps":[],"files":[]}"#;

fn assistant_turn(extra: &str) -> String {
    format!(
        r#"{{"kind":"assistant","user":{{"text":"hi","images":[]}},"assistant":"yo","execution":{EXEC}{extra}}}"#
    )
}

#[test]
fn the_rust_encoding_of_a_plain_turn_is_what_zig_would_accept() {
    let commit = HistoryTurnCommitted {
        conversation_language: "und-Latn".into(),
        total_input_tokens: 1,
        total_output_tokens: 2,
        work_id: None,
        turn: HistoryTurn::Assistant {
            user: UserTurn::text("hi"),
            assistant: "yo".into(),
            execution: ExecutionMemory::new(vec![], vec![]),
        },
    };
    assert_eq!(
        commit.to_payload_json().unwrap(),
        payload(&assistant_turn(""))
    );
}

#[test]
fn an_interrupted_turn_round_trips_with_its_terminal_reason() {
    let commit = HistoryTurnCommitted {
        conversation_language: "und-Latn".into(),
        total_input_tokens: 0,
        total_output_tokens: 0,
        work_id: None,
        turn: HistoryTurn::Interrupted {
            user: UserTurn::text("go"),
            assistant: None,
            tool_call: None,
            completed_tool_names: vec!["compute.selftest".into()],
            execution: ExecutionMemory::new(vec![], vec![]),
            terminal_reason: TerminalReason::Failed,
        },
    };
    let json = commit.to_payload_json().unwrap();
    assert!(json.contains(r#""terminal_reason":"failed""#));
    assert_eq!(
        HistoryTurnCommitted::from_payload_json(&json).unwrap(),
        commit
    );
}

#[test]
fn malformed_turns_are_refused() {
    let cases = [
        ("an extra key", assistant_turn(r#","extra":1"#)),
        ("a missing assistant", format!(r#"{{"kind":"assistant","user":{{"text":"hi","images":[]}},"execution":{EXEC}}}"#)),
        ("a missing user.images", format!(r#"{{"kind":"assistant","user":{{"text":"hi"}},"assistant":"yo","execution":{EXEC}}}"#)),
        ("an unknown kind", r#"{"kind":"mystery"}"#.to_string()),
        ("no kind", r#"{"user":{"text":"hi","images":[]}}"#.to_string()),
        ("an execution without files", r#"{"kind":"assistant","user":{"text":"hi","images":[]},"assistant":"yo","execution":{"schema_version":3,"tool_steps":[]}}"#.to_string()),
        ("a tool call without provider_result", r#"{"kind":"assistant","user":{"text":"hi","images":[]},"assistant":"yo","execution":{"schema_version":3,"tool_steps":[{"assistant":null,"tool_calls":[{"id":"1","name":"n","arguments_json":"{}"}],"tool_results":[]}],"files":[]}}"#.to_string()),
        ("a bad tool status", r#"{"kind":"assistant","user":{"text":"hi","images":[]},"assistant":"yo","execution":{"schema_version":3,"tool_steps":[{"assistant":null,"tool_calls":[],"tool_results":[{"tool_call_id":"1","tool_name":"n","status":"maybe","output":"","output_handle":null,"preview":null,"output_bytes":0,"stored_output_bytes":0,"truncated":false,"provider_native":false,"created_at_ms":1,"permission_feedback":[],"committed_file_presentation":null,"command_output_replay":null,"command_process_presentation":null}]}],"files":[]}}"#.to_string()),
    ];
    for (what, turn) in cases {
        let err = HistoryTurnCommitted::from_payload_json(&payload(&turn)).unwrap_err();
        assert!(
            matches!(err, SessionError::InvalidFormat { .. }),
            "{what}: {err}"
        );
    }
    let err = HistoryTurnCommitted::from_payload_json(
        r#"{"conversation_language":"und-Latn","total_input_tokens":1,"turn":{}}"#,
    )
    .unwrap_err();
    assert!(matches!(err, SessionError::InvalidFormat { .. }), "{err}");
}

#[test]
fn well_formed_history_rust_does_not_interpret_is_unsupported_not_skipped() {
    let cases = [
        r#"{"kind":"compacted_summary","summary":"s","removed_turn_count":1,"compaction_count":1}"#.to_string(),
        r#"{"kind":"background_command","user":{"text":"x","images":[]},"log_path":"l","expect_url":false,"url":null,"background_record_id":null}"#.to_string(),
        r#"{"kind":"assistant","user":{"text":"hi","images":[]},"assistant":"yo","execution":{"schema_version":2,"tool_steps":[],"files":[]}}"#.to_string(),
        r#"{"kind":"assistant","user":{"text":"hi","images":[]},"assistant":"yo","execution":{"schema_version":1,"tool_steps":[],"files":[]}}"#.to_string(),
    ];
    for turn in cases {
        let err = HistoryTurnCommitted::from_payload_json(&payload(&turn)).unwrap_err();
        assert!(matches!(err, SessionError::Unsupported(_)), "{turn}: {err}");
    }
}

fn frame(seq: u64, kind: &str, payload: &str) -> Frame {
    let line = format!(
        r#"{{"schema_version":1,"log_generation":"00000000000000000000000000000001","seq":{seq},"event_id":"{seq:032x}","timestamp_ms":{seq},"kind":"{kind}","payload":{payload}}}"#
    ) + "\n";
    decode_frame(line.as_bytes(), 0).unwrap()
}

const STARTED: &str = r#"{"id":"s","created_at_ms":1,"origin_workspace_root":"/w","workspace_root":"/w","conversation_language":"und-Latn","preferences":{"model":"m","effort":"auto","fast_mode":false}}"#;

#[test]
fn state_replacement_and_duplicate_starts_are_refused_by_the_reducer() {
    for kind in [
        "state_replacement_started",
        "state_replacement_chunk",
        "state_replacement_committed",
    ] {
        let err = reduce_history(&[frame(1, "session_started", STARTED), frame(2, kind, "{}")])
            .unwrap_err();
        assert!(matches!(err, SessionError::Unsupported(_)), "{kind}: {err}");
    }
    let err = reduce_history(&[
        frame(1, "session_started", STARTED),
        frame(2, "session_started", STARTED),
    ])
    .unwrap_err();
    assert!(matches!(err, SessionError::InvalidFormat { .. }), "{err}");
    let err = reduce_history(&[frame(1, "usage_checkpointed", r#"{"usage":{}}"#)]).unwrap_err();
    assert!(matches!(err, SessionError::InvalidFormat { .. }), "{err}");
}

#[test]
fn a_recovery_checkpoint_is_pending_until_a_commit_or_a_clear() {
    let checkpoint = frame(
        2,
        "recovery_checkpoint_set",
        r#"{"checkpoint":{"version":1}}"#,
    );
    let history = reduce_history(&[frame(1, "session_started", STARTED), checkpoint]).unwrap();
    assert!(history.pending_recovery);

    let cleared = reduce_history(&[
        frame(1, "session_started", STARTED),
        frame(
            2,
            "recovery_checkpoint_set",
            r#"{"checkpoint":{"version":1}}"#,
        ),
        frame(3, "recovery_checkpoint_cleared", "{}"),
    ])
    .unwrap();
    assert!(!cleared.pending_recovery);

    let committed = reduce_history(&[
        frame(1, "session_started", STARTED),
        frame(
            2,
            "recovery_checkpoint_set",
            r#"{"checkpoint":{"version":1}}"#,
        ),
        frame(3, "history_turn_committed", &payload(&assistant_turn(""))),
    ])
    .unwrap();
    assert!(!committed.pending_recovery);
    assert_eq!(committed.turns.len(), 1);
}

#[test]
fn preferences_and_workspace_changes_are_applied_in_order() {
    let history = reduce_history(&[
        frame(1, "session_started", STARTED),
        frame(2, "preferences_changed", r#"{"effort":"high"}"#),
        frame(
            3,
            "preferences_changed",
            r#"{"model":"n","fast_mode":true}"#,
        ),
        frame(
            4,
            "workspace_rebound",
            r#"{"previous_workspace_root":"/w","workspace_root":"/x"}"#,
        ),
    ])
    .unwrap();
    assert_eq!(history.preferences.effort, "high");
    assert_eq!(history.preferences.model, "n");
    assert!(history.preferences.fast_mode);
    assert_eq!(history.workspace_root, "/x");
    assert_eq!(history.origin_workspace_root, "/w");
    assert_eq!(history.last_seq, 4);
    let err = reduce_history(&[
        frame(1, "session_started", STARTED),
        frame(2, "preferences_changed", "{}"),
    ])
    .unwrap_err();
    assert!(matches!(err, SessionError::InvalidFormat { .. }), "{err}");
}
