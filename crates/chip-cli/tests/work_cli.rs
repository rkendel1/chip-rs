//! `chip --test-work` and `--test-real-work`: the autonomous loop through the real binary.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chip"))
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
        ("evidence-in-loop", "completed"),
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
        limit.contains("executions: 4 (executor calls observed: 4)") && limit.contains("turns: 5"),
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
        evidence.contains("executions: 0 (executor calls observed: 0)")
            && evidence.contains("model_escalations: 0 (model calls observed: 0)"),
        "{evidence}"
    );
    assert!(
        !evidence.contains("ExecutionStarted"),
        "valid evidence meant nothing was executed"
    );

    let in_loop = text(&run(&["--test-work", "evidence-in-loop"]));
    assert!(
        in_loop.contains("executions: 1 (executor calls observed: 1)")
            && in_loop.contains("evidence_hits: 1"),
        "{in_loop}"
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
            .filter(|l| !l.contains("elapsed_time") && !l.contains("_latency:"))
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
    let out = Command::new(env!("CARGO_BIN_EXE_chip"))
        .arg("--test-real-work")
        .env("COMPUTE_BIN", "/nonexistent/compute")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let t = text(&out);
    assert!(t.contains("SKIPPED"), "{t}");
    assert!(!t.contains("WorkCompleted") && !t.contains("ExecutionCompleted"));
}

// ------------------------------------------------------------------------------------------
// PR27: measurement
// ------------------------------------------------------------------------------------------

const CANONICAL: [&str; 5] = ["completion", "failure", "evidence", "limit", "escalation"];
const KEYS: [&str; 16] = [
    "workload",
    "outcome",
    "turns",
    "executions",
    "observations",
    "evidence_hits",
    "local_decisions",
    "model_escalations",
    "context_bytes",
    "context_chars",
    "model_calls",
    "model_tokens",
    "model_latency_ms",
    "compute_latency_ms",
    "local_decision_latency_ms",
    "total_latency_ms",
];

fn json(args: &[&str]) -> serde_json::Value {
    let out = run(args);
    assert!(
        out.status.success(),
        "{args:?}: {}{}",
        text(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&text(&out))
        .unwrap_or_else(|e| panic!("{args:?} is not valid JSON ({e}): {}", text(&out)))
}

/// A measurement without its latencies, which are the only values allowed to differ between runs.
fn deterministic(mut v: serde_json::Value) -> serde_json::Value {
    let keys: Vec<String> = v
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.ends_with("_ms"))
        .cloned()
        .collect();
    for k in keys {
        v.as_object_mut().unwrap().remove(&k);
    }
    v
}

#[test]
fn every_canonical_workload_produces_a_valid_machine_readable_measurement() {
    for name in CANONICAL {
        let v = json(&["--test-work", name, "--json"]);
        let object = v.as_object().unwrap();
        assert_eq!(
            object
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            KEYS.iter().map(|k| k.to_string()).collect(),
            "{name}"
        );
        assert_eq!(v["workload"], name);
        for k in [
            "turns",
            "executions",
            "observations",
            "evidence_hits",
            "local_decisions",
            "model_escalations",
            "context_bytes",
            "context_chars",
            "model_calls",
        ] {
            assert!(v[k].is_u64(), "{name}.{k}");
        }
        for k in [
            "model_latency_ms",
            "compute_latency_ms",
            "local_decision_latency_ms",
            "total_latency_ms",
        ] {
            assert!(v[k].as_f64().unwrap() >= 0.0, "{name}.{k}");
        }
        assert!(
            v["total_latency_ms"].as_f64() >= v["compute_latency_ms"].as_f64(),
            "{name}"
        );
        assert_eq!(
            v["model_calls"], v["model_escalations"],
            "{name}: one call per escalation"
        );
        assert!(
            v["observations"].as_u64() <= v["executions"].as_u64(),
            "{name}"
        );
        // No tokens were reported unless the model was called.
        assert_eq!(v["model_tokens"].is_null(), v["model_calls"] == 0, "{name}");
    }
}

#[test]
fn the_canonical_workloads_measure_what_they_are_defined_to() {
    let get = |n: &str| json(&["--test-work", n, "--json"]);
    let (c, f, e, l, x) = (
        get("completion"),
        get("failure"),
        get("evidence"),
        get("limit"),
        get("escalation"),
    );
    assert!(c["executions"].as_u64().unwrap() > 0 && c["outcome"] == "completed");
    assert_eq!(
        (f["executions"].as_u64(), f["outcome"].as_str()),
        (Some(1), Some("blocked")),
        "no retry"
    );
    assert_eq!(
        (
            e["executions"].as_u64(),
            e["evidence_hits"].as_u64(),
            e["outcome"].as_str()
        ),
        (Some(0), Some(1), Some("completed"))
    );
    assert_eq!(
        (
            l["outcome"].as_str(),
            l["turns"].as_u64(),
            l["executions"].as_u64()
        ),
        (Some("limit_reached"), Some(5), Some(4))
    );
    assert_eq!(
        (x["model_escalations"].as_u64(), x["model_calls"].as_u64()),
        (Some(1), Some(1))
    );
    assert!(x["context_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        x["model_tokens"], 2,
        "the stand-in reports 1 + 1 tokens per call"
    );
}

#[test]
fn the_json_carries_nothing_it_should_not() {
    for name in CANONICAL {
        let raw = text(&run(&["--test-work", name, "--json"]));
        for forbidden in [
            "Goal:",
            "Question:",
            "prompt",
            "sha256:",
            "receipt",
            "demo-",
            "sk-",
            "API",
            "key",
            "env",
            "202",
        ] {
            assert!(
                !raw.contains(forbidden),
                "{name}: {raw} contains {forbidden:?}"
            );
        }
        assert_eq!(raw.lines().count(), 1, "one line per measurement");
    }
}

#[test]
fn measurements_are_deterministic_apart_from_latency() {
    for name in CANONICAL {
        let a = deterministic(json(&["--test-work", name, "--json"]));
        let b = deterministic(json(&["--test-work", name, "--json"]));
        assert_eq!(a, b, "{name}");
    }
    let a: Vec<_> = json(&["--benchmark-work", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(deterministic)
        .collect();
    let b: Vec<_> = json(&["--benchmark-work", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(deterministic)
        .collect();
    assert_eq!(a, b);
}

#[test]
fn the_benchmark_suite_emits_a_stable_table_and_a_json_array() {
    let out = run(&["--benchmark-work"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let t = text(&out);
    let header = t.lines().next().unwrap();
    for column in [
        "Workload",
        "Turns",
        "Execs",
        "Local",
        "Model",
        "Evidence",
        "Context",
        "Outcome",
        "Model ms",
        "Compute ms",
        "Total ms",
    ] {
        assert!(header.contains(column), "{column} missing from {header}");
    }
    for (name, outcome) in [
        ("completion", "completed"),
        ("failure", "blocked"),
        ("evidence", "completed"),
        ("limit", "limit_reached"),
        ("escalation", "completed"),
    ] {
        let row = t
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no row for {name}:\n{t}"));
        assert!(row.contains(outcome), "{row}");
    }
    assert!(t.contains("satisfied the trajectory invariants"));

    let array = json(&["--benchmark-work", "--json"]);
    let names: Vec<_> = array
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["workload"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, CANONICAL);
}

#[test]
fn the_trace_shows_structure_and_no_prompt_text() {
    let t = text(&run(&["--test-work", "escalation", "--trace"]));
    for needle in [
        "TURN 1",
        "decision: RequestCapability",
        "capability: compute.selftest",
        "source: model",
        "execution: yes",
        "receipt: present",
        "observation: present",
        "context:",
        "observations: 0",
        "evidence: 0",
        "decisions: 0",
        "ruled_out: 0",
        "bytes: ",
        "model: called",
        "TURN 2",
        "decision: Complete",
        "source: local",
        "OUTCOME\n  completed",
    ] {
        assert!(t.contains(needle), "missing {needle:?} in\n{t}");
    }
    assert!(
        !t.contains("Goal:") && !t.contains("Question:") && !t.contains("Perform the self test"),
        "{t}"
    );

    let evidence = text(&run(&["--test-work", "evidence", "--trace"]));
    assert!(
        evidence.contains("execution: no (evidence reused)")
            && evidence.contains("receipt: present"),
        "{evidence}"
    );
    let limit = text(&run(&["--test-work", "limit", "--trace"]));
    assert!(
        limit.contains("OUTCOME\n  limit_reached (executions)"),
        "{limit}"
    );
}

#[test]
fn real_compute_work_skips_cleanly_in_json_mode_too() {
    let out = Command::new(env!("CARGO_BIN_EXE_chip"))
        .args(["--test-real-work", "--json"])
        .env("COMPUTE_BIN", "/nonexistent/compute")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(text(&out).contains("SKIPPED"));
}

#[test]
fn an_escalation_reports_its_context_policy_and_what_was_sent() {
    let out = run(&["--test-work", "escalation"]);
    assert!(out.status.success());
    let t = text(&out);
    let block = t
        .split("Escalation context\n")
        .nth(1)
        .expect("an escalation context block");
    for line in [
        "  policy: full-v1",
        "  bytes: ",
        "  chars: ",
        "  observations: ",
        "  decisions: ",
        "  evidence: ",
        "  ruled-out: ",
    ] {
        assert!(block.contains(line), "missing `{line}` in:\n{block}");
    }
    // A workload that never escalates has no such block.
    assert!(!text(&run(&["--test-work", "completion"])).contains("Escalation context"));
}
