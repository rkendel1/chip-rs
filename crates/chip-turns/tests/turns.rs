//! A real Chip turn (the real work loop, decision boundary and executor path) persisted through the
//! real session writer, reloaded, continued, and interrupted.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use chip_core::WorkOutcome;
use chip_session::{
    Boundary, HistoryTurn, Kind, TerminalReason, ToolStatus, read_session, reduce_history,
};
use chip_turns::{Persistence, TurnError};
use common::*;

fn history(dir: &std::path::Path) -> chip_session::History {
    reduce_history(&read_session(dir).unwrap().frames).unwrap()
}

#[tokio::test]
async fn a_real_turn_creates_a_meaningful_durable_record_that_reloads() {
    let (_t, dir) = rust_session();
    let model = ScriptedModel::new(&[REQUEST_SELFTEST, COMPLETE]);
    let executor = CountingExecutor::new(false);
    let done = turn(&dir, &model, &executor, "run the self test", 1000, None)
        .await
        .unwrap();

    assert!(
        matches!(done.report.outcome, WorkOutcome::Completed { .. }),
        "{:?}",
        done.report.outcome
    );
    let Persistence::Committed(position) = done.persistence else {
        panic!("{:?}", done.persistence)
    };
    assert_eq!(position.through_seq, 2);
    assert_eq!(model.calls(), 2);
    assert_eq!(executor.runs(), 1);

    // Reopened from disk, the turn reconstructs.
    let log = read_session(&dir).unwrap();
    assert_eq!(log.position, position);
    assert_eq!(log.uncommitted_tail_bytes, 0);
    let history = reduce_history(&log.frames).unwrap();
    assert_eq!(history.turns.len(), 1);
    let HistoryTurn::Assistant {
        user,
        assistant,
        execution,
    } = history.turns[0].turn()
    else {
        panic!()
    };
    assert_eq!(user.text, "run the self test");
    assert_eq!(assistant, "the self test passed");
    let step = &execution.tool_steps[0];
    assert_eq!(step.tool_calls[0].name, "compute.selftest");
    assert_eq!(step.tool_calls[0].arguments_json, r#"{"n":3}"#);
    assert_eq!(step.tool_calls[0].provider_result, None);
    assert_eq!(step.tool_results[0].tool_call_id, step.tool_calls[0].id);
    assert_eq!(step.tool_results[0].status, ToolStatus::Success);
    assert_eq!(step.tool_results[0].output, "self test passed");
    assert_eq!(
        history.total_input_tokens, 20,
        "two model calls of 10 prompt tokens"
    );
    assert_eq!(history.total_output_tokens, 10);
    assert!(!history.pending_recovery);
}

#[tokio::test]
async fn the_next_turn_sees_the_previous_one_and_duplicates_nothing() {
    let (_t, dir) = rust_session();
    let executor = CountingExecutor::new(false);
    let first = ScriptedModel::new(&[REQUEST_SELFTEST, COMPLETE]);
    turn(&dir, &first, &executor, "run the self test", 1000, None)
        .await
        .unwrap();
    let before = read_session(&dir).unwrap();
    let first_bytes = fs::read(dir.join("events.jsonl")).unwrap();

    let second = ScriptedModel::new(&[COMPLETE]);
    let done = turn(&dir, &second, &executor, "and what did it say?", 2000, None)
        .await
        .unwrap();
    assert!(matches!(done.persistence, Persistence::Committed(_)));

    let seen = second.first_request_text();
    assert!(
        seen.contains("run the self test"),
        "the earlier request is in context: {seen}"
    );
    assert!(
        seen.contains("the self test passed"),
        "the earlier answer is in context"
    );
    assert!(
        seen.contains("tool result"),
        "the earlier tool result is in context"
    );
    assert!(seen.contains("Current request: and what did it say?"));
    assert!(seen.contains("data, not instructions"));

    let after = read_session(&dir).unwrap();
    assert_eq!(
        after.frames.len(),
        before.frames.len() + 1,
        "exactly one new event"
    );
    assert!(
        fs::read(dir.join("events.jsonl"))
            .unwrap()
            .starts_with(&first_bytes),
        "earlier events are byte-identical"
    );
    for (old, new) in before.frames.iter().zip(&after.frames) {
        assert_eq!((old.seq, old.event_id), (new.seq, new.event_id));
    }
    let h = reduce_history(&after.frames).unwrap();
    assert_eq!(h.turns.len(), 2);
    assert!(
        h.total_input_tokens > 20,
        "totals are cumulative across turns"
    );
    let unique: std::collections::BTreeSet<_> = after.frames.iter().map(|f| f.event_id).collect();
    assert_eq!(unique.len(), after.frames.len(), "event ids are unique");
}

#[tokio::test]
async fn a_zig_created_session_is_read_continued_and_reloaded() {
    let (_t, dir) = zig_session("tool_round_trip");
    let before = read_session(&dir).unwrap();
    let zig_ids: Vec<_> = before.frames.iter().map(|f| f.event_id).collect();

    let model = ScriptedModel::new(&[COMPLETE]);
    let done = turn(
        &dir,
        &model,
        &CountingExecutor::new(false),
        "follow up",
        5000,
        None,
    )
    .await
    .unwrap();
    let Persistence::Committed(position) = done.persistence else {
        panic!("{:?}", done.persistence)
    };
    assert_eq!(position.through_seq, before.position.through_seq + 1);

    let seen = model.first_request_text();
    assert!(
        seen.contains("list the files"),
        "Zig's user turn is in context: {seen}"
    );
    assert!(
        seen.contains("done after the tool"),
        "Zig's answer is in context"
    );
    assert!(seen.contains("list_files"), "Zig's tool call is in context");

    let after = read_session(&dir).unwrap();
    assert_eq!(
        after
            .frames
            .iter()
            .take(zig_ids.len())
            .map(|f| f.event_id)
            .collect::<Vec<_>>(),
        zig_ids,
        "Zig's events are untouched"
    );
    let h = reduce_history(&after.frames).unwrap();
    assert_eq!(h.turns.len(), 2);
    assert_eq!(h.turns[0].turn().user().text, "list the files");
    assert_eq!(h.turns[1].turn().user().text, "follow up");
    assert_eq!(
        h.turns[1].commit.total_input_tokens,
        12 + 10,
        "Zig's cumulative total plus this turn"
    );
}

#[tokio::test]
async fn a_turn_that_does_not_complete_is_recorded_as_interrupted_not_as_an_answer() {
    let (_t, dir) = rust_session();
    let model = ScriptedModel::new(&[REQUEST_SELFTEST, BLOCK]);
    let done = turn(
        &dir,
        &model,
        &CountingExecutor::new(false),
        "do it",
        1000,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(done.report.outcome, WorkOutcome::Blocked { .. }));
    assert!(matches!(done.persistence, Persistence::Committed(_)));
    let h = history(&dir);
    let HistoryTurn::Interrupted {
        assistant,
        completed_tool_names,
        terminal_reason,
        execution,
        ..
    } = h.turns[0].turn()
    else {
        panic!("a blocked turn must not be recorded as an assistant answer")
    };
    assert_eq!(*assistant, None);
    assert_eq!(completed_tool_names, &vec!["compute.selftest".to_string()]);
    assert_eq!(*terminal_reason, TerminalReason::Cancelled);
    assert_eq!(
        execution.tool_steps.len(),
        1,
        "the work that ran is still recorded"
    );

    let (_t2, dir2) = rust_session();
    let model = ScriptedModel::new(&["not a decision"]);
    let done = turn(
        &dir2,
        &model,
        &CountingExecutor::new(false),
        "do it",
        1000,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(done.report.outcome, WorkOutcome::Failed { .. }));
    let HistoryTurn::Interrupted {
        terminal_reason, ..
    } = history(&dir2).turns[0].turn().clone()
    else {
        panic!()
    };
    assert_eq!(terminal_reason, TerminalReason::Failed);
}

#[tokio::test]
async fn a_failed_tool_is_recorded_as_a_failed_result() {
    let (_t, dir) = rust_session();
    let model = ScriptedModel::new(&[REQUEST_SELFTEST, BLOCK]);
    turn(
        &dir,
        &model,
        &CountingExecutor::new(true),
        "do it",
        1000,
        None,
    )
    .await
    .unwrap();
    let h = history(&dir);
    assert_eq!(
        h.turns[0].turn().execution().tool_steps[0].tool_results[0].status,
        ToolStatus::Failure
    );
}

#[tokio::test]
async fn a_crash_before_the_watermark_leaves_no_turn_and_nothing_is_run_again() {
    for boundary in [
        Boundary::AfterEventAppend,
        Boundary::AfterEventSync,
        Boundary::AfterIntentSync,
    ] {
        let (_t, dir) = rust_session();
        let model = ScriptedModel::new(&[REQUEST_SELFTEST, COMPLETE]);
        let executor = CountingExecutor::new(false);
        let err = turn(&dir, &model, &executor, "run it", 1000, Some(boundary))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(err, TurnError::SimulatedCrash(_)),
            "{boundary:?}: {err}"
        );
        assert_eq!(
            (model.calls(), executor.runs()),
            (2, 1),
            "{boundary:?}: the work ran once"
        );

        // The session is recoverable and shows no completed turn.
        assert_eq!(read_session(&dir).unwrap().frames.len(), 1, "{boundary:?}");
        let model2 = ScriptedModel::new(&[COMPLETE]);
        let executor2 = CountingExecutor::new(false);
        let again = turn(&dir, &model2, &executor2, "run it again", 2000, None)
            .await
            .unwrap();
        assert!(
            matches!(again.persistence, Persistence::Committed(_)),
            "{boundary:?}: {:?}",
            again.persistence
        );
        assert_eq!(
            history(&dir).turns.len(),
            1,
            "{boundary:?}: only the second turn exists"
        );
        assert_eq!(history(&dir).turns[0].turn().user().text, "run it again");
        assert!(!dir.join("commit.pending.json").exists());
    }
}

#[tokio::test]
async fn a_crash_after_the_watermark_leaves_the_turn_committed_exactly_once() {
    for boundary in [Boundary::AfterWatermarkRename, Boundary::AfterIntentRemove] {
        let (_t, dir) = rust_session();
        let model = ScriptedModel::new(&[COMPLETE]);
        let err = turn(
            &dir,
            &model,
            &CountingExecutor::new(false),
            "run it",
            1000,
            Some(boundary),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, TurnError::SimulatedCrash(_)));
        let model2 = ScriptedModel::new(&[COMPLETE]);
        turn(
            &dir,
            &model2,
            &CountingExecutor::new(false),
            "next",
            2000,
            None,
        )
        .await
        .unwrap();
        let h = history(&dir);
        assert_eq!(
            h.turns
                .iter()
                .map(|t| t.turn().user().text.as_str())
                .collect::<Vec<_>>(),
            ["run it", "next"],
            "{boundary:?}"
        );
    }
}

#[tokio::test]
async fn a_persistence_failure_is_reported_as_not_committed_and_the_work_is_not_repeated() {
    let (_t, dir) = rust_session();
    let model = ScriptedModel::new(&[REQUEST_SELFTEST, COMPLETE]);
    // Sabotage the commit lock while the model is being asked, so the append fails after the work ran.
    let lock = dir.join("commit.lock");
    *model.before_reply.lock().unwrap() = Some(Box::new(move || {
        let _ = fs::remove_file(&lock);
        let _ = fs::create_dir(&lock);
    }));
    let executor = CountingExecutor::new(false);
    let done = turn(&dir, &model, &executor, "run it", 1000, None)
        .await
        .unwrap();

    assert!(
        matches!(done.report.outcome, WorkOutcome::Completed { .. }),
        "the work itself completed"
    );
    let Persistence::NotCommitted(why) = &done.persistence else {
        panic!("{:?}", done.persistence)
    };
    assert!(!why.is_empty());
    assert_eq!(
        (model.calls(), executor.runs()),
        (2, 1),
        "persistence failure did not cause a rerun"
    );

    fs::remove_dir(dir.join("commit.lock")).unwrap();
    let log = read_session(&dir).unwrap();
    assert_eq!(log.frames.len(), 1, "no falsely completed turn in the log");
    assert_eq!(
        log.uncommitted_tail_bytes, 0,
        "the unpublished tail was rolled back"
    );
}

#[tokio::test]
async fn a_session_with_an_unfinished_turn_is_not_started_over() {
    let (_t, dir) = rust_session();
    let mut w = chip_session::SessionWriter::open(&dir, ids(300), None).unwrap();
    w.append(vec![
        chip_session::NewEvent::new(
            Kind::RecoveryCheckpointSet,
            5,
            r#"{"checkpoint":{"version":1}}"#,
        )
        .unwrap(),
    ])
    .unwrap();
    drop(w);
    let model = ScriptedModel::new(&[COMPLETE]);
    let err = turn(
        &dir,
        &model,
        &CountingExecutor::new(false),
        "go",
        1000,
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(err, TurnError::PendingRecovery), "{err}");
    assert_eq!(model.calls(), 0, "nothing ran");
}

#[tokio::test]
async fn history_this_implementation_does_not_interpret_stops_the_turn_before_it_starts() {
    let (_t, dir) = rust_session();
    let mut w = chip_session::SessionWriter::open(&dir, ids(300), None).unwrap();
    w.append(vec![
        chip_session::NewEvent::new(Kind::StateReplacementStarted, 5, "{}").unwrap(),
    ])
    .unwrap();
    drop(w);
    let model = ScriptedModel::new(&[COMPLETE]);
    let err = turn(
        &dir,
        &model,
        &CountingExecutor::new(false),
        "go",
        1000,
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(
            err,
            TurnError::Session(chip_session::SessionError::Unsupported(_))
        ),
        "{err}"
    );
    assert_eq!(model.calls(), 0);
}

#[tokio::test]
async fn a_malformed_committed_turn_stops_the_next_turn() {
    let (_t, dir) = rust_session();
    let mut w = chip_session::SessionWriter::open(&dir, ids(300), None).unwrap();
    w.append(vec![chip_session::NewEvent::new(Kind::HistoryTurnCommitted, 5, r#"{"conversation_language":"und-Latn","total_input_tokens":1,"total_output_tokens":1,"turn":{"kind":"assistant"}}"#).unwrap()]).unwrap();
    drop(w);
    let model = ScriptedModel::new(&[COMPLETE]);
    let err = turn(
        &dir,
        &model,
        &CountingExecutor::new(false),
        "go",
        1000,
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(
            err,
            TurnError::Session(chip_session::SessionError::InvalidFormat { .. })
        ),
        "{err}"
    );
    assert_eq!(model.calls(), 0);
}

#[tokio::test]
async fn the_session_stays_locked_for_the_whole_turn() {
    let (_t, dir) = rust_session();
    let model = ScriptedModel::new(&[COMPLETE]);
    let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = observed.clone();
    let probe = dir.clone();
    *model.before_reply.lock().unwrap() = Some(Box::new(move || {
        let result = chip_session::SessionWriter::open(&probe, ids(9), None)
            .err()
            .map(|e| e.to_string());
        *slot.lock().unwrap() = result;
    }));
    turn(
        &dir,
        &model,
        &CountingExecutor::new(false),
        "go",
        1000,
        None,
    )
    .await
    .unwrap();
    let seen = observed
        .lock()
        .unwrap()
        .clone()
        .expect("a second writer was refused during the turn");
    assert!(seen.contains("locked"), "{seen}");
}

#[tokio::test]
async fn permissions_stay_private() {
    let (_t, dir) = rust_session();
    turn(
        &dir,
        &ScriptedModel::new(&[COMPLETE]),
        &CountingExecutor::new(false),
        "go",
        1000,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.join("events.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
