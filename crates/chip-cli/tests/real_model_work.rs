//! PR28: a real model escalation inside the autonomous loop.
//!
//! The provider here is the real `HttpProvider` talking to a local mock server, so the whole
//! request/response path is exercised without a network or credentials. No model is called.
//! The one test against an actual provider is opt-in (`CHIP_TEST_REAL_MODEL=1`) and skips
//! otherwise.

#[path = "../../fx-provider-http/tests/common/mod.rs"]
mod common;

use std::process::{Command, Output};
use std::time::Duration;

const SECRET: &str = "sk-pr28-secret-never-printed";

fn completion(content: &str, usage: Option<(u32, u32)>) -> String {
    let mut body = serde_json::json!({
        "id": "resp-pr28",
        "choices": [{"message": {"role": "assistant", "content": content}}],
    });
    if let Some((p, c)) = usage {
        body["usage"] = serde_json::json!({"prompt_tokens": p, "completion_tokens": c});
    }
    body.to_string()
}

const REQUEST: &str = r#"{"decision":"request_capability","capability":"compute.op_a"}"#;
const EXPECTED_DIGEST: &str = "5dc3ef3419c119833659d7288d246c87db93e4eeabb2b11126be581cb83c165f";

fn run(url: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chip"))
        .arg("--test-real-model-work")
        .args(extra)
        .env("CHIP_PROVIDER", "openai-compatible")
        .env("CHIP_MODEL", "mock-model")
        .env("CHIP_ENDPOINT", url)
        .env("CHIP_API_KEY", SECRET)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

async fn run_async(url: String, extra: &'static [&'static str]) -> Output {
    tokio::task::spawn_blocking(move || run(&url, extra))
        .await
        .unwrap()
}

fn field(out: &str, name: &str) -> String {
    out.lines()
        .find_map(|l| l.trim_start().strip_prefix(name))
        .unwrap_or_else(|| panic!("no `{name}` in:\n{out}"))
        .trim()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_decision_drives_one_execution_and_the_loop_finishes_itself() {
    let server = common::start(
        200,
        &completion(REQUEST, Some((120, 17))),
        Duration::from_millis(30),
    )
    .await;
    let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
    let t = text(&out);
    assert!(out.status.success(), "{t}");

    assert_eq!(field(&t, "Outcome:"), "Completed");
    assert_eq!(field(&t, "Executions:"), "1");
    assert_eq!(field(&t, "Observations:"), "1");
    assert_eq!(field(&t, "Model escalations:"), "1");
    assert_eq!(field(&t, "Model calls:"), "1");
    assert_eq!(field(&t, "Model tokens:"), "137");
    // The model chose from the declared set; the answer is the executor's, not the model's.
    assert_eq!(
        field(&t, "offered:"),
        "compute.op_a, compute.op_b, compute.op_c"
    );
    assert_eq!(field(&t, "chosen:"), "compute.op_a");
    assert_eq!(field(&t, "observed output:"), EXPECTED_DIGEST);
    assert_eq!(
        field(&t, "digest from the Compute observation matches:"),
        "yes"
    );
    let latency: u64 = field(&t, "Model latency:")
        .trim_end_matches(" ms")
        .parse()
        .unwrap();
    assert!(latency >= 30, "{t}");

    // Exactly one provider request: no retry, repair or fallback call.
    let captured = server.captured.lock().await;
    assert_eq!(captured.len(), 1);

    // The measured context is the context actually sent.
    let body: serde_json::Value = serde_json::from_str(&captured[0].body).unwrap();
    let sent = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap().len())
        .sum::<usize>();
    let measured: usize = field(&t, "Context bytes:").parse().unwrap();
    assert!(measured > 0);
    assert!(
        sent >= measured && sent - measured < 2048,
        "measured {measured} vs sent {sent}"
    );
    assert!(!t.contains(SECRET));
    assert_eq!(
        captured[0].header("authorization"),
        Some(&*format!("Bearer {SECRET}"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unreported_usage_is_not_estimated() {
    let server = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
    let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert_eq!(field(&t, "Model tokens:"), "not reported");
    assert_eq!(field(&t, "Model calls:"), "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_failure_still_records_the_call_and_executes_nothing() {
    let server = common::start(401, "{}", Duration::ZERO).await;
    let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
    let t = text(&out);
    assert!(!out.status.success());
    assert!(t.contains("Failed"), "{t}");
    assert_eq!(field(&t, "Model calls:"), "1");
    assert_eq!(field(&t, "Executions:"), "0");
    assert!(!t.contains(SECRET));
    assert_eq!(server.captured.lock().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_decisions_fail_without_executing_or_retrying() {
    let replies = [
        "I think you should run the self test.",
        r#"{"decision":"delete_everything"}"#,
        r#"{"decision":"request_capability","capability":"shell.exec"}"#,
        // The obvious names are not declared: the ids are opaque, so guessing from the goal fails.
        r#"{"decision":"request_capability","capability":"compute.hash"}"#,
        r#"{"decision":"request_capability","capability":"compute.sha256"}"#,
        r#"{"decision":"request_capability","capability":"compute.selftest"}"#,
        r#"{"decision":"request_capability"}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","extra":1}"#,
        // The model claiming reality is not reality.
        "The operation succeeded.",
        r#"{"decision":"request_capability","capability":"compute.op_a","status":"success","receipt":"r-1"}"#,
        r#"{"schema":"chip.work-decision.v2","decision":"request_capability","capability":"compute.op_a"}"#,
        // A capability the model made up, and one that was never declared.
        r#"{"decision":"request_capability","capability":"compute.fake"}"#,
        r#"{"decision":"request_capability","capability":"shell.exec"}"#,
        // The model supplying what only Chip may: an id, a command, an executable.
        r#"{"decision":"request_capability","capability":"compute.op_a","execution_id":"mine"}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","command":"echo hi"}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","executable":"/bin/sh"}"#,
        // A completion alongside a request, and a completion claim after the request.
        r#"{"decision":"request_capability","capability":"compute.op_a","summary":"done"}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a"}{"decision":"complete","summary":"done"}"#,
        // A digest asserted in prose is not a digest.
        "The SHA-256 digest is 5dc3ef3419c119833659d7288d246c87db93e4eeabb2b11126be581cb83c165f.",
    ];
    for reply in replies {
        let server = common::start(200, &completion(reply, Some((10, 2))), Duration::ZERO).await;
        let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
        let t = text(&out);
        assert!(!out.status.success(), "{reply}: {t}");
        assert_eq!(field(&t, "Model calls:"), "1", "{reply}");
        assert_eq!(field(&t, "Executions:"), "0", "{reply}");
        assert_eq!(field(&t, "Observations:"), "0", "{reply}");
        assert_eq!(field(&t, "Receipt:"), "none", "{reply}");
        assert!(!t.contains("EvidenceRecorded"), "{reply}");
        assert_eq!(server.captured.lock().await.len(), 1, "{reply}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declared_but_wrong_capability_runs_yet_does_not_answer_the_goal() {
    // The choice is the model's, and a wrong one is observable: op_b and op_c are declared, so
    // Chip runs them, but their output is not the digest and the scenario does not pass.
    for wrong in ["compute.op_b", "compute.op_c"] {
        let reply = format!(r#"{{"decision":"request_capability","capability":"{wrong}"}}"#);
        let server = common::start(200, &completion(&reply, Some((10, 2))), Duration::ZERO).await;
        let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
        let t = text(&out);
        assert!(!out.status.success(), "{wrong}: {t}");
        assert_eq!(field(&t, "Executions:"), "1", "{wrong}");
        assert_eq!(field(&t, "chosen:"), wrong);
        assert_eq!(
            field(&t, "digest from the Compute observation matches:"),
            "no",
            "{wrong}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_is_sent_the_opaque_capabilities_and_what_each_does() {
    let server = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
    let out = run_async(server.url.clone(), &["--deterministic-executor"]).await;
    assert!(out.status.success(), "{}", text(&out));
    let captured = server.captured.lock().await;
    let body: serde_json::Value = serde_json::from_str(&captured[0].body).unwrap();
    let sent: String = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(sent.contains("Goal: Determine the SHA-256 digest of the fixed test input."));
    for line in [
        "compute.op_a - Produce a SHA-256 digest of the fixed test input.",
        "compute.op_b - Report deterministic information about the Compute runtime.",
        "compute.op_c - Run the existing Compute self-test and report its result.",
    ] {
        assert!(sent.contains(line), "{line} not in:\n{sent}");
    }
    // Only the three opaque capabilities; no id that names its purpose, no mechanism, no answer.
    for absent in [
        "compute.selftest",
        "compute.hash",
        "compute.system_info",
        "hashlib",
        "python",
        EXPECTED_DIGEST,
    ] {
        assert!(!sent.contains(absent), "the context contains {absent}");
    }
}

// ---- PR35: the goal is worded without the capability's vocabulary -----------------------------

const GOALS: [&str; 3] = [
    "Produce the canonical fingerprint of the test payload.",
    "Calculate the deterministic identity value for the supplied test data.",
    "Return the standard cryptographic representation of the fixed payload.",
];

/// A deterministic-executor run of one (goal, permutation) cell against a mock model.
async fn run_cell(url: &str, goal: usize, run_number: usize) -> Output {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let (goal, number) = (goal.to_string(), run_number.to_string());
        run(
            &url,
            &[
                "--deterministic-executor",
                "--permutation-seed",
                SEED,
                "--permutation-run",
                &number,
                "--semantic-goal",
                &goal,
            ],
        )
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn each_goal_wording_keeps_the_permutation_and_the_answer() {
    for run_number in 1..=6 {
        let mut answers = Vec::new();
        let mut presented = Vec::new();
        for goal in 1..=3 {
            // Probe for the capability that answers this cell, then choose it.
            let probe = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
            let out = run_cell(&probe.url, goal, run_number).await;
            let t = text(&out);
            let hash_id = field(&t, "hash capability:");
            assert_eq!(field(&t, "goal variant:"), ["A", "B", "C"][goal - 1], "{t}");
            // The model is asked the reworded goal, under the same prompt.
            let body: serde_json::Value =
                serde_json::from_str(&probe.captured.lock().await[0].body).unwrap();
            let sent: String = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["content"].as_str().unwrap())
                .collect();
            assert!(
                sent.contains(&format!("Goal: {}", GOALS[goal - 1])),
                "{sent}"
            );
            let lower = GOALS[goal - 1].to_lowercase();
            for word in ["sha", "digest", "hash"] {
                assert!(!lower.contains(word));
            }

            let reply = format!(r#"{{"decision":"request_capability","capability":"{hash_id}"}}"#);
            let right = common::start(200, &completion(&reply, None), Duration::ZERO).await;
            let out = run_cell(&right.url, goal, run_number).await;
            let t = text(&out);
            assert!(out.status.success(), "goal {goal} run {run_number}: {t}");
            assert_eq!(field(&t, "chosen:"), hash_id);
            assert_eq!(field(&t, "observed output:"), EXPECTED_DIGEST);
            answers.push(hash_id);
            presented.push(field(&t, "offered:"));
        }
        // Only the wording changed: the same mapping, the same presentation, the same answer.
        answers.dedup();
        presented.dedup();
        assert_eq!(answers.len(), 1, "run {run_number}: {answers:?}");
        assert_eq!(presented.len(), 1, "run {run_number}: {presented:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_but_declared_choice_executes_and_the_goal_stays_unmet() {
    for goal in 1..=3 {
        for run_number in 1..=6 {
            let probe = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
            let hash_id = field(
                &text(&run_cell(&probe.url, goal, run_number).await),
                "hash capability:",
            );
            // A declared capability that is not the digest.
            let wrong = ["compute.op_a", "compute.op_b", "compute.op_c"]
                .into_iter()
                .find(|id| *id != hash_id)
                .unwrap();
            let reply = format!(r#"{{"decision":"request_capability","capability":"{wrong}"}}"#);
            let server = common::start(200, &completion(&reply, None), Duration::ZERO).await;
            let out = run_cell(&server.url, goal, run_number).await;
            let t = text(&out);
            let cell = format!("goal {goal} run {run_number} chose {wrong}");

            // Valid, so it is not rejected: it executes and is observed...
            assert_eq!(field(&t, "Executions:"), "1", "{cell}");
            assert_eq!(field(&t, "Observations:"), "1", "{cell}");
            assert!(t.contains("ExecutionStarted"), "{cell}");
            assert!(t.contains("ObservationRecorded"), "{cell}");
            assert_ne!(field(&t, "Receipt:"), "none", "{cell}");
            assert_eq!(field(&t, "chosen:"), wrong, "{cell}");
            // ...and reality shows the goal was not met. The experiment fails; nothing hides it.
            assert_ne!(field(&t, "observed output:"), EXPECTED_DIGEST, "{cell}");
            assert_eq!(
                field(&t, "digest from the Compute observation matches:"),
                "no",
                "{cell}"
            );
            assert!(!out.status.success(), "{cell}: {t}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_goal_variant_is_refused_before_any_model_call() {
    for bad in ["0", "4", "x"] {
        let server = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
        let url = server.url.clone();
        let out = tokio::task::spawn_blocking(move || {
            run(&url, &["--deterministic-executor", "--semantic-goal", bad])
        })
        .await
        .unwrap();
        assert_eq!(out.status.code(), Some(2), "{bad}: {}", text(&out));
        assert!(
            server.captured.lock().await.is_empty(),
            "{bad}: no model call"
        );
    }
}

// ---- PR36: selecting a capability is not inventing how to invoke it ---------------------------

fn request_with(capability: &str, extra: &str) -> String {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(",{extra}")
    };
    format!(
        r#"{{"schema":"chip.work-decision.v1","decision":"request_capability","capability":"{capability}"{extra}}}"#
    )
}

/// Runs one cell (goal 2, dealing `run_number`) against a mock model that replies `reply`.
async fn run_reply(reply: &str, run_number: usize) -> (Output, String) {
    let server = common::start(200, &completion(reply, Some((9, 3))), Duration::ZERO).await;
    let out = run_cell(&server.url, 2, run_number).await;
    let t = text(&out);
    assert_eq!(server.captured.lock().await.len(), 1, "{reply}: no retry");
    (out, t)
}

fn assert_nothing_ran(t: &str, why: &str) {
    assert_eq!(field(t, "Model calls:"), "1", "{why}");
    assert_eq!(field(t, "Executions:"), "0", "{why}");
    assert_eq!(field(t, "Observations:"), "0", "{why}");
    assert_eq!(field(t, "Receipt:"), "none", "{why}");
    for event in [
        "ExecutionRequested",
        "ExecutionStarted",
        "ObservationRecorded",
        "EvidenceRecorded",
    ] {
        assert!(!t.contains(event), "{why}: {event} in\n{t}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_profile_tells_the_four_outcomes_apart_in_every_dealing() {
    for run_number in 1..=6 {
        let probe = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
        let hash_id = field(
            &text(&run_cell(&probe.url, 2, run_number).await),
            "hash capability:",
        );
        let wrong = ["compute.op_a", "compute.op_b", "compute.op_c"]
            .into_iter()
            .find(|id| *id != hash_id)
            .unwrap();
        let cell = |what: &str| format!("run {run_number}: {what}");

        // A: right capability, valid invocation, executed, goal met.
        let (out, t) = run_reply(&request_with(&hash_id, ""), run_number).await;
        assert!(out.status.success(), "{t}");
        assert_eq!(field(&t, "category:"), "A", "{}", cell("A"));
        assert!(field(&t, "selection:").starts_with("valid"));
        assert!(field(&t, "invocation:").starts_with("valid"));

        // B: right capability, but the model invented inputs. Chip refuses; nothing executes.
        for inputs in [
            r#""inputs":{"test_data":"supplied"}"#,
            r#""inputs":{"command":"sha256sum /etc/passwd"}"#,
            r#""inputs":{"executable":"/bin/sh"}"#,
            // Even an empty `inputs` member: how the capability is invoked is not the model's to say.
            r#""inputs":{}"#,
        ] {
            let (out, t) = run_reply(&request_with(&hash_id, inputs), run_number).await;
            assert!(!out.status.success(), "{inputs}: {t}");
            assert_eq!(field(&t, "category:"), "B", "{}", cell(inputs));
            assert!(field(&t, "selection:").starts_with("valid"), "{inputs}");
            assert!(field(&t, "invocation:").starts_with("rejected"), "{inputs}");
            assert_eq!(field(&t, "chosen:"), hash_id);
            assert!(t.contains("CapabilityRequested"), "{inputs}");
            assert!(
                t.contains("WorkBlocked: invalid capability input"),
                "{inputs}"
            );
            assert_nothing_ran(&t, inputs);
        }

        // C: a wrong but declared capability runs, and reality shows the goal unmet.
        let (out, t) = run_reply(&request_with(wrong, ""), run_number).await;
        assert!(!out.status.success(), "{t}");
        assert_eq!(field(&t, "category:"), "C", "{}", cell("C"));
        assert_eq!(field(&t, "Executions:"), "1");
        assert_eq!(field(&t, "Observations:"), "1");
        assert!(field(&t, "invocation:").starts_with("valid"));

        // D: an undeclared capability is never selected, so nothing is invoked.
        for undeclared in ["compute.op_fake", "shell.exec", "compute.hash"] {
            let (out, t) = run_reply(&request_with(undeclared, ""), run_number).await;
            assert!(!out.status.success(), "{t}");
            assert_eq!(field(&t, "category:"), "D", "{}", cell(undeclared));
            assert!(
                field(&t, "selection:").starts_with("rejected"),
                "{undeclared}"
            );
            assert_eq!(field(&t, "invocation:"), "not reached");
            assert!(!t.contains("CapabilityRequested"), "{undeclared}");
            assert_nothing_ran(&t, undeclared);
        }

        // F: wrong capability and invented inputs: refused at invocation, nothing runs.
        let (_, t) = run_reply(
            &request_with(wrong, r#""inputs":{"test_data":"supplied"}"#),
            run_number,
        )
        .await;
        assert_eq!(field(&t, "category:"), "F", "{}", cell("F"));
        assert_nothing_ran(&t, "wrong capability with inputs");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replies_that_try_to_redefine_the_invocation_never_reach_compute() {
    // Rejected before a capability is selected: the reply is not a usable decision.
    let unusable = [
        request_with("compute.op_a", r#""execution_id":"mine""#),
        request_with("compute.op_a", r#""receipt":"sha256:forged""#),
        request_with("compute.op_a", r#""status":"success""#),
        request_with("compute.op_a", r#""result":"the digest is abc""#),
        request_with("compute.op_a", r#""observation":"it worked""#),
        request_with("compute.op_a", r#""command":"sha256sum x""#),
        "I would run compute.op_a.".to_string(),
    ];
    for reply in unusable {
        for run_number in [1, 4] {
            let (out, t) = run_reply(&reply, run_number).await;
            assert!(!out.status.success(), "{reply}: {t}");
            assert_eq!(field(&t, "category:"), "E", "{reply}");
            assert!(!t.contains("CapabilityRequested"), "{reply}");
            assert_nothing_ran(&t, &reply);
        }
    }
}

// ---- PR37: zero-overlap goals, balanced dealing ---------------------------------------------

/// One (goal, dealing) cell of the PR37 matrix against a mock model that replies `reply`.
async fn run_zero(url: &str, goal: usize, run_number: usize) -> Output {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let (goal, number) = (goal.to_string(), run_number.to_string());
        run(
            &url,
            &[
                "--deterministic-executor",
                "--balanced-dealing",
                "--permutation-seed",
                SEED,
                "--permutation-run",
                &number,
                "--zero-overlap-goal",
                &goal,
            ],
        )
    })
    .await
    .unwrap()
}

async fn zero_reply(reply: &str, goal: usize, run_number: usize) -> (Output, String) {
    let server = common::start(200, &completion(reply, Some((9, 3))), Duration::ZERO).await;
    let out = run_zero(&server.url, goal, run_number).await;
    let t = text(&out);
    assert_eq!(server.captured.lock().await.len(), 1, "{reply}: no retry");
    (out, t)
}

fn sent_text(captured_body: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(captured_body).unwrap();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_goal_and_dealing_reaches_the_model_and_the_answer_follows_the_dealing() {
    let mut digest_positions = Vec::new();
    for run_number in 1..=6 {
        let mut answers = Vec::new();
        let mut presented = Vec::new();
        for goal in 1..=6 {
            let probe = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
            let t = text(&run_zero(&probe.url, goal, run_number).await);
            assert_eq!(field(&t, "goal variant:"), format!("Z{goal}"), "{t}");
            let hash_id = field(&t, "hash capability:");

            // What the model reads: the goal, the opaque ids with their descriptions, nothing else.
            let sent = sent_text(&probe.captured.lock().await[0].body);
            assert!(sent.starts_with("Goal: "), "{sent}");
            let goal_line = sent.lines().next().unwrap().trim_start_matches("Goal: ");
            assert!(goal_line.ends_with('.') && goal_line.split_whitespace().count() >= 10);
            for hidden in [
                "compute.hash",
                "compute.selftest",
                "compute.system_info",
                "hashlib",
                "python",
                EXPECTED_DIGEST,
                "op_a = ",
                "assignment",
            ] {
                assert!(
                    !sent.contains(hidden),
                    "goal {goal} run {run_number}: leaks {hidden}"
                );
            }

            // Answer correctly: it runs and the goal is met.
            let reply = request_with(&hash_id, "");
            let right = common::start(200, &completion(&reply, None), Duration::ZERO).await;
            let out = run_zero(&right.url, goal, run_number).await;
            let t = text(&out);
            assert!(out.status.success(), "goal {goal} run {run_number}: {t}");
            assert_eq!(field(&t, "chosen:"), hash_id);
            assert_eq!(field(&t, "observed output:"), EXPECTED_DIGEST);
            assert_eq!(field(&t, "category:"), "A");
            assert_eq!(field(&t, "evidence recorded:"), "1");
            answers.push(hash_id);
            presented.push(field(&t, "offered:"));
        }
        // The dealing does not depend on the goal: only the wording changes between goals.
        answers.dedup();
        presented.dedup();
        assert_eq!(answers.len(), 1, "run {run_number}: {answers:?}");
        assert_eq!(presented.len(), 1, "run {run_number}: {presented:?}");
        let order: Vec<&str> = presented[0].split(", ").collect();
        digest_positions.push(order.iter().position(|id| *id == answers[0]).unwrap());
    }
    // Balanced: the digest sits in each presented position exactly twice in six runs.
    digest_positions.sort();
    assert_eq!(digest_positions, [0, 0, 1, 1, 2, 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_categories_are_read_from_the_trajectory_for_every_kind_of_reply() {
    for (goal, run_number) in [(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)] {
        let probe = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
        let hash_id = field(
            &text(&run_zero(&probe.url, goal, run_number).await),
            "hash capability:",
        );
        let wrong = ["compute.op_a", "compute.op_b", "compute.op_c"]
            .into_iter()
            .find(|id| *id != hash_id)
            .unwrap();
        let why = |what: &str| format!("goal {goal} run {run_number}: {what}");

        let (out, t) = zero_reply(&request_with(&hash_id, ""), goal, run_number).await;
        assert!(out.status.success(), "{t}");
        assert_eq!(field(&t, "category:"), "A", "{}", why("A"));
        assert_eq!(field(&t, "evidence recorded:"), "1");

        // B: the right capability with an invented invocation. Nothing runs, nothing is recorded.
        for inputs in [
            r#""inputs":{}"#,
            r#""inputs":{"text":"the stored sample"}"#,
            r#""inputs":{"command":"sha256sum x"}"#,
        ] {
            let (out, t) = zero_reply(&request_with(&hash_id, inputs), goal, run_number).await;
            assert!(!out.status.success(), "{inputs}");
            assert_eq!(field(&t, "category:"), "B", "{}", why(inputs));
            assert_eq!(field(&t, "evidence recorded:"), "0", "{inputs}");
            assert_nothing_ran(&t, inputs);
        }

        // C: a wrong but declared capability runs; reality shows the goal unmet; evidence exists
        // for what ran and none of it is the digest.
        let (out, t) = zero_reply(&request_with(wrong, ""), goal, run_number).await;
        assert!(!out.status.success(), "{t}");
        assert_eq!(field(&t, "category:"), "C", "{}", why("C"));
        assert_eq!(field(&t, "evidence recorded:"), "1");
        assert_ne!(field(&t, "observed output:"), EXPECTED_DIGEST);

        // D: an undeclared capability.
        let (_, t) = zero_reply(&request_with("compute.op_fake", ""), goal, run_number).await;
        assert_eq!(field(&t, "category:"), "D", "{}", why("D"));
        assert_nothing_ran(&t, "D");

        // E: replies that cannot be read as a decision, or that carry what only the runtime owns.
        for reply in [
            "I would pick the first one.".to_string(),
            request_with(&hash_id, r#""execution_id":"mine""#),
            request_with(&hash_id, r#""receipt":"sha256:forged""#),
            request_with(&hash_id, r#""status":"success""#),
            request_with(&hash_id, r#""result":"abc""#),
            request_with(&hash_id, r#""observation":"it worked""#),
            request_with(&hash_id, r#""evidence":"recorded""#),
            request_with(&hash_id, r#""executable":"/bin/sh""#),
        ] {
            let (_, t) = zero_reply(&reply, goal, run_number).await;
            assert_eq!(field(&t, "category:"), "E", "{}", why(&reply));
            assert_nothing_ran(&t, &reply);
            assert_eq!(field(&t, "evidence recorded:"), "0", "{reply}");
        }

        // F: the wrong capability and an invented invocation.
        let (_, t) = zero_reply(
            &request_with(wrong, r#""inputs":{"text":"x"}"#),
            goal,
            run_number,
        )
        .await;
        assert_eq!(field(&t, "category:"), "F", "{}", why("F"));
        assert_nothing_ran(&t, "F");

        // G: a valid decision that selects no capability. Whatever it claims, nothing ran and
        // nothing was recorded; a completion is the model's words, not the goal being met.
        for reply in [
            r#"{"decision":"complete","summary":"The code is 5dc3ef3419c119833659d7288d246c87db93e4eeabb2b11126be581cb83c165f"}"#,
            r#"{"decision":"escalate","reason":"I cannot tell which capability applies"}"#,
            r#"{"decision":"block","reason":"none of the capabilities fits"}"#,
        ] {
            let (out, t) = zero_reply(reply, goal, run_number).await;
            assert!(!out.status.success(), "{reply}: a claim is not a met goal");
            assert_eq!(field(&t, "category:"), "G", "{}", why(reply));
            assert_nothing_ran(&t, reply);
            assert_eq!(field(&t, "evidence recorded:"), "0", "{reply}");
            assert_eq!(
                field(&t, "digest from the Compute observation matches:"),
                "no",
                "{reply}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_pr37_flags_are_refused_before_any_model_call() {
    let cases: [&[&str]; 5] = [
        &["--zero-overlap-goal", "0"],
        &["--zero-overlap-goal", "7"],
        &["--zero-overlap-goal", "x"],
        &["--zero-overlap-goal", "1", "--semantic-goal", "1"],
        &["--balanced-dealing"],
    ];
    for flags in cases {
        let server = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
        let url = server.url.clone();
        let args: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
        let out = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = std::iter::once("--deterministic-executor")
                .chain(args.iter().map(String::as_str))
                .collect();
            run(&url, &refs)
        })
        .await
        .unwrap();
        assert_eq!(out.status.code(), Some(2), "{flags:?}: {}", text(&out));
        assert!(
            server.captured.lock().await.is_empty(),
            "{flags:?}: no model call"
        );
    }
}

// ---- PR38: a wrong judgment becomes a real failure, is detected from reality, and is recovered ----
//
// The model here is a mock HTTP provider. Compute is REAL: these tests run the real `compute`
// binary and say SKIPPED if it is not installed. They never pass `--deterministic-executor`.

/// A run of the recovery experiment against a mock model that replies `reply` to every call.
async fn run_recovery(
    reply: &str,
    run_number: usize,
    extra: &'static [&'static str],
) -> Option<(Output, String)> {
    let server = common::start(200, &completion(reply, Some((9, 3))), Duration::ZERO).await;
    let url = server.url.clone();
    let out = tokio::task::spawn_blocking(move || {
        let number = run_number.to_string();
        let mut args: Vec<&str> = vec![
            "--balanced-dealing",
            "--permutation-seed",
            SEED,
            "--permutation-run",
            &number,
        ];
        args.extend_from_slice(extra);
        run(&url, &args)
    })
    .await
    .unwrap();
    let t = text(&out);
    if out.status.code() == Some(3) {
        eprintln!("SKIPPED: {}", t.trim());
        return None;
    }
    Some((out, t))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recovery_modes_refuse_a_fake_executor() {
    for flags in [
        &["--deterministic-executor", "--require-output"][..],
        &["--deterministic-executor", "--force-wrong-first"][..],
    ] {
        let server = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
        let url = server.url.clone();
        let args: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
        let out = tokio::task::spawn_blocking(move || {
            run(&url, &args.iter().map(String::as_str).collect::<Vec<_>>())
        })
        .await
        .unwrap();
        assert_eq!(out.status.code(), Some(2), "{flags:?}: {}", text(&out));
        assert!(server.captured.lock().await.is_empty(), "no model call");
    }
    for flags in [
        &["--wrong-role", "self-test"][..],
        &["--force-wrong-first", "--wrong-role", "x"][..],
    ] {
        let server = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
        let url = server.url.clone();
        let args: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
        let out = tokio::task::spawn_blocking(move || {
            run(&url, &args.iter().map(String::as_str).collect::<Vec<_>>())
        })
        .await
        .unwrap();
        assert_eq!(out.status.code(), Some(2), "{flags:?}: {}", text(&out));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_forced_decision_the_correct_first_choice_completes_with_one_execution() {
    for run_number in 1..=3 {
        // Learn which id carries the digest in this dealing, then answer with it.
        let Some((_, t)) = run_recovery(REQUEST, run_number, &["--require-output"]).await else {
            return;
        };
        let hash_id = field(&t, "hash capability:");
        let reply = request_with(&hash_id, "");
        let Some((out, t)) = run_recovery(&reply, run_number, &["--require-output"]).await else {
            return;
        };
        assert!(out.status.success(), "{t}");
        assert_eq!(field(&t, "Model calls:"), "1");
        assert_eq!(field(&t, "Executions:"), "1");
        assert_eq!(field(&t, "Observations:"), "1");
        assert_eq!(field(&t, "evidence recorded:"), "1");
        assert_eq!(field(&t, "goal evaluations:"), "satisfied");
        assert_eq!(field(&t, "unauthorized completion:"), "no");
        assert!(
            field(&t, "Receipt:").starts_with("sha256:") && !t.contains("demo-receipt"),
            "{t}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forced_wrong_first_decision_executes_for_real_and_is_recovered_from_reality() {
    for (run_number, role) in [
        (1, "runtime-info"),
        (2, "self-test"),
        (3, "runtime-info"),
        (4, "self-test"),
    ] {
        let Some((_, t)) = run_recovery(REQUEST, run_number, &["--require-output"]).await else {
            return;
        };
        let hash_id = field(&t, "hash capability:");
        let reply = request_with(&hash_id, "");
        let flags: &'static [&'static str] = match role {
            "runtime-info" => &["--force-wrong-first", "--wrong-role", "runtime-info"],
            _ => &["--force-wrong-first", "--wrong-role", "self-test"],
        };
        let Some((out, t)) = run_recovery(&reply, run_number, flags).await else {
            return;
        };
        let why = format!("run {run_number} {role}");
        assert!(out.status.success(), "{why}: {t}");
        assert_eq!(field(&t, "Outcome:"), "Completed", "{why}");
        // Two model calls, two real executions, two real observations, two pieces of evidence.
        assert_eq!(field(&t, "Model calls:"), "2", "{why}");
        assert_eq!(field(&t, "Turns:"), "3", "{why}");
        assert_eq!(field(&t, "Executions:"), "2", "{why}");
        assert_eq!(field(&t, "Observations:"), "2", "{why}");
        assert_eq!(field(&t, "evidence recorded:"), "2", "{why}");
        // The wrong execution did not satisfy the goal; the right one did; Chip completed only then.
        assert_eq!(
            field(&t, "goal evaluations:"),
            "not satisfied, satisfied",
            "{why}"
        );
        assert_eq!(field(&t, "recovery:"), "recovered", "{why}");
        assert_eq!(field(&t, "unauthorized completion:"), "no", "{why}");
        assert!(
            field(&t, "first decision:").starts_with("forced wrong"),
            "{why}"
        );
        assert_eq!(field(&t, "observed output:"), EXPECTED_DIGEST, "{why}");
        // Real Compute: a real receipt, never the stand-in's.
        assert!(
            field(&t, "Receipt:").starts_with("sha256:") && !t.contains("demo-receipt"),
            "{why}"
        );
        let at = |needle: &str| {
            t.rfind(needle)
                .unwrap_or_else(|| panic!("{why}: no `{needle}`"))
        };
        let first_eval = t
            .find("GoalEvaluated: the observation does not satisfy the goal")
            .unwrap();
        assert!(
            first_eval < at("WorkCompleted"),
            "{why}: completed before the goal was satisfied"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completion_claim_after_a_real_wrong_execution_is_refused() {
    let claim = r#"{"decision":"complete","summary":"Done: the digest is 5dc3ef3419c119833659d7288d246c87db93e4eeabb2b11126be581cb83c165f"}"#;
    for (run_number, flags) in [
        (
            1,
            &["--force-wrong-first", "--wrong-role", "runtime-info"][..],
        ),
        (2, &["--force-wrong-first", "--wrong-role", "self-test"][..]),
    ] {
        let flags: &'static [&'static str] = Box::leak(
            flags
                .iter()
                .map(|f| *f)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let Some((out, t)) = run_recovery(claim, run_number, flags).await else {
            return;
        };
        assert!(!out.status.success(), "{t}");
        assert_eq!(
            field(&t, "Executions:"),
            "1",
            "the wrong execution ran for real"
        );
        assert_eq!(field(&t, "Observations:"), "1");
        assert_eq!(field(&t, "evidence recorded:"), "1");
        assert_eq!(field(&t, "goal evaluations:"), "not satisfied");
        assert!(
            field(&t, "Outcome:").starts_with("Blocked (completion refused"),
            "{t}"
        );
        assert_eq!(
            field(&t, "recovery:"),
            "not recovered (the model claimed completion; Chip refused it)"
        );
        assert_eq!(field(&t, "unauthorized completion:"), "no");
        assert!(!t.contains("WorkCompleted"), "{t}");
        assert_ne!(
            field(&t, "observed output:"),
            EXPECTED_DIGEST,
            "the model's claim is not an observation"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn choosing_the_wrong_capability_again_never_completes_and_stays_bounded() {
    // A model that keeps asking for the capability that already ran: its evidence is reused, the
    // goal is never satisfied, and the work ends at a limit. The mock replies with the runtime-info
    // id whatever the dealing, so learn it from a probe first.
    for run_number in [1, 2, 3] {
        let Some((_, t)) = run_recovery(REQUEST, run_number, &["--require-output"]).await else {
            return;
        };
        let _ = t;
        // Which id carries runtime-info in this dealing is printed on the assignment line.
        let Some((_, t)) = run_recovery(
            REQUEST,
            run_number,
            &["--force-wrong-first", "--wrong-role", "runtime-info"],
        )
        .await
        else {
            return;
        };
        let assignment = field(&t, "assignment:");
        let runtime_id = assignment
            .split_whitespace()
            .find_map(|w| w.strip_prefix("runtime-info="))
            .unwrap()
            .to_string();
        let reply = request_with(&runtime_id, "");
        let Some((out, t)) = run_recovery(
            &reply,
            run_number,
            &["--force-wrong-first", "--wrong-role", "runtime-info"],
        )
        .await
        else {
            return;
        };
        assert!(!out.status.success(), "{t}");
        assert_ne!(field(&t, "Outcome:"), "Completed");
        assert_eq!(
            field(&t, "Executions:"),
            "1",
            "the repeat reuses evidence, it does not re-run"
        );
        assert_eq!(field(&t, "unauthorized completion:"), "no");
        assert!(!field(&t, "recovery:").starts_with("recovered"), "{t}");
        assert!(
            !t.contains("goal evaluations: satisfied") && !t.contains(", satisfied"),
            "{t}"
        );
    }
}

// ---- PR34: the same loop, powered by an Ollama server instead --------------------------------

fn ollama_reply(content: &str, usage: Option<(u32, u32)>) -> String {
    let mut body = serde_json::json!({
        "model": "mock-ollama-model",
        "created_at": "2025-10-07T10:11:12.123456Z",
        "message": {"role": "assistant", "content": content},
        "done": true,
    });
    if let Some((p, c)) = usage {
        body["prompt_eval_count"] = p.into();
        body["eval_count"] = c.into();
    }
    body.to_string()
}

fn run_ollama(base_url: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chip"))
        .arg("--test-real-model-work")
        .args(extra)
        .env("CHIP_PROVIDER", "ollama")
        .env("CHIP_MODEL", "mock-ollama-model")
        .env("CHIP_OLLAMA_ENDPOINT", base_url)
        .env_remove("CHIP_ENDPOINT")
        .env_remove("CHIP_API_KEY")
        .output()
        .unwrap()
}

async fn run_ollama_async(url: String, extra: &'static [&'static str]) -> Output {
    let base = url.split("/v1/").next().unwrap().to_string();
    tokio::task::spawn_blocking(move || run_ollama(&base, extra))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ollama_decision_drives_the_same_loop_and_changes_nothing_else() {
    let server = common::start(200, &ollama_reply(REQUEST, Some((229, 48))), Duration::ZERO).await;
    let out = run_ollama_async(server.url.clone(), &["--deterministic-executor"]).await;
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert_eq!(field(&t, "Outcome:"), "Completed");
    assert_eq!(field(&t, "Model calls:"), "1");
    assert_eq!(field(&t, "Executions:"), "1");
    assert_eq!(field(&t, "Observations:"), "1");
    assert_eq!(field(&t, "Model tokens:"), "277");
    assert_eq!(field(&t, "chosen:"), "compute.op_a");
    assert_eq!(field(&t, "observed output:"), EXPECTED_DIGEST);

    // No key, no bearer token; the chat endpoint.
    let captured = server.captured.lock().await;
    assert_eq!(captured.len(), 1);
    assert!(captured[0].request_line.starts_with("POST /api/chat "));
    assert!(captured[0].header("authorization").is_none());

    // Same escalation prompt as the OpenAI-compatible path: the provider changes the wire format,
    // not what the model is asked, so the context size is identical.
    let other = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
    let other_out = run_async(other.url.clone(), &["--deterministic-executor"]).await;
    assert_eq!(
        field(&t, "Context bytes:"),
        field(&text(&other_out), "Context bytes:")
    );
    let ollama_body: serde_json::Value = serde_json::from_str(&captured[0].body).unwrap();
    let openai_body: serde_json::Value =
        serde_json::from_str(&other.captured.lock().await[0].body).unwrap();
    assert_eq!(
        ollama_body["messages"], openai_body["messages"],
        "identical messages"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ollama_replies_that_break_the_contract_execute_nothing() {
    let replies = [
        // The required case: a capability the model made up, and one that was never declared.
        r#"{"decision":"request_capability","capability":"compute.fake"}"#,
        r#"{"decision":"request_capability","capability":"shell.exec"}"#,
        r#"{"decision":"request_capability","capability":"compute.hash"}"#,
        "I would run compute.op_a to get the digest.",
        "The SHA-256 digest is 5dc3ef3419c119833659d7288d246c87db93e4eeabb2b11126be581cb83c165f.",
        r#"{"decision":"request_capability","capability":"compute.op_a","command":"sha256sum x"}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","execution_id":"mine"}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a","status":"success","receipt":"r-1"}"#,
        r#"{"schema":"chip.work-decision.v2","decision":"request_capability","capability":"compute.op_a"}"#,
        r#"{"decision":"request_capability","capability":"compute.op_a"}{"decision":"complete","summary":"done"}"#,
    ];
    for reply in replies {
        let server = common::start(200, &ollama_reply(reply, Some((10, 2))), Duration::ZERO).await;
        let out = run_ollama_async(server.url.clone(), &["--deterministic-executor"]).await;
        let t = text(&out);
        assert!(!out.status.success(), "{reply}: {t}");
        assert_eq!(field(&t, "Model calls:"), "1", "{reply}");
        assert_eq!(field(&t, "Executions:"), "0", "{reply}");
        assert_eq!(field(&t, "Observations:"), "0", "{reply}");
        assert_eq!(field(&t, "Receipt:"), "none", "{reply}");
        assert!(!t.contains("EvidenceRecorded"), "{reply}");
        assert!(!t.contains("ExecutionStarted"), "{reply}");
        assert_eq!(server.captured.lock().await.len(), 1, "{reply}: no retry");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ollama_failures_execute_nothing_and_never_fall_back() {
    // A model that is not pulled, and a server that is not there.
    let server = common::start(
        404,
        r#"{"error":"model 'mock-ollama-model' not found, try pulling it first"}"#,
        Duration::ZERO,
    )
    .await;
    let out = run_ollama_async(server.url.clone(), &["--deterministic-executor"]).await;
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("not found, try pulling it first"), "{t}");
    assert_eq!(field(&t, "Executions:"), "0");
    assert_eq!(server.captured.lock().await.len(), 1, "no retry");

    let out = tokio::task::spawn_blocking(|| {
        run_ollama("http://127.0.0.1:1", &["--deterministic-executor"])
    })
    .await
    .unwrap();
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("Failed"), "{t}");
    assert_eq!(field(&t, "Executions:"), "0");
}

// ---- PR33: the same experiment with the ids dealt to operations in a different order ----------

const SEED: &str = "3201";

async fn run_dealt(url: &str, run_number: usize) -> Output {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let number = run_number.to_string();
        run(
            &url,
            &[
                "--deterministic-executor",
                "--permutation-seed",
                SEED,
                "--permutation-run",
                &number,
            ],
        )
    })
    .await
    .unwrap()
}

fn capabilities_in_context(captured_body: &str) -> Vec<(String, String)> {
    let body: serde_json::Value = serde_json::from_str(captured_body).unwrap();
    let sent: String = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    let listed = sent.split("Available capabilities: ").nth(1).unwrap();
    listed
        .trim_end_matches('.')
        .split("; ")
        .map(|c| {
            let (id, description) = c.split_once(" - ").unwrap();
            (id.to_string(), description.to_string())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn dealt_runs_move_the_answer_around_and_the_same_seed_repeats() {
    const HASH: &str = "Produce a SHA-256 digest of the fixed test input.";
    let mut hash_ids = Vec::new();
    let mut hash_positions = Vec::new();
    let mut sizes = Vec::new();
    let mut op_a_right = 0;

    for run in 1..=6 {
        // Probe: whatever the model says, the run prints which capability answers the goal.
        let probe = common::start(
            200,
            &completion(REQUEST, None), // always compute.op_a
            Duration::ZERO,
        )
        .await;
        let out = run_dealt(&probe.url, run).await;
        let t = text(&out);
        let hash_id = field(&t, "hash capability:");
        let offered = field(&t, "offered:");
        sizes.push(field(&t, "Context bytes:"));
        if out.status.success() {
            op_a_right += 1;
            assert_eq!(hash_id, "compute.op_a", "run {run}");
        } else {
            assert_ne!(hash_id, "compute.op_a", "run {run}: {t}");
            assert_eq!(field(&t, "chosen:"), "compute.op_a");
            assert_eq!(
                field(&t, "digest from the Compute observation matches:"),
                "no",
                "op_a ran but is not the digest in run {run}"
            );
        }

        // What the model reads is the dealt order, each id carrying its own description.
        let listing = capabilities_in_context(&probe.captured.lock().await[0].body);
        let ids: Vec<&str> = listing.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids.join(", "), offered, "run {run}: presented order");
        let described_as_hash: Vec<&str> = listing
            .iter()
            .filter(|(_, d)| d.trim_end_matches('.') == HASH.trim_end_matches('.'))
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(described_as_hash, [hash_id.as_str()], "run {run}");
        hash_positions.push(ids.iter().position(|id| *id == hash_id).unwrap());
        hash_ids.push(hash_id.clone());

        // Answer correctly: the capability that does the digest is the one chosen, and succeeds.
        let reply = format!(r#"{{"decision":"request_capability","capability":"{hash_id}"}}"#);
        let right = common::start(200, &completion(&reply, None), Duration::ZERO).await;
        let out = run_dealt(&right.url, run).await;
        let t = text(&out);
        assert!(out.status.success(), "run {run}: {t}");
        assert_eq!(field(&t, "chosen:"), hash_id);
        assert_eq!(field(&t, "observed output:"), EXPECTED_DIGEST);
        // The same seed and run deal the same way.
        assert_eq!(field(&t, "hash capability:"), hash_id, "run {run}");
        assert_eq!(field(&t, "offered:"), offered, "run {run}");
    }

    // The deck covers every assignment once, so each id carries the digest twice in six runs and
    // always choosing op_a is right in exactly those two.
    for id in ["compute.op_a", "compute.op_b", "compute.op_c"] {
        assert_eq!(hash_ids.iter().filter(|h| *h == id).count(), 2, "{id}");
    }
    assert_eq!(op_a_right, 2);
    // The answer is not stuck in one place in the list...
    hash_positions.sort();
    hash_positions.dedup();
    assert!(hash_positions.len() > 1, "{hash_positions:?}");
    // ...and permuting changes no byte count: the same strings, dealt differently.
    sizes.dedup();
    assert_eq!(sizes.len(), 1, "{sizes:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_specified_permutation_is_refused() {
    let server = common::start(200, &completion(REQUEST, None), Duration::ZERO).await;
    let url = server.url.clone();
    let out = tokio::task::spawn_blocking(move || {
        run(
            &url,
            &["--deterministic-executor", "--permutation-seed", "3201"],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(server.captured.lock().await.is_empty(), "no model call");
}

#[test]
fn an_unconfigured_provider_is_skipped_not_failed() {
    let out = Command::new(env!("CARGO_BIN_EXE_chip"))
        .arg("--test-real-model-work")
        .env_remove("CHIP_PROVIDER")
        .env_remove("CHIP_MODEL")
        .env_remove("CHIP_ENDPOINT")
        .env_remove("CHIP_API_KEY")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(text(&out).contains("SKIPPED: real model provider unavailable"));
}

/// Opt-in: talks to the provider named by `CHIP_*` and the real Compute. Skips unless
/// `CHIP_TEST_REAL_MODEL=1` and the configuration exists; a provider that is configured but
/// fails is a failure, never a skip.
#[test]
fn real_model_escalation() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_chip"))
        .arg("--test-real-model-work")
        .output()
        .unwrap();
    let t = text(&out);
    if out.status.code() == Some(3) {
        assert!(t.contains("SKIPPED"), "{t}");
        eprintln!("SKIPPED: {}", t.trim());
        return;
    }
    assert!(out.status.success(), "{t}");

    assert_eq!(field(&t, "Outcome:"), "Completed");
    assert_eq!(field(&t, "Turns:"), "2");
    assert_eq!(field(&t, "Model calls:"), "1");
    assert_eq!(field(&t, "Model escalations:"), "1");
    assert_eq!(field(&t, "Local decisions:"), "1");
    assert_eq!(field(&t, "Executions:"), "1");
    assert_eq!(field(&t, "Observations:"), "1");
    assert_ne!(field(&t, "Receipt:"), "none", "{t}");
    // The model chose among the declared capabilities, and the answer is real Compute's,
    // compared with a digest known independently of both.
    assert_eq!(field(&t, "chosen:"), "compute.op_a");
    assert_eq!(field(&t, "observed output:"), EXPECTED_DIGEST);
    assert_eq!(
        field(&t, "digest from the Compute observation matches:"),
        "yes"
    );
    assert!(field(&t, "Context bytes:").parse::<u64>().unwrap() > 0);
    let tokens = field(&t, "Model tokens:");
    assert!(
        tokens == "not reported" || tokens.parse::<u64>().unwrap() > 0,
        "{t}"
    );
    for label in ["Model latency:", "Compute latency:"] {
        assert!(!field(&t, label).starts_with("0 "), "{label} {t}");
    }

    // The next decision happens inside the loop, after the observation, with no caller turn.
    let at = |needle: &str| {
        t.find(needle)
            .unwrap_or_else(|| panic!("no `{needle}` in:\n{t}"))
    };
    assert!(at("ModelCalled") < at("ExecutionStarted"));
    assert!(at("ExecutionStarted") < at("ObservationRecorded"));
    assert!(at("ObservationRecorded") < at("DecisionStarted (turn 2)"));
    assert!(at("DecisionStarted (turn 2)") < at("LocalDecision: complete"));
    assert!(at("LocalDecision: complete") < at("WorkCompleted"));

    // Nothing sensitive in the output.
    for var in ["CHIP_API_KEY"] {
        if let Ok(secret) = std::env::var(var) {
            assert!(!secret.is_empty() && !t.contains(&secret));
        }
    }
    assert!(!t.contains("Decide the next step"), "prompt leaked");
}

/// Opt-in, like `real_model_escalation`: the same experiment with the opaque ids dealt from a seed.
/// The model is not required to choose correctly (that is the measurement, not the assertion);
/// what must hold for every run is that the choice is validated, executed once for real, and
/// judged by the observation: the digest matches exactly when the digest capability was chosen.
#[test]
fn real_model_permuted_selection() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    let mut correct = 0;
    for run_number in 1..=5 {
        let out = Command::new(env!("CARGO_BIN_EXE_chip"))
            .arg("--test-real-model-work")
            .args(["--permutation-seed", SEED])
            .args(["--permutation-run", &run_number.to_string()])
            .output()
            .unwrap();
        let t = text(&out);
        if out.status.code() == Some(3) {
            eprintln!("SKIPPED: {}", t.trim());
            return;
        }
        let (chosen, hash) = (field(&t, "chosen:"), field(&t, "hash capability:"));
        assert_eq!(field(&t, "Model calls:"), "1", "run {run_number}: {t}");
        assert_eq!(field(&t, "Executions:"), "1", "run {run_number}: {t}");
        assert_eq!(field(&t, "Observations:"), "1", "run {run_number}: {t}");
        assert_ne!(field(&t, "Receipt:"), "none", "run {run_number}: {t}");
        let digest = field(&t, "observed output:") == EXPECTED_DIGEST;
        assert_eq!(digest, chosen == hash, "run {run_number}: {t}");
        assert_eq!(
            out.status.success(),
            chosen == hash,
            "run {run_number}: {t}"
        );
        eprintln!(
            "run {run_number}: presented [{}] digest capability {hash} chosen {chosen} {}",
            field(&t, "offered:"),
            if chosen == hash { "correct" } else { "WRONG" }
        );
        correct += usize::from(chosen == hash);
    }
    eprintln!("selected the digest capability in {correct} of 5 runs");
}

/// Opt-in: the PR35 matrix, three goal wordings by six dealings, against whichever provider is
/// configured. Nothing here requires the model to be right; that is the measurement. What must
/// hold in every cell: one model call; and either the request was rejected before execution
/// (no execution, no receipt, not Completed) or it ran once for real, and the digest matches
/// exactly when the capability that does the digest was the one requested.
#[test]
fn real_model_semantic_goal_matrix() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    let (mut selected, mut satisfied) = (0, 0);
    let mut tally = std::collections::BTreeMap::new();
    for goal in 1..=3 {
        for run_number in 1..=6 {
            let out = Command::new(env!("CARGO_BIN_EXE_chip"))
                .arg("--test-real-model-work")
                .args(["--permutation-seed", SEED])
                .args(["--permutation-run", &run_number.to_string()])
                .args(["--semantic-goal", &goal.to_string()])
                .output()
                .unwrap();
            let t = text(&out);
            if out.status.code() == Some(3) {
                eprintln!("SKIPPED: {}", t.trim());
                return;
            }
            let cell = format!("goal {goal} run {run_number}");
            let (chosen, hash) = (field(&t, "chosen:"), field(&t, "hash capability:"));
            assert_eq!(field(&t, "Model calls:"), "1", "{cell}: {t}");
            let executed = field(&t, "Executions:") == "1";
            let digest = field(&t, "observed output:") == EXPECTED_DIGEST;
            if executed {
                assert_ne!(field(&t, "Receipt:"), "none", "{cell}: {t}");
                assert_eq!(digest, chosen == hash, "{cell}: {t}");
            } else {
                assert_eq!(field(&t, "Receipt:"), "none", "{cell}: {t}");
                assert!(
                    !field(&t, "Outcome:").starts_with("Completed"),
                    "{cell}: {t}"
                );
            }
            // The profile is derived from the trajectory; it must agree with what happened.
            let category = field(&t, "category:");
            match category.as_str() {
                "A" => assert!(executed && digest && chosen == hash, "{cell}: {t}"),
                "B" => assert!(!executed && chosen == hash, "{cell}: {t}"),
                "C" => assert!(executed && !digest && chosen != hash, "{cell}: {t}"),
                "D" | "E" => assert!(!executed && chosen == "none", "{cell}: {t}"),
                "F" => assert!(!executed && chosen != hash, "{cell}: {t}"),
                other => panic!("{cell}: unexpected category {other}: {t}"),
            }
            *tally.entry(category.clone()).or_insert(0usize) += 1;
            let kind = match (chosen == hash, executed) {
                (true, true) => "right capability, goal met",
                (true, false) => "right capability, rejected before execution",
                (false, _) => "wrong capability, executed, goal unmet",
            };
            eprintln!(
                "{cell}: presented [{}] digest {hash} chose {chosen}: {kind}",
                field(&t, "offered:")
            );
            selected += usize::from(chosen == hash);
            satisfied += usize::from(executed && digest);
        }
    }
    eprintln!("selected the digest capability in {selected} of 18; goal met in {satisfied} of 18");
    eprintln!("categories: {tally:?}");
}

/// Opt-in: the PR37 matrix, six zero-overlap goals by six balanced dealings, against whichever
/// provider is configured. Nothing here requires the model to be right; that is the measurement.
/// What must hold in every cell is the boundary: a cell that did not execute has no observation,
/// no evidence and no receipt; a cell that executed has exactly one of each; and the goal is met
/// exactly when the digest capability was the one that ran.
#[test]
fn real_model_zero_overlap_matrix() {
    if std::env::var("CHIP_TEST_REAL_MODEL").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set CHIP_TEST_REAL_MODEL=1 to call a real model");
        return;
    }
    let (mut selected, mut met) = (0, 0);
    let mut tally = std::collections::BTreeMap::new();
    for goal in 1..=6 {
        for run_number in 1..=6 {
            let out = Command::new(env!("CARGO_BIN_EXE_chip"))
                .arg("--test-real-model-work")
                .arg("--balanced-dealing")
                .args(["--permutation-seed", SEED])
                .args(["--permutation-run", &run_number.to_string()])
                .args(["--zero-overlap-goal", &goal.to_string()])
                .output()
                .unwrap();
            let t = text(&out);
            if out.status.code() == Some(3) {
                eprintln!("SKIPPED: {}", t.trim());
                return;
            }
            let cell = format!("goal Z{goal} run {run_number}");
            let (chosen, hash) = (field(&t, "chosen:"), field(&t, "hash capability:"));
            let category = field(&t, "category:");
            let executed = field(&t, "Executions:") == "1";
            assert_eq!(field(&t, "Model calls:"), "1", "{cell}: {t}");
            if executed {
                assert_eq!(field(&t, "Observations:"), "1", "{cell}: {t}");
                assert_eq!(field(&t, "evidence recorded:"), "1", "{cell}: {t}");
                assert_ne!(field(&t, "Receipt:"), "none", "{cell}: {t}");
                assert_eq!(
                    field(&t, "observed output:") == EXPECTED_DIGEST,
                    chosen == hash,
                    "{cell}: {t}"
                );
            } else {
                assert_eq!(field(&t, "Observations:"), "0", "{cell}: {t}");
                assert_eq!(field(&t, "evidence recorded:"), "0", "{cell}: {t}");
                assert_eq!(field(&t, "Receipt:"), "none", "{cell}: {t}");
            }
            match category.as_str() {
                "A" => assert!(executed && chosen == hash, "{cell}: {t}"),
                "B" => assert!(!executed && chosen == hash, "{cell}: {t}"),
                "C" => assert!(executed && chosen != hash, "{cell}: {t}"),
                "D" | "E" | "G" => assert!(!executed && chosen == "none", "{cell}: {t}"),
                "F" => assert!(!executed && chosen != hash, "{cell}: {t}"),
                other => panic!("{cell}: unexpected category {other}: {t}"),
            }
            *tally.entry(category.clone()).or_insert(0usize) += 1;
            selected += usize::from(chosen == hash);
            met += usize::from(category == "A");
            eprintln!(
                "{cell}: presented [{}] digest {hash} chose {chosen}: category {category}",
                field(&t, "offered:")
            );
        }
    }
    eprintln!("selected the digest capability in {selected} of 36; goal met in {met} of 36");
    eprintln!("categories: {tally:?}");
}
