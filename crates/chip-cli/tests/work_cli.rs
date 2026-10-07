//! `chip-cli --test-work` and `--test-real-work`: the autonomous loop through the real binary.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .args(args)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn the_default_workload_runs_the_whole_loop_and_prints_trajectory_and_summary() {
    let out = run(&["--test-work"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let t = text(&out);
    let order = [
        "WorkStarted",
        "DecisionStarted (turn 1)",
        "LocalDecision: request compute.selftest",
        "CapabilityRequested: compute.selftest",
        "ExecutionRequested",
        "ExecutionCompleted",
        "ObservationRecorded: execution.completed",
        "EvidenceRecorded",
        "DecisionStarted (turn 2)",
        "LocalDecision: complete",
        "WorkCompleted",
    ];
    let mut at = 0;
    for needle in order {
        at += t[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing or out of order in\n{t}"));
    }
    for needle in [
        "turns: 2",
        "executions: 1",
        "observations: 1",
        "evidence_hits: 0",
        "local_decisions: 2",
        "model_escalations: 0",
        "context_bytes: 0",
        "elapsed_time:",
        "terminal_state: completed",
        "Expected terminal state reached: completed",
    ] {
        assert!(t.contains(needle), "missing {needle:?} in\n{t}");
    }
    // The second decision came from the loop: one invocation, no manual turn.
    assert_eq!(t.matches("DecisionStarted").count(), 2);
}

#[test]
fn every_scenario_reaches_its_expected_terminal_state() {
    for (scenario, terminal) in [
        ("completion", "completed"),
        ("failure", "blocked"),
        ("limit", "limit_reached"),
        ("turn-limit", "limit_reached"),
        ("evidence", "completed"),
        ("escalation", "completed"),
    ] {
        let out = run(&["--test-work", scenario]);
        assert!(
            out.status.success(),
            "{scenario}: {}{}",
            text(&out),
            String::from_utf8_lossy(&out.stderr)
        );
        let t = text(&out);
        assert!(
            t.contains(&format!("terminal_state: {terminal}")),
            "{scenario}\n{t}"
        );
        assert!(t.contains("Expected terminal state reached"), "{scenario}");
    }
}

#[test]
fn the_failure_scenario_observes_the_real_failure() {
    let t = text(&run(&["--test-work", "failure"]));
    assert!(t.contains("ExecutionFailed"));
    assert!(t.contains("ObservationRecorded: execution.failed"));
    assert!(!t.contains("ExecutionCompleted"));
    assert!(t.contains("WorkBlocked: the self test failed"));
}

#[test]
fn the_limit_scenarios_stop_exactly_at_their_bounds() {
    let limit = text(&run(&["--test-work", "limit"]));
    assert!(
        limit.contains("executions: 3 (executor calls observed: 3)"),
        "{limit}"
    );
    assert!(limit.contains("WorkLimitReached: executions"));
    let turns = text(&run(&["--test-work", "turn-limit"]));
    assert!(
        turns.contains("turns: 5")
            && turns.contains("executions: 1")
            && turns.contains("evidence_hits: 4"),
        "{turns}"
    );
}

#[test]
fn evidence_and_escalation_are_visible_in_the_trajectory_and_the_summary() {
    let evidence = text(&run(&["--test-work", "evidence"]));
    assert!(evidence.contains("EvidenceReused: compute.selftest (receipt sha256:demo-receipt)"));
    assert!(
        evidence.contains("executions: 1 (executor calls observed: 1)")
            && evidence.contains("model_escalations: 0 (model calls observed: 0)")
    );

    let escalation = text(&run(&["--test-work", "escalation"]));
    assert!(escalation.contains("ModelEscalation:"));
    assert!(escalation.contains("model_escalations: 1 (model calls observed: 1)"));
    assert!(
        !escalation.contains("context_bytes: 0"),
        "an escalation sends context: {escalation}"
    );
}

#[test]
fn the_trajectory_is_deterministic() {
    let strip = |t: String| {
        t.lines()
            .filter(|l| !l.contains("elapsed_time"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for scenario in ["completion", "escalation", "evidence"] {
        assert_eq!(
            strip(text(&run(&["--test-work", scenario]))),
            strip(text(&run(&["--test-work", scenario]))),
            "{scenario}"
        );
    }
}

#[test]
fn an_unknown_scenario_is_a_usage_error() {
    let out = run(&["--test-work", "nonsense"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).is_empty());
}

#[test]
fn real_compute_work_is_skipped_not_faked_when_compute_is_unavailable() {
    let out = Command::new(env!("CARGO_BIN_EXE_chip-cli"))
        .arg("--test-real-work")
        .env("COMPUTE_BIN", "/nonexistent/compute")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let t = text(&out);
    assert!(t.contains("SKIPPED"), "{t}");
    assert!(!t.contains("WorkCompleted") && !t.contains("ExecutionCompleted"));
}
